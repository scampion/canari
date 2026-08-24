use axum::extract::{Path, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::model::{Check, CheckKind, format_rfc3339, parse_duration};
use crate::state::AppState;
use crate::store::{self, NewCheck};

pub const API_KEY_HEADER: &str = "x-api-key";

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/v1/checks", get(list_checks).post(create_check))
        .route(
            "/api/v1/checks/{uuid}",
            get(get_check).post(update_check).delete(delete_check),
        )
        .route("/api/v1/checks/{uuid}/pause", post(pause_check))
        .route("/api/v1/checks/{uuid}/resume", post(resume_check))
        .route("/api/v1/checks/{uuid}/pings", get(list_pings))
        .route("/api/v1/channels", get(list_channels))
        .route("/api/v1/keys/{id}", delete(revoke_key))
        .route_layer(middleware::from_fn_with_state(state, require_api_key))
}

/// Authenticate every API request, and refuse writes to read-only keys.
///
/// Read-only keys exist so a status page or a dashboard can poll the API
/// without holding a credential that could delete checks.
async fn require_api_key(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let Some(presented) = presented else {
        return AppError::Unauthorized.into_response();
    };

    let key = match store::authenticate_api_key(&state.db, &presented).await {
        Ok(Some(key)) => key,
        Ok(None) => return AppError::Unauthorized.into_response(),
        Err(err) => {
            tracing::error!(?err, "cannot verify api key");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if key.read_only && request.method() != Method::GET {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this API key is read-only" })),
        )
            .into_response();
    }

    next.run(request).await
}

// ----------------------------------------------------------------- handlers

async fn list_checks(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let checks = store::list_checks(&state.db).await?;
    let checks: Vec<Value> = checks
        .iter()
        .map(|check| check_json(check, &state))
        .collect();
    Ok(Json(json!({ "checks": checks })))
}

async fn get_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<Value>, AppError> {
    let check = store::get_check(&state.db, &uuid)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(check_json(&check, &state)))
}

/// Payload for create and update. Durations are the same human strings the CLI
/// and the web form accept, so all three agree on "5m".
#[derive(Debug, Deserialize)]
struct CheckPayload {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    period: Option<String>,
    #[serde(default)]
    grace: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    /// Channel ids to notify. Omitted means "leave unchanged" on update, and
    /// "none" on create.
    #[serde(default)]
    channels: Option<Vec<i64>>,
}

impl CheckPayload {
    fn to_new_check(&self) -> Result<NewCheck, AppError> {
        let kind = match self.kind.as_deref() {
            Some("cron") => CheckKind::Cron,
            Some("simple") | None => CheckKind::Simple,
            Some(other) => {
                return Err(AppError::BadRequest(format!(
                    "unknown kind {other:?}, expected \"simple\" or \"cron\""
                )));
            }
        };

        if self.name.trim().is_empty() {
            return Err(AppError::BadRequest("name is required".into()));
        }

        let period_s = parse_duration(self.period.as_deref().unwrap_or("1d"))
            .map_err(|err| AppError::BadRequest(err.to_string()))?;
        let grace_s = parse_duration(self.grace.as_deref().unwrap_or("1h"))
            .map_err(|err| AppError::BadRequest(err.to_string()))?;

        Ok(NewCheck {
            name: self.name.trim().to_string(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            kind,
            period_s,
            grace_s,
            cron_expr: self.cron.clone(),
            tz: self.tz.clone().unwrap_or_else(|| "UTC".into()),
        })
    }
}

async fn create_check(
    State(state): State<AppState>,
    Json(payload): Json<CheckPayload>,
) -> Result<Response, AppError> {
    let new = payload.to_new_check()?;
    let check = store::create_check(&state.db, new)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;

    if let Some(channels) = &payload.channels {
        store::set_check_channels(&state.db, check.id, channels).await?;
    }

    Ok((StatusCode::CREATED, Json(check_json(&check, &state))).into_response())
}

async fn update_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    Json(payload): Json<CheckPayload>,
) -> Result<Json<Value>, AppError> {
    let existing = store::get_check(&state.db, &uuid)
        .await?
        .ok_or(AppError::NotFound)?;

    let new = payload.to_new_check()?;
    let check = store::update_check(&state.db, &uuid, new)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;

    if let Some(channels) = &payload.channels {
        store::set_check_channels(&state.db, existing.id, channels).await?;
    }

    Ok(Json(check_json(&check, &state)))
}

async fn delete_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<StatusCode, AppError> {
    if store::delete_check(&state.db, &uuid).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

async fn pause_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<Value>, AppError> {
    if !store::pause_check(&state.db, &uuid).await? {
        return Err(AppError::NotFound);
    }
    let check = store::get_check(&state.db, &uuid)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(check_json(&check, &state)))
}

async fn resume_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<Value>, AppError> {
    if !store::resume_check(&state.db, &uuid).await? {
        return Err(AppError::NotFound);
    }
    let check = store::get_check(&state.db, &uuid)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(check_json(&check, &state)))
}

async fn list_pings(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Json<Value>, AppError> {
    let check = store::get_check(&state.db, &uuid)
        .await?
        .ok_or(AppError::NotFound)?;

    let pings: Vec<Value> = store::list_pings(&state.db, check.id, 100)
        .await?
        .into_iter()
        .map(|ping| {
            json!({
                "n": ping.n,
                "time": format_rfc3339(ping.ts),
                "kind": ping.kind.as_str(),
                "exit_code": ping.exit_code,
                "duration_ms": ping.duration_ms,
                "remote_addr": ping.remote_addr,
                "user_agent": ping.user_agent,
                "method": ping.method,
                "body": ping.body,
            })
        })
        .collect();

    Ok(Json(json!({ "pings": pings })))
}

async fn list_channels(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let channels: Vec<Value> = store::list_channels(&state.db)
        .await?
        .iter()
        .map(|channel| {
            json!({
                "id": channel.id,
                "name": channel.name,
                "kind": channel.kind.as_str(),
                "enabled": channel.enabled,
                // Deliberately not the raw config: it holds tokens.
                "target": crate::web::ui::describe_target(channel),
            })
        })
        .collect();
    Ok(Json(json!({ "channels": channels })))
}

async fn revoke_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    if store::delete_api_key(&state.db, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

fn check_json(check: &Check, state: &AppState) -> Value {
    json!({
        "uuid": check.uuid,
        "name": check.name,
        "description": check.description,
        "tags": check.tags.split_whitespace().collect::<Vec<_>>(),
        "kind": match check.kind { CheckKind::Simple => "simple", CheckKind::Cron => "cron" },
        "period_s": check.period_s,
        "grace_s": check.grace_s,
        "cron": check.cron_expr,
        "tz": check.tz,
        "status": check.status.as_str(),
        "schedule": check.schedule_summary(),
        "last_ping": check.last_ping_at.map(format_rfc3339),
        "last_duration_ms": check.last_duration_ms,
        "late_after": check.alert_after.map(format_rfc3339),
        "n_pings": check.n_pings,
        "ping_url": format!("{}/{}", state.config.ping_base(), check.uuid),
        "badge_url": state.config.badge_url(&check.badge_token),
        "created_at": format_rfc3339(check.created_at),
    })
}

// ------------------------------------------------------------------ badges

/// Status badge. Unauthenticated on purpose — badges end up in public READMEs —
/// which is why they are addressed by badge_token and not by the check uuid.
pub async fn badge(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    let token = token.strip_suffix(".svg").unwrap_or(&token);

    let check = sqlx::query_as::<_, Check>("SELECT * FROM checks WHERE badge_token = ?")
        .bind(token)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let svg = render_badge(&check.name, check.status.as_str());
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            // Short cache: a badge that keeps claiming "up" for an hour after an
            // outage is worse than no badge.
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        svg,
    )
        .into_response())
}

fn status_color(status: &str) -> &'static str {
    match status {
        "up" => "#2fb380",
        "grace" => "#f5b301",
        "down" => "#e03131",
        "paused" => "#6b8590",
        _ => "#29a3bd",
    }
}

/// Flat badge in the shields.io style, drawn by hand: one small SVG is cheaper
/// than a dependency, and it keeps the binary self-contained.
fn render_badge(label: &str, status: &str) -> String {
    let label = truncate_label(label);
    // 11px DejaVu Sans averages ~6.5px per character; padding is 5px each side.
    let label_width = (label.chars().count() as f32 * 6.5).round() + 10.0;
    let status_width = (status.chars().count() as f32 * 6.5).round() + 10.0;
    let total = label_width + status_width;
    let color = status_color(status);

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{total}" height="20" role="img" aria-label="{label}: {status}">
  <title>{label}: {status}</title>
  <linearGradient id="s" x2="0" y2="100%">
    <stop offset="0" stop-color="#bbb" stop-opacity=".1"/>
    <stop offset="1" stop-opacity=".1"/>
  </linearGradient>
  <clipPath id="r"><rect width="{total}" height="20" rx="3" fill="#fff"/></clipPath>
  <g clip-path="url(#r)">
    <rect width="{label_width}" height="20" fill="#3c4a52"/>
    <rect x="{label_width}" width="{status_width}" height="20" fill="{color}"/>
    <rect width="{total}" height="20" fill="url(#s)"/>
  </g>
  <g fill="#fff" text-anchor="middle" font-family="Verdana,DejaVu Sans,Geneva,sans-serif" font-size="11">
    <text x="{label_x}" y="15" fill="#010101" fill-opacity=".3">{label}</text>
    <text x="{label_x}" y="14">{label}</text>
    <text x="{status_x}" y="15" fill="#010101" fill-opacity=".3">{status}</text>
    <text x="{status_x}" y="14">{status}</text>
  </g>
</svg>
"##,
        label_x = label_width / 2.0,
        status_x = label_width + status_width / 2.0,
    )
}

/// Keep badges a sane width, and escape what goes into the SVG.
fn truncate_label(label: &str) -> String {
    let trimmed: String = if label.chars().count() > 28 {
        label.chars().take(27).chain(['…']).collect()
    } else {
        label.to_string()
    };

    trimmed
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_escapes_markup_in_names() {
        let svg = render_badge("<script>alert(1)</script>", "up");
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("&lt;script&gt;"));
    }

    #[test]
    fn badge_colors_follow_status() {
        assert!(render_badge("job", "down").contains("#e03131"));
        assert!(render_badge("job", "up").contains("#2fb380"));
        assert!(render_badge("job", "paused").contains("#6b8590"));
    }

    #[test]
    fn badge_label_is_bounded() {
        let svg = render_badge(&"x".repeat(200), "up");
        let width: f32 = svg
            .split("width=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.parse().ok())
            .expect("width attribute");
        assert!(width < 250.0, "badge too wide: {width}");
    }
}
