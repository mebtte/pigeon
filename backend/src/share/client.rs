use std::{error::Error, fs::File, time::Duration};

use reqwest::{
    Body, RequestBuilder, Response, StatusCode, Url,
    header::{HeaderValue, RETRY_AFTER},
    multipart::{Form, Part},
    redirect, retry,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use super::ShareOptions;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const API_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_JSON_BYTES: usize = 16 * 1024;

pub(super) struct Client {
    http: reqwest::Client,
    endpoint: Url,
}

impl Client {
    pub(super) fn new(endpoint: &str) -> Result<Self> {
        let endpoint = parse_endpoint(endpoint)?;
        // Keep credentials on the configured origin and avoid replaying uploads.
        let http = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .retry(retry::never())
            .connect_timeout(API_TIMEOUT)
            .build()
            .map_err(reqwest::Error::without_url)?;
        Ok(Self { http, endpoint })
    }

    pub(super) async fn login(&self, username: &str, password: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Session {
            token: String,
        }

        let request = self
            .http
            .post(api_url(&self.endpoint, "sessions"))
            .json(&json!({ "username": username, "password": password }));
        let response = send(request, StatusCode::OK, "Login").await?;
        let session: Session = read_json(response)
            .await
            .map_err(|error| format!("Login: {error}"))?;
        Ok(session.token)
    }

    pub(super) async fn max_file_size(&self, token: &str) -> Result<Option<u64>> {
        #[derive(Deserialize)]
        struct Config {
            max_file_size_bytes: Option<u64>,
        }

        let request = self
            .http
            .get(api_url(&self.endpoint, "config"))
            .header("X-Pigeon-Token", token_header(token)?);
        let response = send(request, StatusCode::OK, "Read upload limit").await?;
        let config: Config = read_json(response)
            .await
            .map_err(|error| format!("Read upload limit: {error}"))?;
        Ok(config.max_file_size_bytes)
    }

    pub(super) async fn upload(
        &self,
        token: &str,
        file: File,
        filename: &str,
        size: u64,
        options: &ShareOptions,
    ) -> Result<Url> {
        #[derive(Deserialize)]
        struct Share {
            share_url: String,
        }

        if filename.is_empty()
            || filename.len() > 255
            || filename.contains(['/', '\\'])
            || filename.chars().any(char::is_control)
        {
            return Err("Invalid upload filename".into());
        }
        let mut form = Form::new();
        if let Some(days) = options.expiry_days {
            form = form.text("expiry_days", days.to_string());
        }
        if let Some(downloads) = options.max_downloads {
            form = form.text("max_downloads", downloads.to_string());
        }
        if let Some(password) = &options.password {
            form = form.text("password", password.clone());
        }
        let file = tokio::fs::File::from_std(file).take(size);
        let body = Body::wrap_stream(ReaderStream::new(file));
        let part = Part::stream_with_length(body, size)
            .file_name(filename.to_owned())
            .mime_str("application/octet-stream")?;
        // The length lets proxies enforce their upload limits without buffering the whole file.
        let response = self
            .http
            .post(api_url(&self.endpoint, "shares"))
            .header("X-Pigeon-Token", token_header(token)?)
            .multipart(form.part("file", part))
            .send()
            .await
            .map_err(|error| {
                let reason = request_error("Upload", &error);
                if error.is_connect() || error.is_builder() {
                    reason.into()
                } else {
                    unknown_upload(&reason)
                }
            })?;

        let status = response.status();
        if status != StatusCode::CREATED {
            let reason = response_error(response, "Upload").await;
            return if status.is_success()
                || (status.is_server_error() && status != StatusCode::INSUFFICIENT_STORAGE)
            {
                Err(unknown_upload(&reason))
            } else {
                Err(reason.into())
            };
        }
        let share: Share = read_json(response).await.map_err(unknown_upload)?;
        share_url(&self.endpoint, &share.share_url)
            .ok_or_else(|| unknown_upload("Pigeon returned an invalid share URL"))
    }

    pub(super) async fn logout(&self, token: &str) -> Result<()> {
        let request = self
            .http
            .delete(api_url(&self.endpoint, "sessions"))
            .header("X-Pigeon-Token", token_header(token)?);
        send(request, StatusCode::NO_CONTENT, "Logout").await?;
        Ok(())
    }
}

fn parse_endpoint(value: &str) -> Result<Url> {
    let invalid =
        "--endpoint must be an absolute HTTP(S) URL without credentials, query, or fragment";
    let (_, rest) = value.split_once("://").ok_or(invalid)?;
    let authority = rest.split('/').next().unwrap_or_default();
    let url = Url::parse(value).map_err(|_| invalid)?;
    if authority.is_empty()
        || authority.contains('@')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid.into());
    }
    Ok(url)
}

fn api_url(endpoint: &Url, route: &str) -> Url {
    let mut url = endpoint.clone();
    url.path_segments_mut()
        .expect("HTTP(S) URL with a host")
        .pop_if_empty()
        .extend(["api", route]);
    url
}

fn share_url(endpoint: &Url, value: &str) -> Option<Url> {
    if !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return None;
    }
    let url = endpoint.join(value).ok()?;
    let id = url.fragment()?.strip_prefix("/sharing/")?;
    (url.origin() == endpoint.origin()
        && url.query().is_none()
        && id.len() == 32
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(url)
}

pub(super) fn link_with_password(link: &Url, password: &str) -> Url {
    let mut url = link.clone();
    // Match the web app's fragment format so HTTP requests omit the password.
    if let Some(route) = link.fragment() {
        url.set_fragment(Some(&format!("{route}?password={password}")));
    }
    url
}

fn token_header(token: &str) -> Result<HeaderValue> {
    let mut value =
        HeaderValue::from_str(token).map_err(|_| "Pigeon returned an invalid session token")?;
    value.set_sensitive(true);
    Ok(value)
}

async fn send(request: RequestBuilder, expected: StatusCode, action: &str) -> Result<Response> {
    let response = request
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|error| request_error(action, &error))?;
    if response.status() == expected {
        Ok(response)
    } else {
        Err(response_error(response, action).await.into())
    }
}

fn request_error(action: &str, error: &reqwest::Error) -> String {
    let detail = if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "could not connect to the server"
    } else {
        "HTTP request failed"
    };
    format!("{action}: {detail}")
}

async fn response_error(response: Response, action: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorResponse {
        error: String,
    }

    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|seconds| seconds.parse::<u64>().ok());
    let error = read_json::<ErrorResponse>(response).await.ok();
    // Only display known error codes. A proxy or untrusted endpoint can return arbitrary text.
    let detail = match error.as_ref().map(|body| body.error.as_str()) {
        Some("invalid_credentials") => "invalid username or password",
        Some("password_cooldown") => "login is temporarily on cooldown",
        Some("rate_limited") => "server rate limit reached",
        Some("unauthorized") => "session is unauthorized",
        Some("invalid_request") => "server rejected the request",
        Some("file_too_large") => "file exceeds the server upload limit",
        Some("insufficient_space") => "server has insufficient disk space",
        Some("upload_timeout") => "server stopped waiting for upload data",
        _ if status.is_redirection() => {
            "endpoint redirected the request, use the final deployment URL"
        }
        _ => status
            .canonical_reason()
            .unwrap_or("unexpected server response"),
    };
    let mut message = format!("{action}: {detail} (HTTP {})", status.as_u16());
    if let Some(seconds) = retry_after {
        message.push_str(&format!(". Retry in {seconds}s"));
    }
    message
}

async fn read_json<T: DeserializeOwned>(
    mut response: Response,
) -> std::result::Result<T, &'static str> {
    // Limit the body independently of upload size and tolerate broken proxy responses.
    let read = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "could not read the server response")?
        {
            if chunk.len() > MAX_JSON_BYTES - bytes.len() {
                return Err("server response is too large");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "server returned invalid JSON")
    };
    tokio::time::timeout(API_TIMEOUT, read)
        .await
        .map_err(|_| "timed out reading the server response")?
}

fn unknown_upload(reason: &str) -> Box<dyn Error> {
    format!("{reason}. The upload outcome is unknown. Check the share list before retrying.").into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_endpoint_path_when_building_api_routes() {
        for (endpoint, expected) in [
            (
                "http://127.0.0.1:3000",
                "http://127.0.0.1:3000/api/sessions",
            ),
            (
                "https://example.com/pigeon",
                "https://example.com/pigeon/api/sessions",
            ),
            (
                "https://example.com/pigeon/",
                "https://example.com/pigeon/api/sessions",
            ),
        ] {
            assert_eq!(
                api_url(&parse_endpoint(endpoint).unwrap(), "sessions").as_str(),
                expected
            );
        }
    }

    #[test]
    fn rejects_ambiguous_or_secret_bearing_endpoints() {
        for endpoint in [
            "example.com/pigeon",
            "file:///pigeon",
            "https:///example.com",
            "https://user:password@example.com/pigeon",
            "https://@example.com/pigeon",
            "https://example.com/pigeon?token=secret",
            "https://example.com/pigeon#/login",
        ] {
            assert!(parse_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn resolves_the_share_url_and_puts_the_password_in_the_fragment() {
        let endpoint = parse_endpoint("https://example.com/pigeon").unwrap();
        let relative = "/pigeon/#/sharing/0123456789abcdef0123456789abcdef";
        let link = share_url(&endpoint, relative).unwrap();
        assert_eq!(link.as_str(), format!("https://example.com{relative}"));
        assert_eq!(
            link_with_password(&link, "Ab12").as_str(),
            format!("https://example.com{relative}?password=Ab12")
        );
        assert!(
            share_url(
                &endpoint,
                "//other.example/#/sharing/0123456789abcdef0123456789abcdef"
            )
            .is_none()
        );
        assert!(share_url(&endpoint, "/pigeon/#/login").is_none());
    }
}
