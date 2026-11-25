"""Exercise the standalone release or a container running that release."""

import argparse
from contextlib import contextmanager
from html.parser import HTMLParser
from io import BytesIO
from pathlib import Path
import json
import re
import shutil
import socket
import subprocess
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, urlsplit
from urllib.request import ProxyHandler, Request, build_opener
from uuid import uuid4
from zipfile import ZipFile

BASE = "/pigeon"
USERNAME = "smoke"
PASSWORD = "smoke-password"
SHARE_PASSWORD = "P1GE0N42"
CLI_ACCOUNTS = (("smoke-cli", "cli:password"), ("smoke-zip", "zip-password"))
PAYLOAD = b"hello pigeon\n"
OPENER = build_opener(ProxyHandler({}))


class FrontendPage(HTMLParser):
    def __init__(self):
        super().__init__()
        self.module_sources = []

    def handle_starttag(self, tag, attributes):
        fields = dict(attributes)
        if tag == "script" and fields.get("type") == "module" and fields.get("src"):
            self.module_sources.append(fields["src"])


def request(origin, path, *, method="GET", headers=None, body=None, status=200):
    req = Request(origin + path, method=method, headers=headers or {}, data=body)
    try:
        response = OPENER.open(req, timeout=5)
    except HTTPError as error:
        response = error
    with response:
        data = response.read()
        if response.status != status:
            raise AssertionError(
                f"{method} {path}: expected {status}, got {response.status}: {data[:250]!r}"
            )
        return response.headers, data


def post_json(origin, path, values, *, status=200):
    _, body = request(
        origin,
        path,
        method="POST",
        headers={"Content-Type": "application/json"},
        body=json.dumps(values).encode(),
        status=status,
    )
    return json.loads(body)


def check_embedded_page(origin, path):
    headers, body = request(origin, path)
    page = FrontendPage()
    page.feed(body.decode())
    assert headers.get_content_type() == "text/html", path
    assert page.module_sources, f"{path} lacks the embedded React entry point"
    for source in page.module_sources:
        parsed = urlsplit(source)
        assert not (parsed.scheme or parsed.netloc or parsed.query or parsed.fragment), source
        assert parsed.path.startswith(f"{BASE}/assets/") and parsed.path.endswith(".js"), source
        headers, javascript = request(origin, source)
        assert headers.get_content_type() == "text/javascript", source
        assert javascript, f"{source} is empty"


def upload_file(origin, token, *, filename="smoke.txt", fields=None):
    boundary = uuid4().hex
    if fields is None:
        fields = {"password": SHARE_PASSWORD, "expiry_days": "7", "max_downloads": "1"}
    parts = [
        f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
        for name, value in fields.items()
    ]
    parts.extend(
        [
            f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\n'.encode(),
            b"Content-Type: text/plain\r\n\r\n",
            PAYLOAD,
            f"\r\n--{boundary}--\r\n".encode(),
        ]
    )
    _, uploaded = request(
        origin,
        f"{BASE}/api/shares",
        method="POST",
        headers={
            "Content-Type": f"multipart/form-data; boundary={boundary}",
            "X-Pigeon-Token": token,
        },
        body=b"".join(parts),
        status=201,
    )
    return json.loads(uploaded)["share_url"]


def share_id_from_url(url):
    parsed = urlsplit(url)
    assert not (parsed.scheme or parsed.netloc or parsed.query), url
    assert parsed.path == f"{BASE}/", url
    match = re.fullmatch(r"/s/([A-Za-z0-9_-]{6})", parsed.fragment)
    assert match, url
    return match.group(1)


def check_download_url(url):
    parsed = urlsplit(url)
    assert not (parsed.scheme or parsed.netloc or parsed.fragment), url
    assert parsed.path == f"{BASE}/sharing", url
    query = parse_qs(parsed.query, strict_parsing=True)
    assert set(query) == {"download"} and len(query["download"]) == 1, url


def check_release(origin):
    check_embedded_page(origin, f"{BASE}/")
    request(origin, f"{BASE}/unknown", status=404)
    _, robots = request(origin, f"{BASE}/robots.txt")
    assert b"Disallow: /" in robots, robots

    session = post_json(
        origin, f"{BASE}/api/sessions", {"username": USERNAME, "password": PASSWORD}
    )
    token = session["token"]
    _, config_payload = request(
        origin, f"{BASE}/api/config", headers={"X-Pigeon-Token": token}
    )
    config = json.loads(config_payload)
    assert config["available_bytes"] >= 0
    assert config["max_file_size_bytes"] is None

    share = upload_file(origin, token)
    share_id = share_id_from_url(share)
    metadata_path = f"{BASE}/api/shares/{share_id}"
    _, metadata = request(origin, metadata_path)
    assert json.loads(metadata) == {"id": share_id, "requires_password": True}, metadata

    request(origin, f"{BASE}/sharing?id={share_id}", status=404)

    extraction = post_json(
        origin,
        f"{metadata_path}/download-links",
        {"password": SHARE_PASSWORD},
        status=201,
    )

    # The browser fragment selects the share while its HTML shell remains available.
    request(origin, metadata_path, status=404)
    headers, _ = request(origin, urlsplit(share).path)
    assert headers.get_content_type() == "text/html", share
    download_url = extraction["download_url"]
    check_download_url(download_url)
    headers, data = request(
        origin, download_url, headers={"Range": "bytes=0-4"}, status=206
    )
    assert headers.get("Content-Range") == f"bytes 0-4/{len(PAYLOAD)}"
    assert data == PAYLOAD[:5], data
    _, data = request(origin, download_url)
    assert data == PAYLOAD, data


def check_share_cli(binary, origin):
    def run_share(account, inputs, answers, *, trailing_slash=False):
        endpoint = f"{origin}{BASE}" + ("/" if trailing_slash else "")
        process = subprocess.run(
            [str(binary), "share", "--endpoint", endpoint, "--user", ":".join(account), "--", *inputs],
            input=answers,
            text=True,
            encoding="utf-8",
            capture_output=True,
            cwd=root,
            timeout=30,
        )
        assert process.returncode == 0, process.stderr
        links = process.stdout.splitlines()
        if links:
            assert re.search(
                r"^Upload progress: 100%.*B/s", process.stderr, re.MULTILINE
            ), process.stderr
        else:
            assert "Upload progress:" not in process.stderr, process.stderr
        return links

    def get_share_id(url):
        assert url.startswith(f"{origin}{BASE}/#/s/"), url
        return share_id_from_url(url.removeprefix(origin))

    with tempfile.TemporaryDirectory(prefix="pigeon-share-") as directory:
        root = Path(directory)
        filename = "-测试.txt"
        (root / filename).write_bytes(PAYLOAD)
        (root / "folder").mkdir()
        (root / "folder" / "nested.txt").write_bytes(b"zip payload")

        assert run_share(CLI_ACCOUNTS[0], [filename], "\n\n\nn\n") == []
        (url,) = run_share(CLI_ACCOUNTS[0], [filename], "\n\n\n\n")
        share_id = get_share_id(url)
        path = f"{BASE}/api/shares/{share_id}"
        _, body = request(origin, path)
        assert json.loads(body) == {
            "id": share_id,
            "requires_password": False,
            "filename": filename,
            "size_bytes": len(PAYLOAD),
        }, body
        download = post_json(origin, f"{path}/download-links", {}, status=201)
        assert request(origin, download["download_url"])[1] == PAYLOAD

        url, url_with_password = run_share(
            CLI_ACCOUNTS[1],
            [filename, "folder"],
            f"forever\n1\n{SHARE_PASSWORD}\ny\n",
            trailing_slash=True,
        )
        assert url_with_password == f"{url}?p={SHARE_PASSWORD}", url_with_password
        path = f"{BASE}/api/shares/{get_share_id(url)}"
        extraction = post_json(
            origin, f"{path}/download-links", {"password": SHARE_PASSWORD}, status=201
        )
        assert extraction["filename"] == "files.zip", extraction
        request(origin, path, status=404)
        _, archive = request(origin, extraction["download_url"])
        with ZipFile(BytesIO(archive)) as zipped:
            assert zipped.read(filename) == PAYLOAD
            assert zipped.read("folder/nested.txt") == b"zip payload"


def wait_for_server(origin, process=None):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError(f"Pigeon exited with status {process.returncode}")
        try:
            request(origin, f"{BASE}/robots.txt")
        except (URLError, TimeoutError):
            time.sleep(0.1)
        else:
            return
    raise TimeoutError(f"Pigeon did not listen on {origin}")


@contextmanager
def start_standalone(binary, *, accounts=((USERNAME, PASSWORD),)):
    with tempfile.TemporaryDirectory(prefix="pigeon-release-") as directory:
        # Only the shipping executable goes into the scratch directory.
        root = Path(directory)
        executable = root / binary.name
        shutil.copy2(binary, executable)
        with socket.socket() as reserving:
            reserving.bind(("127.0.0.1", 0))
            port = reserving.getsockname()[1]
        arguments = [
            str(executable),
            "serve",
            "--port",
            str(port),
            "--data",
            str(root / "data"),
            "--base-path",
            BASE,
        ]
        for username, password in accounts:
            arguments.extend(("--user", f"{username}:{password}"))
        process = subprocess.Popen(arguments, cwd=root)
        try:
            origin = f"http://127.0.0.1:{port}"
            wait_for_server(origin, process)
            yield origin
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument("binary", nargs="?", type=Path)
    target.add_argument("--url", help="Origin of a running smoke-test container")
    args = parser.parse_args()

    if args.url:
        origin = args.url.rstrip("/")
        wait_for_server(origin)
        check_release(origin)
    else:
        binary = args.binary.resolve(strict=True)
        with start_standalone(binary, accounts=((USERNAME, PASSWORD), *CLI_ACCOUNTS)) as origin:
            check_release(origin)
            check_share_cli(binary, origin)
    print("Release smoke test passed")


if __name__ == "__main__":
    main()
