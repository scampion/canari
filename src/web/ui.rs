use std::collections::HashMap;
use std::time::Duration;

use anyhow::anyhow;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::error::AppError;
use crate::model::{Check, ChannelKind, CheckKind, format_duration, format_ts, now, parse_duration};
use crate::notify;
use crate::state::AppState;
use crate::store::{self, Channel, NewCheck};

/// Delay applied to a failed sign-in, to take the speed out of guessing.
const LOGIN_FAILURE_DELAY: Duration = Duration::from_secs(1);

pub fn routes(state: AppState) -> Router<AppState> {
    let protected = Router::new()
        .route("/", get(index))
        .route("/checks", post(create_check))
        .route("/checks/new", get(new_check_form))
        .route("/checks/{uuid}", get(check_detail).post(update_check))
        .route("/checks/{uuid}/edit", get(edit_check_form))
        .route("/checks/{uuid}/pause", post(pause_check))
        .route("/checks/{uuid}/resume", post(resume_check))
        .route("/checks/{uuid}/delete", post(delete_check))
        .route("/channels", get(channels).post(create_channel))
        .route("/channels/{id}/test", post(test_channel))
        .route("/channels/{id}/delete", post(delete_channel))
        .route_layer(middleware::from_fn_with_state(state, auth::require_auth));

    Router::new()
        .route("/login", get(login_form).post(login))
        .route("/logout", post(logout))
        .merge(protected)
}

// ---------------------------------------------------------------- templates

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    flash_ok: Option<String>,
    flash_err: Option<String>,
    rows: Vec<CheckRow>,
    summary: String,
    down_count: usize,
}

struct CheckRow {
    uuid: String,
    name: String,
    status: String,
    tags: Vec<String>,
    schedule: String,
    last_ping: String,
    late_in: String,
    n_pings: i64,
}

#[derive(Template)]
#[template(path = "check.html")]
struct CheckTemplate {
    flash_ok: Option<String>,
    flash_err: Option<String>,
    uuid: String,
    name: String,
    status: String,
    schedule: String,
    last_ping: String,
    late_in: String,
    n_pings: i64,
    last_duration: Option<String>,
    channels: String,
    description: String,
    tags: Vec<String>,
    ping_url: String,
    pings: Vec<PingRow>,
}

struct PingRow {
    n: i64,
    when: String,
    kind: String,
    exit_code: String,
    duration: String,
    method: String,
    remote_addr: String,
    user_agent: String,
    body: String,
}

#[derive(Template)]
#[template(path = "check_form.html")]
struct CheckFormTemplate {
    flash_ok: Option<String>,
    flash_err: Option<String>,
    heading: String,
    action: String,
    cancel_url: String,
    name: String,
    description: String,
    tags: String,
    kind: String,
    period: String,
    cron: String,
    tz: String,
    grace: String,
    channels: Vec<ChannelChoice>,
}

struct ChannelChoice {
    id: i64,
    name: String,
    kind: String,
    target: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "channels.html")]
struct ChannelsTemplate {
    flash_ok: Option<String>,
    flash_err: Option<String>,
    rows: Vec<ChannelRow>,
}

struct ChannelRow {
    id: i64,
    kind: String,
    name: String,
    target: String,
    check_count: i64,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

/// Flash messages travel in the query string, so no server-side session state
/// is needed to survive a redirect.
#[derive(Debug, Default, Deserialize)]
struct Flash {
    ok: Option<String>,
    err: Option<String>,
}

// ------------------------------------------------------------------- pages

async fn index(
    State(state): State<AppState>,
    Query(flash): Query<Flash>,
) -> Result<Response, AppError> {
    let checks = store::list_checks(&state.db).await?;
    let ts = now();

    let down_count = checks
        .iter()
        .filter(|c| c.status.as_str() == "down")
        .count();
    let summary = format!(
        "{} check{}, {} down",
        checks.len(),
        if checks.len() == 1 { "" } else { "s" },
        down_count
    );

    let rows = checks
        .into_iter()
        .map(|check| CheckRow {
            status: check.status.to_string(),
            tags: tag_list(&check.tags),
            schedule: check.schedule_summary(),
            last_ping: format_ago(check.last_ping_at, ts),
            late_in: format_late_in(&check, ts),
            n_pings: check.n_pings,
            uuid: check.uuid,
            name: check.name,
        })
        .collect();

    render(IndexTemplate {
        flash_ok: flash.ok,
        flash_err: flash.err,
        rows,
        summary,
        down_count,
    })
}

async fn check_detail(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    Query(flash): Query<Flash>,
) -> Result<Response, AppError> {
    let Some(check) = store::get_check(&state.db, &uuid).await? else {
        return Ok(not_found());
    };
    let ts = now();

    let channels = store::channels_for_check(&state.db, check.id).await?;
    let channel_summary = if channels.is_empty() {
        "none — alerts go nowhere".to_string()
    } else {
        channels
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let pings = store::list_pings(&state.db, check.id, 50)
        .await?
        .into_iter()
        .map(|ping| PingRow {
            n: ping.n,
            when: format_ts(ping.ts),
            kind: ping.kind.to_string(),
            exit_code: ping.exit_code.map(|c| c.to_string()).unwrap_or_default(),
            duration: ping
                .duration_ms
                .map(|d| format_duration(d / 1000))
                .unwrap_or_default(),
            method: ping.method.unwrap_or_default(),
            remote_addr: ping.remote_addr.unwrap_or_default(),
            user_agent: ping.user_agent.unwrap_or_default(),
            body: ping.body.unwrap_or_default(),
        })
        .collect();

    render(CheckTemplate {
        flash_ok: flash.ok,
        flash_err: flash.err,
        status: check.status.to_string(),
        schedule: check.schedule_summary(),
        last_ping: format_ago(check.last_ping_at, ts),
        late_in: format_late_in(&check, ts),
        n_pings: check.n_pings,
        last_duration: check.last_duration_ms.map(|d| format_duration(d / 1000)),
        channels: channel_summary,
        description: check.description.clone(),
        tags: tag_list(&check.tags),
        ping_url: format!("{}/{}", state.config.ping_base(), check.uuid),
        pings,
        uuid: check.uuid,
        name: check.name,
    })
}

async fn new_check_form(State(state): State<AppState>) -> Result<Response, AppError> {
    let channels = channel_choices(&state, &[]).await?;
    render(CheckFormTemplate {
        flash_ok: None,
        flash_err: None,
        heading: "New check".into(),
        action: "/checks".into(),
        cancel_url: "/".into(),
        name: String::new(),
        description: String::new(),
        tags: String::new(),
        kind: "simple".into(),
        period: "1d".into(),
        cron: String::new(),
        tz: "UTC".into(),
        grace: "1h".into(),
        channels,
    })
}

async fn edit_check_form(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Response, AppError> {
    let Some(check) = store::get_check(&state.db, &uuid).await? else {
        return Ok(not_found());
    };
    let attached: Vec<i64> = store::channels_for_check(&state.db, check.id)
        .await?
        .iter()
        .map(|c| c.id)
        .collect();

    render(CheckFormTemplate {
        flash_ok: None,
        flash_err: None,
        heading: format!("Edit {}", check.name),
        action: format!("/checks/{}", check.uuid),
        cancel_url: format!("/checks/{}", check.uuid),
        name: check.name.clone(),
        description: check.description.clone(),
        tags: check.tags.clone(),
        kind: match check.kind {
            CheckKind::Simple => "simple".into(),
            CheckKind::Cron => "cron".into(),
        },
        period: format_duration(check.period_s),
        cron: check.cron_expr.clone().unwrap_or_default(),
        tz: check.tz.clone(),
        grace: format_duration(check.grace_s),
        channels: channel_choices(&state, &attached).await?,
    })
}

async fn create_check(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let new = match new_check_from_form(&form) {
        Ok(new) => new,
        Err(err) => return form_with_error(&state, &form, None, &err.to_string()).await,
    };

    let check = match store::create_check(&state.db, new).await {
        Ok(check) => check,
        Err(err) => return form_with_error(&state, &form, None, &err.to_string()).await,
    };

    store::set_check_channels(&state.db, check.id, &selected_channels(&form)).await?;
    Ok(redirect_ok(
        &format!("/checks/{}", check.uuid),
        "Check created",
    ))
}

async fn update_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let Some(check) = store::get_check(&state.db, &uuid).await? else {
        return Ok(not_found());
    };

    let new = match new_check_from_form(&form) {
        Ok(new) => new,
        Err(err) => return form_with_error(&state, &form, Some(&check), &err.to_string()).await,
    };

    if let Err(err) = store::update_check(&state.db, &uuid, new).await {
        return form_with_error(&state, &form, Some(&check), &err.to_string()).await;
    }

    store::set_check_channels(&state.db, check.id, &selected_channels(&form)).await?;
    Ok(redirect_ok(&format!("/checks/{uuid}"), "Check saved"))
}

async fn pause_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Response, AppError> {
    store::pause_check(&state.db, &uuid).await?;
    Ok(redirect_ok(&format!("/checks/{uuid}"), "Check paused"))
}

async fn resume_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Response, AppError> {
    store::resume_check(&state.db, &uuid).await?;
    Ok(redirect_ok(&format!("/checks/{uuid}"), "Check resumed"))
}

async fn delete_check(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Response, AppError> {
    store::delete_check(&state.db, &uuid).await?;
    Ok(redirect_ok("/", "Check deleted"))
}

async fn channels(
    State(state): State<AppState>,
    Query(flash): Query<Flash>,
) -> Result<Response, AppError> {
    let mut rows = Vec::new();
    for channel in store::list_channels(&state.db).await? {
        let check_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM check_channels WHERE channel_id = ?",
        )
        .bind(channel.id)
        .fetch_one(&state.db)
        .await?;

        rows.push(ChannelRow {
            id: channel.id,
            kind: channel.kind.to_string(),
            target: describe_target(&channel),
            name: channel.name,
            check_count,
        });
    }

    render(ChannelsTemplate {
        flash_ok: flash.ok,
        flash_err: flash.err,
        rows,
    })
}

async fn create_channel(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let name = field(&form, "name");
    if name.is_empty() {
        return Ok(redirect_err("/channels", "Channel name is required"));
    }

    let (kind, config) = match field(&form, "kind") {
        "ntfy" => {
            let priority = field(&form, "priority");
            (
                ChannelKind::Ntfy,
                json!({
                    "server": non_empty(field(&form, "server")).unwrap_or("https://ntfy.sh"),
                    "topic": field(&form, "topic"),
                    "token": non_empty(field(&form, "token")),
                    "priority": priority.parse::<u8>().ok(),
                }),
            )
        }
        _ => {
            // "Name: value" per line, matching what a reverse proxy config or
            // curl invocation looks like.
            let mut headers = serde_json::Map::new();
            for line in field(&form, "headers").lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match line.split_once(':') {
                    Some((key, value)) => {
                        headers.insert(key.trim().to_string(), json!(value.trim()));
                    }
                    None => {
                        return Ok(redirect_err(
                            "/channels",
                            &format!("Header {line:?} is not \"Name: value\""),
                        ));
                    }
                }
            }

            (
                ChannelKind::Webhook,
                json!({
                    "url": field(&form, "url"),
                    "method": non_empty(field(&form, "method")).unwrap_or("POST"),
                    "headers": headers,
                    "body": non_empty(field(&form, "body")),
                }),
            )
        }
    };

    match store::create_channel(&state.db, kind, name, &config.to_string()).await {
        Ok(_) => Ok(redirect_ok("/channels", "Channel added")),
        Err(err) => Ok(redirect_err("/channels", &err.to_string())),
    }
}

async fn test_channel(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(channel) = store::get_channel(&state.db, id).await? else {
        return Ok(not_found());
    };

    match notify::send_test(&state, &channel).await {
        Ok(()) => Ok(redirect_ok(
            "/channels",
            &format!("Test alert sent through {}", channel.name),
        )),
        Err(err) => Ok(redirect_err("/channels", &format!("Delivery failed: {err}"))),
    }
}

async fn delete_channel(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    store::delete_channel(&state.db, id).await?;
    Ok(redirect_ok("/channels", "Channel deleted"))
}

// -------------------------------------------------------------------- auth

async fn login_form(
    State(state): State<AppState>,
    Query(flash): Query<Flash>,
) -> Result<Response, AppError> {
    if !auth::password_is_set(&state.db).await? {
        return Ok(setup_required());
    }
    render(LoginTemplate { error: flash.err })
}

async fn login(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AppError> {
    if auth::verify_password(&state.db, field(&form, "password")).await? {
        let token = auth::create_session(&state.db).await?;
        let cookie = auth::session_cookie(&token, &state.config.site_url);
        return Ok((
            StatusCode::SEE_OTHER,
            [(header::SET_COOKIE, cookie), (header::LOCATION, "/".into())],
        )
            .into_response());
    }

    tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
    Ok(redirect_err("/login", "Wrong password"))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = auth::token_from_headers(&headers) {
        auth::destroy_session(&state.db, &token).await?;
    }
    Ok((
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, auth::cleared_cookie()),
            (header::LOCATION, "/login".into()),
        ],
    )
        .into_response())
}

fn setup_required() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "canari has no password yet.\n\nSet one with:  canari admin set-password\n",
    )
        .into_response()
}

// ----------------------------------------------------------------- helpers

fn render<T: Template>(template: T) -> Result<Response, AppError> {
    let html = template
        .render()
        .map_err(|err| AppError::Other(anyhow!("rendering template: {err}")))?;
    Ok(Html(html).into_response())
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

fn redirect_ok(path: &str, message: &str) -> Response {
    Redirect::to(&format!("{path}?ok={}", urlencoding::encode(message))).into_response()
}

fn redirect_err(path: &str, message: &str) -> Response {
    Redirect::to(&format!("{path}?err={}", urlencoding::encode(message))).into_response()
}

fn field<'a>(form: &'a HashMap<String, String>, key: &str) -> &'a str {
    form.get(key).map(|s| s.trim()).unwrap_or("")
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn tag_list(tags: &str) -> Vec<String> {
    tags.split_whitespace().map(str::to_string).collect()
}

fn format_ago(ts: Option<i64>, now_ts: i64) -> String {
    match ts {
        Some(t) if now_ts >= t => format!("{} ago", format_duration(now_ts - t)),
        Some(t) => format_ts(t),
        None => "never".into(),
    }
}

fn format_late_in(check: &Check, now_ts: i64) -> String {
    match check.alert_after {
        Some(t) if t > now_ts => format_duration(t - now_ts),
        Some(t) => format!("overdue by {}", format_duration(now_ts - t)),
        None => "—".into(),
    }
}

/// Checkboxes are named `channel_<id>`: browsers only submit the ones that are
/// ticked, and distinct names sidestep url-encoded forms' lack of repeated keys.
fn selected_channels(form: &HashMap<String, String>) -> Vec<i64> {
    let mut ids: Vec<i64> = form
        .keys()
        .filter_map(|key| key.strip_prefix("channel_")?.parse().ok())
        .collect();
    ids.sort_unstable();
    ids
}

fn new_check_from_form(form: &HashMap<String, String>) -> anyhow::Result<NewCheck> {
    let name = field(form, "name");
    if name.is_empty() {
        anyhow::bail!("name is required");
    }

    let kind = match field(form, "kind") {
        "cron" => CheckKind::Cron,
        _ => CheckKind::Simple,
    };
    let cron_expr = match kind {
        CheckKind::Cron => Some(field(form, "cron").to_string()),
        CheckKind::Simple => None,
    };

    Ok(NewCheck {
        name: name.to_string(),
        description: field(form, "description").to_string(),
        tags: field(form, "tags").to_string(),
        kind,
        period_s: parse_duration(non_empty(field(form, "period")).unwrap_or("1d"))?,
        grace_s: parse_duration(non_empty(field(form, "grace")).unwrap_or("1h"))?,
        cron_expr,
        tz: non_empty(field(form, "tz")).unwrap_or("UTC").to_string(),
    })
}

/// Re-render the form with what was typed, so a rejected cron expression does
/// not cost the operator the rest of their input.
async fn form_with_error(
    state: &AppState,
    form: &HashMap<String, String>,
    existing: Option<&Check>,
    error: &str,
) -> Result<Response, AppError> {
    let (heading, action, cancel_url) = match existing {
        Some(check) => (
            format!("Edit {}", check.name),
            format!("/checks/{}", check.uuid),
            format!("/checks/{}", check.uuid),
        ),
        None => ("New check".to_string(), "/checks".to_string(), "/".to_string()),
    };

    render(CheckFormTemplate {
        flash_ok: None,
        flash_err: Some(error.to_string()),
        heading,
        action,
        cancel_url,
        name: field(form, "name").to_string(),
        description: field(form, "description").to_string(),
        tags: field(form, "tags").to_string(),
        kind: field(form, "kind").to_string(),
        period: field(form, "period").to_string(),
        cron: field(form, "cron").to_string(),
        tz: field(form, "tz").to_string(),
        grace: field(form, "grace").to_string(),
        channels: channel_choices(state, &selected_channels(form)).await?,
    })
}

async fn channel_choices(
    state: &AppState,
    selected: &[i64],
) -> Result<Vec<ChannelChoice>, AppError> {
    let choices = store::list_channels(&state.db)
        .await?
        .into_iter()
        .map(|channel| ChannelChoice {
            selected: selected.contains(&channel.id),
            target: describe_target(&channel),
            kind: channel.kind.to_string(),
            id: channel.id,
            name: channel.name,
        })
        .collect();
    Ok(choices)
}

/// Where a channel sends, for display in listings and forms.
pub fn describe_target(channel: &Channel) -> String {
    let config: serde_json::Value =
        serde_json::from_str(&channel.config).unwrap_or(serde_json::Value::Null);
    match channel.kind {
        ChannelKind::Webhook => config["url"].as_str().unwrap_or("?").to_string(),
        ChannelKind::Ntfy => format!(
            "{}/{}",
            config["server"]
                .as_str()
                .unwrap_or("https://ntfy.sh")
                .trim_end_matches('/'),
            config["topic"].as_str().unwrap_or("?")
        ),
    }
}
