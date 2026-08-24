use std::net::SocketAddr;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Router, body::Bytes};
use uuid::Uuid;

use crate::error::AppError;
use crate::model::PingKind;
use crate::state::AppState;
use crate::store::{self, PingInput};

/// Request bodies above this are rejected outright; what we keep is truncated
/// further down in the store.
const MAX_REQUEST_BODY: usize = 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        // Any method: clients ping with GET, POST, or HEAD depending on the
        // tool at hand, and none of them should get a 405.
        .route("/ping/{uuid}", any(ping))
        .route("/ping/{uuid}/{suffix}", any(ping_with_suffix))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
}

async fn ping(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    handle(state, uuid, None, peer, method, headers, body).await
}

async fn ping_with_suffix(
    State(state): State<AppState>,
    Path((uuid, suffix)): Path<(String, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    handle(state, uuid, Some(suffix), peer, method, headers, body).await
}

async fn handle(
    state: AppState,
    uuid: String,
    suffix: Option<String>,
    peer: SocketAddr,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let (kind, exit_code) = match suffix.as_deref() {
        None => (PingKind::Success, None),
        Some(s) => match parse_suffix(s) {
            Some(parsed) => parsed,
            None => {
                return Ok((StatusCode::BAD_REQUEST, "unrecognized ping suffix\n").into_response());
            }
        },
    };

    // Reject malformed uuids before touching the database.
    if Uuid::parse_str(&uuid).is_err() {
        return Ok(not_found());
    }

    let input = PingInput {
        kind: Some(kind),
        exit_code,
        remote_addr: Some(client_ip(&headers, peer)),
        user_agent: header_string(&headers, header::USER_AGENT),
        method: Some(method.as_str().to_owned()),
        body: (!body.is_empty()).then(|| String::from_utf8_lossy(&body).into_owned()),
    };

    match store::record_ping(&state.db, &uuid, input).await? {
        Some(outcome) => {
            tracing::debug!(
                uuid,
                %kind,
                n = outcome.n,
                from = %outcome.previous,
                to = %outcome.status,
                "ping recorded"
            );
            Ok((StatusCode::OK, "OK\n").into_response())
        }
        None => Ok(not_found()),
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// `start`, `fail`, `log`, or an exit code: 0 is a success, anything else a
/// failure carrying the code.
fn parse_suffix(suffix: &str) -> Option<(PingKind, Option<i64>)> {
    match suffix {
        "start" => Some((PingKind::Start, None)),
        "fail" => Some((PingKind::Fail, None)),
        "log" => Some((PingKind::Log, None)),
        other => match other.parse::<u16>() {
            Ok(0) => Some((PingKind::Success, Some(0))),
            Ok(code) if code <= 255 => Some((PingKind::Fail, Some(code.into()))),
            _ => None,
        },
    }
}

/// Reported client address. `X-Forwarded-For` wins when present, since the
/// common deployment is behind a reverse proxy — it is display-only metadata,
/// never used for authorization.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| peer.ip().to_string())
}

fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_suffix("start"), Some((PingKind::Start, None)));
        assert_eq!(parse_suffix("fail"), Some((PingKind::Fail, None)));
        assert_eq!(parse_suffix("log"), Some((PingKind::Log, None)));
        assert_eq!(parse_suffix("0"), Some((PingKind::Success, Some(0))));
        assert_eq!(parse_suffix("1"), Some((PingKind::Fail, Some(1))));
        assert_eq!(parse_suffix("255"), Some((PingKind::Fail, Some(255))));
        assert_eq!(parse_suffix("256"), None);
        assert_eq!(parse_suffix("nope"), None);
    }
}
