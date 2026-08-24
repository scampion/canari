use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    /// Shared so notification deliveries reuse connections instead of opening
    /// a TLS session per alert.
    pub http: reqwest::Client,
}
