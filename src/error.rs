use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Internal failures are logged in full and reported to the caller as a
        // bare 500: error text can carry database contents.
        tracing::error!(error = ?self, "request failed");
        let body = Json(json!({ "error": "internal server error" }));
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}
