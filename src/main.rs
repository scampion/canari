mod cli;
mod config;
mod db;
mod engine;
mod error;
mod model;
mod schedule;
mod state;
mod store;
mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use sqlx::SqlitePool;
use tracing_subscriber::EnvFilter;

use crate::cli::Command;
use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();

    // One-shot commands print their own output; the server logs.
    let default_filter = match config.command {
        Some(Command::Check(_)) => "warn",
        _ => "canari=info,tower_http=warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CANARI_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .init();

    let pool = db::connect(&config.db).await?;

    let result = match &config.command {
        Some(Command::Check(cmd)) => cli::run(&pool, &config, cmd).await,
        _ => {
            tracing::info!(db = %config.db.display(), "database ready");
            serve(config, pool.clone()).await
        }
    };

    // Let in-flight writes land before the WAL is closed.
    pool.close().await;
    result
}

async fn serve(config: Config, pool: SqlitePool) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(listen = %config.listen, ping_base = %config.ping_base(), "canari listening");

    let state = AppState {
        db: pool,
        config: Arc::new(config),
    };

    let alert_loop = tokio::spawn(engine::run(state.clone()));

    // ConnectInfo gives ping handlers the peer address to record.
    let app = web::router(state).into_make_service_with_connect_info::<SocketAddr>();

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error");

    // Nothing to drain: each tick commits its own transaction, and an aborted
    // one rolls back.
    alert_loop.abort();

    tracing::info!("shutdown complete");
    result
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
