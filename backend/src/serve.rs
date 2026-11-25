use std::{io, net::Ipv4Addr, sync::Arc, time::Duration};

use tokio::net::TcpListener;
use tracing_subscriber::{
    filter::{LevelFilter, Targets},
    prelude::*,
};

use crate::{config::ServeArguments, http, store::Store};

pub async fn run(args: ServeArguments) -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    let base = args.base_path().map_err(io::Error::other)?;
    let accounts = args.accounts().map_err(io::Error::other)?;
    tracing::debug!(
        base_path = %base.home(),
        account_count = accounts.len(),
        max_file_size_bytes = ?args.max_file_size,
        "Backend configuration loaded"
    );
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, args.port)).await?;
    let store = Store::open(&args.data)?;
    tracing::debug!("Storage initialized and startup cleanup completed");
    let state = Arc::new(http::App::new(
        base,
        accounts,
        store.clone(),
        args.max_file_size,
    )?);

    let cleanup = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await; // Store::open already ran cleanup.
        loop {
            interval.tick().await;
            let store = store.clone();
            if !matches!(
                tokio::task::spawn_blocking(move || store.cleanup()).await,
                Ok(Ok(()))
            ) {
                tracing::warn!("Storage cleanup failed; it will be retried");
            }
        }
    });

    tracing::info!(port = listener.local_addr()?.port(), "Pigeon listening");
    let result = axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown())
        .await;
    cleanup.abort();
    result?;
    Ok(())
}

fn init_logging() {
    let app_level = if cfg!(debug_assertions) {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let defaults = || {
        Targets::new()
            .with_default(LevelFilter::INFO)
            .with_target(env!("CARGO_CRATE_NAME"), app_level)
    };
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) if !value.trim().is_empty() => value.parse().unwrap_or_else(|_| {
            eprintln!("Invalid RUST_LOG; using default log levels");
            defaults()
        }),
        _ => defaults(),
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(filter)
        .init();
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
