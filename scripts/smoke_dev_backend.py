"""Verify backend watch restarts and privacy-preserving development logs."""

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, Request, build_opener

ROOT = Path(__file__).resolve().parent.parent
HTTP = build_opener(ProxyHandler({}))


def wait_for_page(process, url, expected, log):
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline and process.poll() is None:
        try:
            with HTTP.open(url, timeout=0.5) as response:
                if expected in response.read():
                    return
        except (URLError, TimeoutError, OSError):
            pass
        time.sleep(0.15)
    raise AssertionError(f"Expected {expected!r} at {url}\n{log.read_text()}")


def expect_not_found(request):
    try:
        with HTTP.open(request, timeout=2) as response:
            raise AssertionError(f"Expected HTTP 404, got {response.status}")
    except HTTPError as error:
        with error:
            assert error.code == 404, error.code


def check_request_logs(origin, log, username, password, token):
    share_id = "q7_K2V"
    query_value = "pigeon-private-query-value"
    authorization = "pigeon-private-authorization-value"
    extract = Request(
        f"{origin}/api/shares/{share_id}/download-links?private={query_value}",
        data=b"{}",
        headers={
            "Content-Type": "application/json",
            "X-Pigeon-Token": token,
            "Authorization": f"Bearer {authorization}",
        },
    )
    expect_not_found(extract)
    unmatched_path = "pigeon-private-unmatched-path"
    expect_not_found(f"{origin}/{unmatched_path}")

    output = log.read_text()
    for name, value in (
        ("username", username),
        ("password", password),
        ("session token", token),
        ("share ID", share_id),
        ("query value", query_value),
        ("Authorization value", authorization),
        ("unmatched path", unmatched_path),
    ):
        assert value not in output, f"Development logs exposed the {name}"

    # Match all fields on one event without depending on formatter field order.
    for method, route in (
        ("POST", "/pigeon/api/shares/{id}/download-links"),
        ("GET", "<unmatched>"),
    ):
        fields = (
            r"\bDEBUG\b",
            r"\bHTTP request handled\b",
            rf"(?<![\w])method={method}(?=\s|}}|$)",
            rf"(?<![\w])route={re.escape(route)}(?=\s|}}|$)",
            r"(?:^|\s)status=404(?:\s|$)",
            r"(?:^|\s)elapsed_ms=\d+(?:\.\d+)?(?:\s|$)",
        )
        assert any(
            all(re.search(field, line) for field in fields) for line in output.splitlines()
        ), f"Expected a DEBUG event for {method} {route} with status and timing"


def main():
    with tempfile.TemporaryDirectory(prefix="pigeon-watch-") as directory:
        scratch = Path(directory)
        project = scratch / "project with spaces"
        backend = project / "backend"
        scripts = project / "scripts"
        frontend = project / "frontend"
        scripts.mkdir(parents=True)
        frontend.mkdir()
        shutil.copytree(ROOT / "backend/src", backend / "src")
        for name in ("Cargo.toml", "Cargo.lock", "build.rs"):
            shutil.copy2(ROOT / "backend" / name, backend / name)
        watcher = scripts / "dev-backend.sh"
        shutil.copy2(ROOT / "scripts/dev-backend.sh", watcher)

        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]

        log = scratch / "watch.log"
        origin = f"http://127.0.0.1:{port}/pigeon"
        username = "pigeon-private-smoke-user"
        password = "local test $password"
        environment = os.environ.copy()
        environment.pop("RUST_LOG", None)
        environment["NO_COLOR"] = "1"
        with log.open("w") as output:
            process = subprocess.Popen(
                [str(watcher), "--port", str(port), "--base-path", "/pigeon",
                 "--data", "developer data", "--user", f"{username}:{password}"],
                cwd=scratch,
                stdout=output,
                stderr=output,
                env=environment,
            )
        try:
            robots = origin + "/robots.txt"
            wait_for_page(process, robots, b"Disallow: /\n", log)
            assert (scratch / "developer data" / "pigeon.sqlite3").is_file()
            credentials = json.dumps({"username": username, "password": password}).encode()
            login = Request(origin + "/api/sessions", data=credentials,
                            headers={"Content-Type": "application/json"})
            with HTTP.open(login, timeout=2) as response:
                token = json.load(response)["token"]
                assert token
            check_request_logs(origin, log, username, password, token)

            # The frontend build has its own watcher. It must not reset Rust sessions.
            (frontend / "index.html").write_text("<title>Changed frontend</title>")
            time.sleep(1)
            config_request = Request(origin + "/api/config", headers={"X-Pigeon-Token": token})
            with HTTP.open(config_request, timeout=2) as response:
                config = json.load(response)
            assert config["available_bytes"] >= 0
            assert config["max_file_size_bytes"] is None
            assert log.read_text().count("Pigeon listening") == 1

            source = backend / "src/http/pages.rs"
            original = source.read_text()
            for index in (1, 2):
                marker = f"/watch-test-{index}"
                # Watchexec polling compares modification times at one-second resolution.
                next_second = source.stat().st_mtime_ns // 1_000_000_000 + 1
                time.sleep(max(0.0, next_second - time.time()))
                # Editors can replace the file instead of modifying its inode.
                replacement = source.with_suffix(".tmp")
                replacement.write_text(original.replace("Disallow: /", f"Disallow: {marker}"))
                replacement.replace(source)
                wait_for_page(process, robots, marker.encode(), log)
            assert "Native fs watcher error" not in log.read_text()
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=15)

        with socket.socket() as probe:
            probe.settimeout(1)
            assert probe.connect_ex(("127.0.0.1", port)) != 0, "Backend kept running after Ctrl+C"

    print("Backend watcher smoke test passed")


if __name__ == "__main__":
    main()
