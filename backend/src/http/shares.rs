use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::store::{ListedShare, SharePage};

use super::{App, api::authenticated, error::HttpError, storage_work};

const DEFAULT_PAGE_SIZE: u64 = 20;
const MAX_PAGE_SIZE: u64 = 100;

#[derive(Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    page: Option<u64>,
    page_size: Option<u64>,
}

#[derive(Serialize)]
pub(super) struct ShareMetadata {
    id: String,
    requires_password: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

pub(super) async fn metadata(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> Result<Json<ShareMetadata>, HttpError> {
    let store = app.store.clone();
    let share = storage_work(move || store.share(&id)).await?;
    let is_public = !share.requires_password;
    Ok(Json(ShareMetadata {
        id: share.code,
        requires_password: share.requires_password,
        filename: is_public.then_some(share.filename),
        size_bytes: is_public.then_some(share.size_bytes),
    }))
}

pub async fn list(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<Value>, HttpError> {
    let owner = authenticated(&app, &headers)?;
    let Query(query) = query.map_err(|_| HttpError::invalid())?;
    let search = query.q.unwrap_or_default();
    let page = query.page.unwrap_or(1);
    let page_size = query.page_size.unwrap_or(DEFAULT_PAGE_SIZE);

    if !(1..=MAX_PAGE_SIZE).contains(&page_size) || search.len() > 1024 {
        return Err(HttpError::invalid());
    }

    let store = app.store.clone();
    let result: SharePage =
        storage_work(move || store.list_shares(&owner, search.trim(), page, page_size)).await?;
    let items: Vec<_> = result
        .items
        .into_iter()
        .map(|share: ListedShare| {
            json!({
                "filename": share.filename,
                "size_bytes": share.size_bytes,
                "id": share.id,
                "created_at": share.created_at,
                "expires_at": share.expires_at,
                "remaining_downloads": share.remaining_downloads,
                "requires_password": share.requires_password,
                "share_url": app.share_url(&share.id),
            })
        })
        .collect();
    tracing::debug!(page = result.page, total = result.total, "Shares listed");
    Ok(Json(json!({
        "items": items,
        "page": result.page,
        "page_size": result.page_size,
        "total": result.total,
    })))
}

pub async fn delete(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, HttpError> {
    let owner = authenticated(&app, &headers)?;
    let store = app.store.clone();
    storage_work(move || store.delete_share(&id, &owner)).await?;
    tracing::debug!("Share deleted");
    Ok(StatusCode::NO_CONTENT)
}
