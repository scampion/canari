use std::fmt;

use anyhow::{Context, bail};

/// Current time as unix epoch seconds, the representation used everywhere in
/// the database.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum Status {
    /// Created but never pinged: nothing to expect yet.
    New,
    Up,
    /// Late, but still inside the grace period — not yet an alert.
    Grace,
    Down,
    Paused,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::New => "new",
            Status::Up => "up",
            Status::Grace => "grace",
            Status::Down => "down",
            Status::Paused => "paused",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` rather than `write_str`: these are printed in aligned tables.
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum CheckKind {
    /// A ping is expected every `period_s` seconds.
    Simple,
    /// A ping is expected after every occurrence of `cron_expr` in `tz`.
    Cron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum ChannelKind {
    /// Arbitrary HTTP request, with the payload under the user's control.
    Webhook,
    /// ntfy.sh (or a self-hosted instance): push notification to a topic.
    Ntfy,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Webhook => "webhook",
            ChannelKind::Ntfy => "ntfy",
        }
    }
}

impl fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum PingKind {
    Success,
    /// Job started; the matching success/fail ping measures the duration.
    Start,
    Fail,
    /// Recorded in the ping log without touching the check state.
    Log,
}

impl PingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PingKind::Success => "success",
            PingKind::Start => "start",
            PingKind::Fail => "fail",
            PingKind::Log => "log",
        }
    }
}

impl fmt::Display for PingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Check {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub description: String,
    pub tags: String,
    pub kind: CheckKind,
    pub period_s: i64,
    pub grace_s: i64,
    pub cron_expr: Option<String>,
    pub tz: String,
    pub status: Status,
    pub last_ping_at: Option<i64>,
    pub last_start_at: Option<i64>,
    pub last_duration_ms: Option<i64>,
    pub alert_after: Option<i64>,
    pub n_pings: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Check {
    /// One-line description of what this check expects, shared by the CLI, the
    /// UI and notification messages.
    pub fn schedule_summary(&self) -> String {
        match self.kind {
            CheckKind::Simple => format!(
                "every {} (grace {})",
                format_duration(self.period_s),
                format_duration(self.grace_s)
            ),
            CheckKind::Cron => format!(
                "{} [{}] (grace {})",
                self.cron_expr.as_deref().unwrap_or("?"),
                self.tz,
                format_duration(self.grace_s)
            ),
        }
    }
}

/// Parse a human duration into seconds: `30s`, `5m`, `2h`, `1d`, or a
/// combination such as `1h30m`. A bare number is read as seconds.
pub fn parse_duration(input: &str) -> anyhow::Result<i64> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty duration");
    }

    let mut total: i64 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;

    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            'w' => 604800,
            _ => bail!("invalid duration {input:?}: unexpected {c:?}"),
        };
        let value: i64 = digits
            .parse()
            .with_context(|| format!("invalid duration {input:?}: missing number before {c:?}"))?;
        total += value * unit;
        digits.clear();
        saw_unit = true;
    }

    if !digits.is_empty() {
        // Trailing digits with no unit: seconds.
        total += digits.parse::<i64>()?;
    } else if !saw_unit {
        bail!("invalid duration {input:?}");
    }

    if total <= 0 {
        bail!("duration must be positive");
    }
    Ok(total)
}

/// Render a timestamp as UTC, for CLI and UI output.
pub fn format_ts(ts: i64) -> String {
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%SZ").to_string(),
        None => ts.to_string(),
    }
}

/// Render a duration in seconds compactly, for CLI and UI output.
pub fn format_duration(mut secs: i64) -> String {
    if secs <= 0 {
        return "0s".into();
    }
    let mut parts = Vec::new();
    for (unit, label) in [(86400, "d"), (3600, "h"), (60, "m"), (1, "s")] {
        let n = secs / unit;
        if n > 0 {
            parts.push(format!("{n}{label}"));
            secs -= n * unit;
        }
        if parts.len() == 2 {
            break;
        }
    }
    parts.join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("0").is_err());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(300), "5m");
        assert_eq!(format_duration(5400), "1h30m");
        assert_eq!(format_duration(90061), "1d1h");
    }
}
