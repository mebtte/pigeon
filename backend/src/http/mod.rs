mod api;
mod download;
mod error;
mod pages;
mod shares;
mod upload;

use std::{collections::HashMap, io, sync::Arc, time::Instant};

use axum::{
    Router,
    extract::{DefaultBodyLimit, MatchedPath, Request},
    http::{HeaderValue, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use tracing::Instrument;

use crate::{auth::Auth, config::BasePath, store::Store};
use error::HttpError;
use pages::Frontend;

pub struct App {
    base: BasePath,
    auth: Auth,
    store: Store,
    frontend: Frontend,
    max_file_size: Option<usize>,
}

impl App {
    pub fn new(
        base: BasePath,
        accounts: HashMap<String, String>,
        store: Store,
        max_file_size: Option<usize>,
    ) -> io::Result<Self> {
        let frontend = Frontend::new(&base)?;
        Ok(Self {
            base,
            auth: Auth::new(accounts),
            store,
            frontend,
            max_file_size,
        })
    }

    fn share_url(&self, code: &str) -> String {
        // The trailing slash lets prefix-only proxy rules forward the share page.
        format!("{}#/s/{code}", self.base.path("/"))
    }

    fn download_url(&self, token: &str) -> String {
        format!("{}?download={token}", self.base.path("/sharing"))
    }
}

pub fn router(app: Arc<App>) -> Router {
    let upload_body_limit = match app.max_file_size {
        Some(size) => DefaultBodyLimit::max(size.saturating_add(upload::MULTIPART_OVERHEAD)),
        None => DefaultBodyLimit::disable(),
    };
    let home = app.base.path("/");
    let mut router = Router::new().route(&home, get(pages::home));
    if home != app.base.home() {
        // Preserve the original entry point for existing bookmarks and share links.
        router = router.route(app.base.home(), get(pages::home));
    }
    router
        .route(&app.base.path("/assets/{name}"), get(pages::asset))
        .route(&app.base.path("/sharing"), get(download::download))
        .route(&app.base.path("/robots.txt"), get(pages::robots))
        .route(
            &app.base.path("/api/sessions"),
            post(api::login)
                .delete(api::logout)
                .layer(DefaultBodyLimit::max(4096)),
        )
        .route(&app.base.path("/api/config"), get(api::config))
        .route(
            &app.base.path("/api/shares"),
            get(shares::list)
                .post(upload::upload)
                .layer(upload_body_limit),
        )
        .route(
            &app.base.path("/api/shares/{id}"),
            get(shares::metadata).delete(shares::delete),
        )
        .route(
            &app.base.path("/api/shares/{id}/download-links"),
            post(api::extract_link).layer(DefaultBodyLimit::max(4096)),
        )
        .fallback(error::not_found)
        .layer(middleware::from_fn(response_headers))
        .with_state(app)
}

async fn response_headers(request: Request, next: Next) -> Response {
    // Use the route pattern so share IDs, tokens, and query strings stay out of logs.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str);
    let span = tracing::debug_span!("http", method = %request.method(), route = %route);
    async move {
        let start = Instant::now();
        let mut response = next.run(request).await;
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        // Streaming bodies are sent after the handler returns, so this times response preparation.
        tracing::debug!(
            status = response.status().as_u16(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "HTTP request handled"
        );
        response
    }
    .instrument(span)
    .await
}

async fn storage_work<R: Send + 'static>(
    operation: impl FnOnce() -> Result<R, crate::store::StoreError> + Send + 'static,
) -> Result<R, HttpError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| HttpError::internal())?
        .map_err(HttpError::from)
}

#[cfg(test)]
mod tests;
