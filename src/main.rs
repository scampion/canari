mod auth;
mod cli;
mod config;
mod db;
mod engine;
mod error;
mod model;
mod notify;
mod schedule;
mod state;
mod store;
mod web;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::Command;
use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();

    // One-shot commands print their own output; the server logs.
    let default_filter = match config.command {
        Some(Command::Check(_)) | Some(Command::Channel(_)) | Some(Command::Admin(_)) => "warn",
        _ => "canari=info,tower_http=warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CANARI_LOG")
                .unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .init();

    // rustls needs a crypto provider chosen explicitly. ring is picked over
    // aws-lc-rs because it builds for musl without cmake or a C toolchain,
    // which is what keeps "one static binary" a one-command build.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("installing the rustls crypto provider"))?;

    let pool = db::connect(&config.db).await?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("canari/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let state = AppState {
        db: pool.clone(),
        config: Arc::new(config),
        http,
    };

    let result = match &state.config.command {
        Some(Command::Check(cmd)) => cli::run_check(&state, cmd).await,
        Some(Command::Channel(cmd)) => cli::run_channel(&state, cmd).await,
        Some(Command::Admin(cmd)) => cli::run_admin(&state, cmd).await,
        _ => {
            tracing::info!(db = %state.config.db.display(), "database ready");
            serve(state.clone()).await
        }
    };

    // Let in-flight writes land before the WAL is closed.
    pool.close().await;
    result
}

async fn serve(state: AppState) -> anyhow::Result<()> {
    let listen = state.config.listen;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, ping_base = %state.config.ping_base(), "canari listening");

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
