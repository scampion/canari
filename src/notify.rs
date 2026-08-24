use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;

use crate::model::{ChannelKind, Check, CheckKind, Status, format_duration, format_ts, now};
use crate::state::AppState;
use crate::store::{self, Channel};

/// Delivery attempts per channel, and the pauses between them. Only transient
/// failures are retried — a 4xx will not fix itself.
const ATTEMPTS: usize = 3;
const BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

/// What happened to the check. Notifications are sent on transitions only, so
/// there is exactly one of these per state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Down,
    Up,
}

impl Event {
    fn reason(self) -> &'static str {
        match self {
            Event::Down => "down",
            Event::Up => "up",
        }
    }
}

#[derive(Debug, Deserialize)]
struct WebhookConfig {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    /// Placeholder template. Absent means the default JSON payload.
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NtfyConfig {
    #[serde(default = "default_ntfy_server")]
    server: String,
    topic: String,
    /// Access token for protected topics.
    #[serde(default)]
    token: Option<String>,
    /// Overrides the priority chosen from the event.
    #[serde(default)]
    priority: Option<u8>,
}

fn default_ntfy_server() -> String {
    "https://ntfy.sh".to_string()
}

/// Reject a channel configuration at creation time rather than discovering it
/// is unusable when an alert finally needs to go out.
pub fn validate_config(kind: ChannelKind, config: &str) -> anyhow::Result<()> {
    match kind {
        ChannelKind::Webhook => {
            let cfg: WebhookConfig = serde_json::from_str(config)?;
            if let Some(method) = &cfg.method {
                reqwest::Method::from_bytes(method.as_bytes())
                    .map_err(|_| anyhow::anyhow!("invalid HTTP method {method:?}"))?;
            }
            if !cfg.url.starts_with("http://") && !cfg.url.starts_with("https://") {
                anyhow::bail!("webhook url must start with http:// or https://");
            }
        }
        ChannelKind::Ntfy => {
            let cfg: NtfyConfig = serde_json::from_str(config)?;
            if cfg.topic.is_empty() {
                anyhow::bail!("ntfy topic must not be empty");
            }
        }
    }
    Ok(())
}

/// Deliver an event to every channel attached to the check, recording each
/// attempt. Failures are logged, never propagated: a broken channel must not
/// take down the alert loop or a ping request.
pub async fn dispatch(state: &AppState, check: &Check, event: Event) {
    let channels = match store::channels_for_check(&state.db, check.id).await {
        Ok(channels) => channels,
        Err(err) => {
            tracing::error!(?err, uuid = %check.uuid, "cannot load channels");
            return;
        }
    };

    if channels.is_empty() {
        tracing::debug!(uuid = %check.uuid, event = event.reason(), "no channel attached");
        return;
    }

    for channel in channels {
        let error = match deliver(state, &channel, check, event).await {
            Ok(()) => {
                tracing::info!(
                    check = %check.name,
                    channel = %channel.name,
                    event = event.reason(),
                    "notification sent"
                );
                None
            }
            Err(err) => {
                tracing::warn!(
                    check = %check.name,
                    channel = %channel.name,
                    event = event.reason(),
                    error = %err,
                    "notification failed"
                );
                Some(err.to_string())
            }
        };

        if let Err(err) = store::record_notification(
            &state.db,
            check.id,
            channel.id,
            event.reason(),
            error.as_deref(),
        )
        .await
        {
            tracing::error!(?err, "cannot record notification");
        }
    }
}

/// Fire-and-forget delivery: neither the alert loop nor a ping response waits
/// on someone else's HTTP endpoint.
pub fn spawn(state: AppState, check: Check, event: Event) {
    tokio::spawn(async move { dispatch(&state, &check, event).await });
}

/// Same, for callers that only hold a uuid.
pub fn spawn_by_uuid(state: AppState, uuid: String, event: Event) {
    tokio::spawn(async move {
        match store::get_check(&state.db, &uuid).await {
            Ok(Some(check)) => dispatch(&state, &check, event).await,
            Ok(None) => {}
            Err(err) => tracing::error!(?err, uuid, "cannot load check for notification"),
        }
    });
}

/// Send a sample alert, so a channel can be proven working before an incident
/// depends on it.
pub async fn send_test(state: &AppState, channel: &Channel) -> Result<(), SendError> {
    let sample = Check {
        id: 0,
        uuid: "00000000-0000-4000-8000-000000000000".into(),
        name: format!("canari test ({})", channel.name),
        description: "test notification".into(),
        tags: "test".into(),
        kind: CheckKind::Simple,
        period_s: 3600,
        grace_s: 600,
        cron_expr: None,
        tz: "UTC".into(),
        status: Status::Down,
        last_ping_at: Some(now() - 3600),
        last_start_at: None,
        last_duration_ms: None,
        alert_after: None,
        n_pings: 0,
        created_at: now(),
        updated_at: now(),
        badge_token: "sample".into(),
    };
    send_once(state, channel, &sample, Event::Down).await
}

async fn deliver(
    state: &AppState,
    channel: &Channel,
    check: &Check,
    event: Event,
) -> Result<(), SendError> {
    let mut last: Option<SendError> = None;

    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(BACKOFF[attempt - 1]).await;
        }
        match send_once(state, channel, check, event).await {
            Ok(()) => return Ok(()),
            Err(err) if !err.retryable => return Err(err),
            Err(err) => {
                tracing::debug!(channel = %channel.name, attempt, error = %err, "delivery attempt failed");
                last = Some(err);
            }
        }
    }

    Err(last.expect("at least one attempt"))
}

async fn send_once(
    state: &AppState,
    channel: &Channel,
    check: &Check,
    event: Event,
) -> Result<(), SendError> {
    let vars = Vars::new(check, event, &state.config.site_url);

    let request = match channel.kind {
        ChannelKind::Webhook => {
            let cfg: WebhookConfig =
                serde_json::from_str(&channel.config).map_err(SendError::permanent)?;
            let method = match &cfg.method {
                Some(m) => reqwest::Method::from_bytes(m.as_bytes())
                    .map_err(|_| SendError::permanent(format!("invalid HTTP method {m:?}")))?,
                None => reqwest::Method::POST,
            };

            let mut request = state.http.request(method.clone(), vars.render(&cfg.url));
            for (name, value) in &cfg.headers {
                request = request.header(name, vars.render(value));
            }

            match &cfg.body {
                Some(template) => {
                    let body = vars.render(template);
                    if body.is_empty() {
                        request
                    } else {
                        request.body(body)
                    }
                }
                // No template: a JSON document built with serde, so a name
                // containing quotes cannot produce a broken payload.
                None if method_takes_body(&method) => request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(vars.default_json()),
                None => request,
            }
        }

        ChannelKind::Ntfy => {
            let cfg: NtfyConfig =
                serde_json::from_str(&channel.config).map_err(SendError::permanent)?;
            let url = format!("{}/{}", cfg.server.trim_end_matches('/'), cfg.topic);
            let priority = cfg.priority.unwrap_or(match event {
                Event::Down => 4,
                Event::Up => 3,
            });
            let tags = match event {
                Event::Down => "rotating_light",
                Event::Up => "white_check_mark",
            };

            let mut request = state
                .http
                .post(url)
                .header("Title", encode_header(&vars.title))
                .header("Priority", priority.to_string())
                .header("Tags", tags)
                .body(vars.message.clone());

            if let Some(token) = &cfg.token {
                request = request.bearer_auth(token);
            }
            request
        }
    };

    let response = request.send().await.map_err(SendError::from_reqwest)?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let detail = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect::<String>();
    let message = format!("HTTP {status}: {}", detail.trim());

    // 408/429 and 5xx are worth another try; the rest are configuration errors.
    if status.is_server_error() || status == 429 || status == 408 {
        Err(SendError::transient(message))
    } else {
        Err(SendError::permanent(message))
    }
}

fn method_takes_body(method: &reqwest::Method) -> bool {
    matches!(
        *method,
        reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::PATCH
    )
}

/// Flatten an error and its causes into one line, skipping links that merely
/// repeat what the previous one already said.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();

    while let Some(cause) = source {
        let message = cause.to_string();
        if !parts.iter().any(|part| part.contains(&message)) {
            parts.push(message);
        }
        source = cause.source();
    }

    parts.join(": ")
}

/// HTTP headers are latin-1; ntfy reads titles as RFC 2047 when encoded, which
/// keeps accented check names readable instead of mangled.
fn encode_header(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(value);
    format!("=?UTF-8?B?{encoded}?=")
}

#[derive(Debug)]
pub struct SendError {
    message: String,
    retryable: bool,
}

impl SendError {
    fn transient(err: impl fmt::Display) -> Self {
        SendError {
            message: err.to_string(),
            retryable: true,
        }
    }

    /// reqwest's own Display stops at "error sending request for url (…)" and
    /// hides what actually went wrong — refused connection, unknown CA,
    /// unresolvable name. The cause lives further down the source chain, so
    /// unwind it before the message is shown or stored.
    fn from_reqwest(err: reqwest::Error) -> Self {
        let retryable = !err.is_builder();
        SendError {
            message: error_chain(&err),
            retryable,
        }
    }

    fn permanent(err: impl fmt::Display) -> Self {
        SendError {
            message: err.to_string(),
            retryable: false,
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SendError {}

/// Values substituted into user-supplied templates.
struct Vars<'a> {
    check: &'a Check,
    event: Event,
    title: String,
    message: String,
    url: String,
}

impl<'a> Vars<'a> {
    fn new(check: &'a Check, event: Event, site_url: &str) -> Self {
        let ts = now();
        let title = match event {
            Event::Down => format!("{} is DOWN", check.name),
            Event::Up => format!("{} is UP", check.name),
        };

        let last_ping = match check.last_ping_at {
            Some(t) => format!("{} ({} ago)", format_ts(t), format_duration(ts - t)),
            None => "never".to_string(),
        };
        let message = match event {
            Event::Down => format!(
                "Check \"{}\" is down.\nLast ping: {}.\nExpected: {}.",
                check.name,
                last_ping,
                check.schedule_summary()
            ),
            Event::Up => format!(
                "Check \"{}\" is back up.\nLast ping: {}.",
                check.name, last_ping
            ),
        };

        Vars {
            check,
            event,
            title,
            message,
            url: format!("{}/checks/{}", site_url.trim_end_matches('/'), check.uuid),
        }
    }

    /// Replace `$NAME`-style placeholders. `$*_JSON` variants are escaped for
    /// embedding in a JSON template.
    fn render(&self, template: &str) -> String {
        let json_str = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
        template
            .replace("$NAME_JSON", &json_str(&self.check.name))
            .replace("$MESSAGE_JSON", &json_str(&self.message))
            .replace("$TITLE_JSON", &json_str(&self.title))
            .replace("$NAME", &self.check.name)
            .replace("$MESSAGE", &self.message)
            .replace("$TITLE", &self.title)
            .replace("$UUID", &self.check.uuid)
            .replace("$STATUS", self.event.reason())
            .replace("$TAGS", &self.check.tags)
            .replace("$URL", &self.url)
            .replace("$NOW", &format_ts(now()))
    }

    fn default_json(&self) -> String {
        json!({
            "check": self.check.name,
            "uuid": self.check.uuid,
            "status": self.event.reason(),
            "tags": self.check.tags,
            "title": self.title,
            "message": self.message,
            "url": self.url,
            "last_ping": self.check.last_ping_at,
            "schedule": self.check.schedule_summary(),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_check(name: &str) -> Check {
        Check {
            id: 1,
            uuid: "3f1a2b3c-0000-4000-8000-000000000001".into(),
            name: name.into(),
            description: String::new(),
            tags: "prod db".into(),
            kind: CheckKind::Simple,
            period_s: 300,
            grace_s: 60,
            cron_expr: None,
            tz: "UTC".into(),
            status: Status::Down,
            last_ping_at: Some(now() - 600),
            last_start_at: None,
            last_duration_ms: None,
            alert_after: None,
            n_pings: 7,
            created_at: 0,
            updated_at: 0,
            badge_token: "badge".into(),
        }
    }

    #[test]
    fn renders_placeholders() {
        let check = sample_check("backup");
        let vars = Vars::new(&check, Event::Down, "https://canari.example.org/");
        let rendered = vars.render("$NAME|$STATUS|$TAGS|$URL");
        assert_eq!(
            rendered,
            "backup|down|prod db|https://canari.example.org/checks/3f1a2b3c-0000-4000-8000-000000000001"
        );
    }

    #[test]
    fn json_variants_are_escaped() {
        let check = sample_check("say \"hello\"");
        let vars = Vars::new(&check, Event::Down, "https://x.test");
        let body = vars.render("{\"text\": $NAME_JSON}");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["text"], "say \"hello\"");
    }

    #[test]
    fn default_payload_survives_hostile_names() {
        let check = sample_check("weird \"name\" \n with newline");
        let vars = Vars::new(&check, Event::Up, "https://x.test");
        let parsed: serde_json::Value =
            serde_json::from_str(&vars.default_json()).expect("valid JSON");
        assert_eq!(parsed["check"], "weird \"name\" \n with newline");
        assert_eq!(parsed["status"], "up");
    }

    #[test]
    fn error_chain_keeps_the_root_cause() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);

        impl fmt::Display for Wrapper {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("error sending request for url (https://example.test/x)")
            }
        }

        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let wrapped = Wrapper(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        let flattened = error_chain(&wrapped);

        assert!(flattened.starts_with("error sending request for url"));
        assert!(flattened.ends_with("connection refused"));
    }

    #[test]
    fn error_chain_does_not_repeat_itself() {
        #[derive(Debug)]
        struct Echo(&'static str);

        impl fmt::Display for Echo {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.0)
            }
        }

        impl std::error::Error for Echo {}

        assert_eq!(error_chain(&Echo("boom")), "boom");
    }

    #[test]
    fn encodes_non_ascii_headers() {
        assert_eq!(encode_header("backup"), "backup");
        assert_eq!(
            encode_header("sauvegarde réussie"),
            "=?UTF-8?B?c2F1dmVnYXJkZSByw6l1c3NpZQ==?="
        );
    }

    #[test]
    fn validates_channel_configs() {
        assert!(validate_config(ChannelKind::Webhook, r#"{"url":"https://x.test"}"#).is_ok());
        assert!(validate_config(ChannelKind::Webhook, r#"{"url":"ftp://x.test"}"#).is_err());
        assert!(validate_config(ChannelKind::Webhook, r#"{}"#).is_err());
        assert!(
            validate_config(
                ChannelKind::Webhook,
                r#"{"url":"https://x.test","method":"WAT WAT"}"#
            )
            .is_err()
        );
        assert!(validate_config(ChannelKind::Ntfy, r#"{"topic":"alerts"}"#).is_ok());
        assert!(validate_config(ChannelKind::Ntfy, r#"{"topic":""}"#).is_err());
        assert!(
            validate_config(
                ChannelKind::Ntfy,
                r#"{"server":"https://n.test","topic":"a","priority":5}"#
            )
            .is_ok()
        );
    }
}
