use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::error::AppError;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Placeholder until the web UI lands; confirms which base URL this instance
/// hands out in ping URLs.
async fn index(State(state): State<AppState>) -> String {
    format!(
        "canari {}\nping endpoint: {}/<uuid>\n",
        env!("CARGO_PKG_VERSION"),
        state.config.ping_base()
    )
}

/// Liveness *and* readiness: a reachable process with an unusable database is
/// not healthy, so the database is actually queried.
async fn healthz(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    })))
}
