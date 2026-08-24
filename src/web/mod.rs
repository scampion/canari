mod api;
mod ping;
pub mod ui;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::error::AppError;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/static/style.css", get(style))
        .route("/static/logo.png", get(logo))
        .route("/static/favicon.png", get(favicon))
        // Badges are public: they are addressed by an opaque token, never by
        // the uuid that authorises pings.
        .route("/badge/{token}", get(api::badge))
        .merge(ping::routes())
        .merge(api::routes(state.clone()))
        .merge(ui::routes(state.clone()))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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

// Assets are baked into the binary: deployment stays a single file, and there
// is no directory to get out of sync with the executable.
const CACHE: &str = "public, max-age=3600";

async fn style() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, CACHE),
        ],
        include_str!("../../static/style.css"),
    )
}

async fn logo() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, CACHE),
        ],
        include_bytes!("../../static/logo-192.png").as_slice(),
    )
}

async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, CACHE),
        ],
        include_bytes!("../../static/favicon-32.png").as_slice(),
    )
}
