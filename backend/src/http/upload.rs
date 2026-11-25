use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{
        Multipart, State,
        multipart::{Field, MultipartError, MultipartRejection},
    },
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};
use tokio::{fs::File, io::AsyncWriteExt, time::timeout};

use crate::store::{NewShare, StagedUpload};

use super::{App, api::authenticated, error::HttpError, storage_work};

// Account for multipart headers and optional fields separately from the file bytes.
pub const MULTIPART_OVERHEAD: usize = 1024 * 1024;
const RESERVE: u64 = 256 * 1024 * 1024;
const DISK_CHECK_INTERVAL: u64 = 8 * 1024 * 1024;
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn upload(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(StatusCode, Json<Value>), HttpError> {
    let username = authenticated(&app, &headers)?;
    let (stage, metadata) =
        receive(&app, multipart.map_err(|_| HttpError::invalid())?, username).await?;
    let store = app.store.clone();
    let share = storage_work(move || store.finish_upload(stage, metadata)).await?;
    tracing::debug!(size_bytes = share.size_bytes, "Upload saved");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "share_url": app.share_url(&share.code),
            "created_at": share.created_at,
            "expires_at": share.expires_at,
            "remaining_downloads": share.remaining_downloads,
        })),
    ))
}

async fn receive(
    app: &App,
    mut fields: Multipart,
    uploaded_by: String,
) -> Result<(StagedUpload, NewShare), HttpError> {
    let mut file = None;
    let mut password = None;
    let mut expires_in = None;
    let mut download_limit = None;

    while let Some(field) = timeout(CHUNK_TIMEOUT, fields.next_field())
        .await
        .map_err(|_| timed_out())?
        .map_err(multipart_error)?
    {
        match field.name() {
            Some("file") if file.is_none() => file = Some(receive_file(app, field).await?),
            Some("password") if password.is_none() => {
                let value = small_text(field).await?;
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return Err(HttpError::invalid());
                }
                password = Some(value);
            }
            Some("expiry_days") if expires_in.is_none() => {
                let days = positive_number(field).await?;
                let seconds = days.checked_mul(86_400).ok_or_else(HttpError::invalid)?;
                expires_in = Some(Duration::from_secs(seconds));
            }
            Some("max_downloads") if download_limit.is_none() => {
                download_limit = Some(positive_number(field).await?);
            }
            _ => return Err(HttpError::invalid()),
        }
    }

    let (stage, filename, size_bytes) = file.ok_or_else(HttpError::invalid)?;
    Ok((
        stage,
        NewShare {
            filename,
            size_bytes,
            uploaded_by,
            expires_in,
            download_limit,
            password,
        },
    ))
}

async fn receive_file(
    app: &App,
    mut field: Field<'_>,
) -> Result<(StagedUpload, String, u64), HttpError> {
    let filename = field.file_name().ok_or_else(HttpError::invalid)?;
    let filename = filename.rsplit(['/', '\\']).next().unwrap_or_default();
    if filename.is_empty() || filename.len() > 255 || filename.chars().any(char::is_control) {
        return Err(HttpError::invalid());
    }
    let filename = filename.to_owned();

    let store = app.store.clone();
    let mut stage = storage_work(move || store.start_upload()).await?;
    let mut file = File::from_std(stage.take_file());
    let mut size = 0_u64;
    let mut until_check = 0;
    while let Some(chunk) = timeout(CHUNK_TIMEOUT, field.chunk())
        .await
        .map_err(|_| timed_out())?
        .map_err(multipart_error)?
    {
        let size_after_chunk = size.checked_add(chunk.len() as u64).ok_or_else(too_large)?;
        if app
            .max_file_size
            .is_some_and(|max_size| size_after_chunk > max_size as u64)
        {
            return Err(too_large());
        }
        if size_after_chunk > until_check {
            let store = app.store.clone();
            let free = storage_work(move || store.available_bytes()).await?;
            if free < RESERVE + chunk.len() as u64 {
                return Err(HttpError::disk_full());
            }
            // Recheck before using the next block of disk space. Drive space can
            // also change because of programs outside Pigeon.
            until_check = size + (free - RESERVE).min(DISK_CHECK_INTERVAL.max(chunk.len() as u64));
        }
        file.write_all(&chunk).await?;
        size = size_after_chunk;
    }
    file.flush().await?;
    drop(file);
    Ok((stage, filename, size))
}

async fn small_text(mut field: Field<'_>) -> Result<String, HttpError> {
    let mut value = Vec::new();
    while let Some(chunk) = timeout(CHUNK_TIMEOUT, field.chunk())
        .await
        .map_err(|_| timed_out())?
        .map_err(multipart_error)?
    {
        if value.len() + chunk.len() > 64 {
            return Err(HttpError::invalid());
        }
        value.extend_from_slice(&chunk);
    }
    String::from_utf8(value).map_err(|_| HttpError::invalid())
}

async fn positive_number(field: Field<'_>) -> Result<u64, HttpError> {
    let value = small_text(field).await?;
    let number = value.parse::<u64>().map_err(|_| HttpError::invalid())?;
    if number == 0 || number > i64::MAX as u64 {
        return Err(HttpError::invalid());
    }
    Ok(number)
}

fn timed_out() -> HttpError {
    HttpError::new(StatusCode::REQUEST_TIMEOUT, "upload_timeout")
}

fn too_large() -> HttpError {
    HttpError::new(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large")
}

fn multipart_error(error: MultipartError) -> HttpError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large()
    } else {
        HttpError::invalid()
    }
}
