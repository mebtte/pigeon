use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{auth::LoginError, store::is_share_code};

use super::{App, error::HttpError, storage_work};

#[derive(Deserialize)]
pub struct Credentials {
    username: String,
    password: String,
}

#[derive(Deserialize)]
pub struct Extraction {
    password: Option<String>,
}

pub async fn login(
    State(app): State<Arc<App>>,
    payload: Result<Json<Credentials>, JsonRejection>,
) -> Result<Json<Value>, HttpError> {
    let Json(credentials) = payload.map_err(|_| HttpError::invalid())?;
    let token = app
        .auth
        .login(&credentials.username, &credentials.password)
        .map_err(|error| match error {
            LoginError::Invalid => HttpError::new(StatusCode::UNAUTHORIZED, "invalid_credentials"),
            LoginError::PasswordCooldown(wait) => HttpError::password_cooldown(wait),
            LoginError::Capacity => HttpError::limited(),
            LoginError::Entropy => HttpError::internal(),
        })?;
    tracing::debug!("Login session issued");
    Ok(Json(json!({ "token": token })))
}

pub async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> StatusCode {
    if let Some(token) = token(&headers) {
        app.auth.logout(token);
    }
    StatusCode::NO_CONTENT
}

fn token(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-pigeon-token")?.to_str().ok()
}

pub fn authenticated(app: &App, headers: &HeaderMap) -> Result<String, HttpError> {
    token(headers)
        .and_then(|token| app.auth.username(token))
        .ok_or_else(HttpError::unauthorized)
}

pub async fn config(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    authenticated(&app, &headers)?;
    let store = app.store.clone();
    let available_bytes = storage_work(move || store.available_bytes()).await?;
    Ok(Json(json!({
        "available_bytes": available_bytes,
        "max_file_size_bytes": app.max_file_size,
    })))
}

pub async fn extract_link(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    payload: Result<Json<Extraction>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), HttpError> {
    if !is_share_code(&id) {
        return Err(HttpError::missing());
    }
    let Json(extraction) = payload.map_err(|_| HttpError::invalid())?;
    let store = app.store.clone();
    let link = storage_work(move || store.issue_link(&id, extraction.password.as_deref())).await?;
    tracing::debug!(expires_at = link.expires_at, "Download link issued");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "download_url": app.download_url(&link.token),
            "expires_at": link.expires_at,
            "filename": link.filename,
            "size_bytes": link.size_bytes,
        })),
    ))
}
