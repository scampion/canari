mod config;
mod db;
mod error;
mod state;
mod web;

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CANARI_LOG")
                .unwrap_or_else(|_| EnvFilter::new("canari=info,tower_http=warn")),
        )
        .init();

    let pool = db::connect(&config.db).await?;
    tracing::info!(db = %config.db.display(), "database ready");

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(listen = %config.listen, ping_base = %config.ping_base(), "canari listening");

    let state = AppState {
        db: pool.clone(),
        config: Arc::new(config),
    };

    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    // Let in-flight writes land before the WAL is closed.
    pool.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
