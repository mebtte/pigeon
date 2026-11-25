use std::{collections::HashSet, fs, io::Write, net::SocketAddr, path::PathBuf, time::Duration};

use axum::{
    Extension,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::store::{NewShare, is_share_code};

use super::*;

const FILE_BYTES: &[u8] = b"pigeon testing";
const MULTIPART_TYPE: &str = "multipart/form-data; boundary=pigeon";

struct TestData(PathBuf);

impl TestData {
    fn new() -> Self {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let name = format!("pigeon-http-{:016x}", u64::from_ne_bytes(random));
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn unprotected_share(&self) -> String {
        let store = Store::open(&self.0).unwrap();
        let mut upload = store.start_upload().unwrap();
        upload.take_file().write_all(FILE_BYTES).unwrap();
        store
            .finish_upload(
                upload,
                NewShare {
                    filename: "test.txt".to_owned(),
                    size_bytes: FILE_BYTES.len() as u64,
                    uploaded_by: "test".to_owned(),
                    expires_in: None,
                    download_limit: None,
                    password: None,
                },
            )
            .unwrap()
            .code
    }

    fn router(&self, prefix: &str) -> Router {
        self.router_with_limit(prefix, None)
    }

    fn router_with_limit(&self, prefix: &str, max_file_size: Option<usize>) -> Router {
        router(Arc::new(
            App::new(
                prefix.parse().unwrap(),
                HashMap::from([
                    ("test".to_owned(), "password".to_owned()),
                    ("other".to_owned(), "other-password".to_owned()),
                ]),
                Store::open(&self.0).unwrap(),
                max_file_size,
            )
            .unwrap(),
        ))
    }
}

impl Drop for TestData {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn applies_exact_root_paths_and_requires_session() {
    let data = TestData::new();
    let app = data.router("/");
    for path in ["/", "/robots.txt"] {
        assert_eq!(
            call(&app, "GET", path, &[], Body::empty()).await.status(),
            StatusCode::OK
        );
    }
    for path in [
        "/missing",
        "/robots.txt/",
        "/sharing",
        "/sharing?id=bad",
        "/sharing?download=bad",
        "/api/missing",
        "/api/storage",
    ] {
        assert_eq!(
            call(&app, "GET", path, &[], Body::empty()).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let response = call(
        &app,
        "GET",
        "/api/config",
        &[("Authorization", "Basic example")],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn serves_one_html_shell_with_separate_public_share_metadata() {
    let data = TestData::new();
    let id = data.unprotected_share();
    let app = data.router("/pigeon");

    let home = call(&app, "GET", "/pigeon/", &[], Body::empty()).await;
    assert_eq!(home.status(), StatusCode::OK);
    let html = to_bytes(home.into_body(), usize::MAX).await.unwrap();

    let legacy_home = call(&app, "GET", "/pigeon", &[], Body::empty()).await;
    assert_eq!(legacy_home.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(legacy_home.into_body(), usize::MAX).await.unwrap(),
        html
    );

    let (_, meta) = std::str::from_utf8(&html)
        .unwrap()
        .split_once("name=\"pigeon-bootstrap\" content=\"")
        .unwrap();
    let (encoded, _) = meta.split_once('"').unwrap();
    let bootstrap: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
    assert_eq!(bootstrap, json!({"base_path": "/pigeon"}));

    let metadata_path = format!("/pigeon/api/shares/{id}");
    let metadata = call(&app, "GET", &metadata_path, &[], Body::empty()).await;
    assert_eq!(metadata.status(), StatusCode::OK);
    assert_eq!(
        read_json(metadata).await,
        json!({"id": id, "requires_password": false, "filename": "test.txt", "size_bytes": FILE_BYTES.len()})
    );
    let sharing_path = format!("/pigeon/sharing?id={id}");
    assert_eq!(
        call(&app, "GET", &sharing_path, &[], Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn config_under_base_path_reports_and_enforces_the_upload_size_limit() {
    let data = TestData::new();
    let app = data.router_with_limit("/pigeon", Some(FILE_BYTES.len()));
    let config_path = "/pigeon/api/config";
    assert_eq!(
        call(&app, "GET", config_path, &[], Body::empty())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let session = post_json(
        &app,
        "/pigeon/api/sessions",
        json!({"username": "test", "password": "password"}),
    )
    .await;
    assert_eq!(session.status(), StatusCode::OK);
    let session = read_json(session).await;
    let token = session["token"].as_str().unwrap();
    let auth = [("X-Pigeon-Token", token)];
    let config = call(&app, "GET", config_path, &auth, Body::empty()).await;
    assert_eq!(config.status(), StatusCode::OK);
    let config = read_json(config).await;
    assert!(config["available_bytes"].is_u64());
    assert_eq!(config["max_file_size_bytes"], FILE_BYTES.len());
    for path in ["/api/config", "/pigeon/api/storage"] {
        assert_eq!(
            call(&app, "GET", path, &auth, Body::empty()).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    let upload_headers = [("Content-Type", MULTIPART_TYPE), auth[0]];
    let upload = |bytes: &[u8]| {
        let mut payload = multipart_file_field(bytes);
        payload.extend(b"\r\n--pigeon--\r\n");
        call(
            &app,
            "POST",
            "/pigeon/api/shares",
            &upload_headers,
            Body::from(payload),
        )
    };
    let accepted = upload(FILE_BYTES).await;
    assert_eq!(accepted.status(), StatusCode::CREATED);
    let oversized = [FILE_BYTES, b"!"].concat();
    let rejection = upload(&oversized).await;
    assert_eq!(rejection.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(read_json(rejection).await["error"], "file_too_large");
}

#[tokio::test]
async fn default_config_allows_uploads_over_axums_default_body_limit() {
    let data = TestData::new();
    let app = data.router("/");
    let session = post_json(
        &app,
        "/api/sessions",
        json!({"username": "test", "password": "password"}),
    )
    .await;
    assert_eq!(session.status(), StatusCode::OK);
    let session = read_json(session).await;
    let token = session["token"].as_str().unwrap();
    let config = call(
        &app,
        "GET",
        "/api/config",
        &[("X-Pigeon-Token", token)],
        Body::empty(),
    )
    .await;
    assert_eq!(config.status(), StatusCode::OK);
    assert_eq!(
        read_json(config).await.get("max_file_size_bytes"),
        Some(&Value::Null)
    );

    let file = vec![b'x'; 2 * 1024 * 1024 + 1];
    let mut payload = multipart_file_field(&file);
    payload.extend(b"\r\n--pigeon--\r\n");
    let response = call(
        &app,
        "POST",
        "/api/shares",
        &[("Content-Type", MULTIPART_TYPE), ("X-Pigeon-Token", token)],
        Body::from(payload),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn login_cooldown_follows_the_username_across_sources() {
    let data = TestData::new();
    let app = data.router("/");
    let first_source = app.clone().layer(Extension(ConnectInfo(
        "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
    )));
    let second_source = app.layer(Extension(ConnectInfo(
        "127.0.0.2:40000".parse::<SocketAddr>().unwrap(),
    )));
    let sessions = "/api/sessions";

    for index in 0..31 {
        let response = post_json(
            &first_source,
            sessions,
            json!({"username": format!("unknown-{index}"), "password": "wrong"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "name {index}");
        assert_eq!(read_json(response).await["error"], "invalid_credentials");
    }

    let failed = post_json(
        &first_source,
        sessions,
        json!({"username": "test", "password": "wrong"}),
    )
    .await;
    assert_eq!(failed.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(read_json(failed).await["error"], "invalid_credentials");

    let retry = post_json(
        &second_source,
        sessions,
        json!({"username": "test", "password": "password"}),
    )
    .await;
    assert_eq!(retry.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(retry.headers().contains_key("retry-after"));
    assert_eq!(read_json(retry).await["error"], "password_cooldown");

    let other = post_json(
        &first_source,
        sessions,
        json!({"username": "other", "password": "other-password"}),
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    assert!(read_json(other).await["token"].is_string());
}

#[tokio::test]
async fn anonymous_download_survives_last_extraction_and_same_data_restart() {
    let data = TestData::new();
    let app = data.router("/pigeon");
    let session = post_json(
        &app,
        "/pigeon/api/sessions",
        json!({"username": "test", "password": "password"}),
    )
    .await;
    assert_eq!(session.status(), StatusCode::OK);
    let session = read_json(session).await;
    let token = session["token"].as_str().unwrap();

    let login_retry = post_json(
        &app,
        "/pigeon/api/sessions",
        json!({"username": "test", "password": "wrong"}),
    )
    .await;
    assert_eq!(login_retry.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(login_retry.headers().contains_key("retry-after"));
    assert_eq!(read_json(login_retry).await["error"], "password_cooldown");

    let mut payload = multipart_file_field(FILE_BYTES);
    payload.extend(b"\r\n--pigeon\r\nContent-Disposition: form-data; name=\"max_downloads\"\r\n\r\n1\r\n--pigeon\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\nP1GEON\r\n--pigeon--\r\n");
    let upload = call(
        &app,
        "POST",
        "/pigeon/api/shares",
        &[("Content-Type", MULTIPART_TYPE), ("X-Pigeon-Token", token)],
        Body::from(payload),
    )
    .await;
    assert_eq!(upload.status(), StatusCode::CREATED);
    let upload = read_json(upload).await;
    assert!(upload["expires_at"].is_null());
    let share = upload["share_url"].as_str().unwrap();
    let id = share.strip_prefix("/pigeon/#/s/").unwrap();
    assert!(is_share_code(id));

    let metadata_path = format!("/pigeon/api/shares/{id}");
    let extract = format!("{metadata_path}/download-links");
    let metadata = call(&app, "GET", &metadata_path, &[], Body::empty()).await;
    assert_eq!(metadata.status(), StatusCode::OK);
    assert_eq!(
        read_json(metadata).await,
        json!({"id": id, "requires_password": true})
    );

    let wrong = post_json(&app, &extract, json!({"password": "wrong"})).await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    assert_eq!(read_json(wrong).await, json!({"error": "invalid_password"}));

    let retry = post_json(&app, &extract, json!({"password": "P1GEON"})).await;
    assert_eq!(retry.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(matches!(
        retry.headers()["retry-after"].to_str().unwrap(),
        "1" | "2" | "3"
    ));
    assert_eq!(read_json(retry).await["error"], "password_cooldown");

    tokio::time::sleep(Duration::from_secs(3)).await;
    let direct = post_json(&app, &extract, json!({"password": "P1GEON"})).await;
    assert_eq!(direct.status(), StatusCode::CREATED);
    let direct = read_json(direct).await;
    assert_eq!(direct["filename"], "test.txt");
    assert_eq!(direct["size_bytes"], FILE_BYTES.len());
    let direct = direct["download_url"].as_str().unwrap();
    assert!(direct.starts_with("/pigeon/sharing?download="));
    let direct_with_password = format!("{direct}&password=P1GEON");
    assert_eq!(
        call(&app, "GET", &direct_with_password, &[], Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "GET", &metadata_path, &[], Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let logout = call(
        &app,
        "DELETE",
        "/pigeon/api/sessions",
        &[("X-Pigeon-Token", token)],
        Body::empty(),
    )
    .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let after_logout = call(
        &app,
        "GET",
        "/pigeon/api/config",
        &[("X-Pigeon-Token", token)],
        Body::empty(),
    )
    .await;
    assert_eq!(after_logout.status(), StatusCode::UNAUTHORIZED);
    let fresh = post_json(
        &app,
        "/pigeon/api/sessions",
        json!({"username": "test", "password": "password"}),
    )
    .await;
    let fresh = read_json(fresh).await;
    let token = fresh["token"].as_str().unwrap();

    // A new application instance must use the persisted direct link, but a
    // session created before restart belongs to the previous process.
    drop(app);
    let app = data.router("/pigeon");
    let response = call(
        &app,
        "GET",
        "/pigeon/api/config",
        &[("X-Pigeon-Token", token)],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = call(
        &app,
        "GET",
        direct,
        &[("Range", "bytes=2-6")],
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 2-6/14");
    assert_eq!(
        to_bytes(response.into_body(), 100).await.unwrap(),
        &FILE_BYTES[2..=6]
    );
    let response = call(&app, "HEAD", direct, &[], Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "14");
}

#[tokio::test]
async fn unprotected_share_can_issue_more_than_sixty_links_to_one_client() {
    let data = TestData::new();
    let id = data.unprotected_share();
    let app = data.router("/");
    let extract = format!("/api/shares/{id}/download-links");
    let mut urls = HashSet::new();
    for index in 1..=61 {
        let response = post_json(&app, &extract, json!({})).await;
        assert_eq!(response.status(), StatusCode::CREATED, "link {index}");
        let direct = read_json(response).await;
        assert!(urls.insert(direct["download_url"].as_str().unwrap().to_owned()));
    }
}

#[tokio::test]
async fn download_bodies_remain_open_through_the_sixty_fifth_response() {
    let data = TestData::new();
    let id = data.unprotected_share();
    let app = data.router("/");
    let extract = format!("/api/shares/{id}/download-links");
    let direct = post_json(&app, &extract, json!({})).await;
    assert_eq!(direct.status(), StatusCode::CREATED);
    let direct = read_json(direct).await;
    let url = direct["download_url"].as_str().unwrap();

    // Keep each response body alive until all download requests have received headers.
    let mut bodies = Vec::new();
    for index in 1..=65 {
        let response = call(&app, "GET", url, &[], Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK, "download {index}");
        bodies.push(response.into_body());
    }
    for body in bodies {
        assert_eq!(to_bytes(body, FILE_BYTES.len()).await.unwrap(), FILE_BYTES);
    }
}

#[tokio::test]
async fn five_uploads_can_wait_for_their_request_bodies_at_the_same_time() {
    let data = TestData::new();
    let app = data.router("/");
    let session = post_json(
        &app,
        "/api/sessions",
        json!({"username": "test", "password": "password"}),
    )
    .await;
    assert_eq!(session.status(), StatusCode::OK);
    let session = read_json(session).await;
    let headers = [
        ("Content-Type", MULTIPART_TYPE),
        ("X-Pigeon-Token", session["token"].as_str().unwrap()),
    ];
    let mut payload = multipart_file_field(FILE_BYTES);
    payload.extend(b"\r\n--pigeon--\r\n");

    let mut requests = Vec::new();
    let mut senders = Vec::new();
    for _ in 0..5 {
        let (sender, receiver) = tokio::sync::oneshot::channel::<Vec<u8>>();
        let body = Body::from_stream(futures_util::stream::once(receiver));
        let mut request = Box::pin(call(&app, "POST", "/api/shares", &headers, body));
        // The handler must wait for its body instead of rejecting an overlapping upload.
        assert!(futures_util::poll!(request.as_mut()).is_pending());
        requests.push(request);
        senders.push(sender);
    }
    for sender in senders {
        sender.send(payload.clone()).unwrap();
    }
    for request in requests {
        assert_eq!(request.await.status(), StatusCode::CREATED);
    }
}

fn multipart_file_field(bytes: &[u8]) -> Vec<u8> {
    let mut payload =
        b"--pigeon\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\n"
            .to_vec();
    payload.extend(bytes);
    payload
}

async fn post_json(app: &Router, path: &str, value: Value) -> Response {
    call(
        app,
        "POST",
        path,
        &[("Content-Type", "application/json")],
        Body::from(value.to_string()),
    )
    .await
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn read_json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap()
}
