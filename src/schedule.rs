use std::str::FromStr;

use anyhow::{Context, anyhow};
use chrono::DateTime;
use chrono_tz::Tz;
use croner::Cron;

use crate::model::{Check, CheckKind};

/// The instant a check becomes late, given the time of its last successful
/// ping. Grace is included, so this is directly the alert threshold.
pub fn next_deadline(check: &Check, from_ts: i64) -> anyhow::Result<i64> {
    match check.kind {
        CheckKind::Simple => Ok(from_ts + check.period_s + check.grace_s),
        CheckKind::Cron => {
            let expr = check
                .cron_expr
                .as_deref()
                .context("cron check has no cron expression")?;
            let next = next_cron_occurrence(expr, &check.tz, from_ts)?;
            Ok(next + check.grace_s)
        }
    }
}

/// First occurrence of `expr` strictly after `from_ts`, as a unix timestamp.
fn next_cron_occurrence(expr: &str, tz: &str, from_ts: i64) -> anyhow::Result<i64> {
    let cron = Cron::from_str(expr).map_err(|e| anyhow!("invalid cron expression {expr:?}: {e}"))?;
    let tz: Tz = tz.parse().map_err(|_| anyhow!("unknown timezone {tz:?}"))?;
    let from = DateTime::from_timestamp(from_ts, 0)
        .context("timestamp out of range")?
        .with_timezone(&tz);

    let next = cron
        .find_next_occurrence(&from, false)
        .map_err(|e| anyhow!("no next occurrence for {expr:?}: {e}"))?;
    Ok(next.timestamp())
}

/// Reject a schedule before it reaches the database, so a broken expression
/// surfaces at creation time rather than on the first ping.
pub fn validate(kind: CheckKind, cron_expr: Option<&str>, tz: &str, period_s: i64) -> anyhow::Result<()> {
    match kind {
        CheckKind::Simple => {
            if period_s <= 0 {
                return Err(anyhow!("period must be positive"));
            }
            Ok(())
        }
        CheckKind::Cron => {
            let expr = cron_expr.context("cron check requires a cron expression")?;
            // Resolving one occurrence exercises both the expression and the
            // timezone, and catches patterns that can never match.
            next_cron_occurrence(expr, tz, crate::model::now())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    fn check(kind: CheckKind, cron_expr: Option<&str>, tz: &str) -> Check {
        Check {
            id: 1,
            uuid: "u".into(),
            name: "n".into(),
            description: String::new(),
            tags: String::new(),
            kind,
            period_s: 3600,
            grace_s: 600,
            cron_expr: cron_expr.map(str::to_string),
            tz: tz.into(),
            status: Status::New,
            last_ping_at: None,
            last_start_at: None,
            last_duration_ms: None,
            alert_after: None,
            n_pings: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn simple_deadline_is_period_plus_grace() {
        let c = check(CheckKind::Simple, None, "UTC");
        assert_eq!(next_deadline(&c, 1_000).unwrap(), 1_000 + 3600 + 600);
    }

    #[test]
    fn cron_deadline_uses_next_occurrence() {
        // 2026-08-24T09:00:00Z, hourly schedule: next occurrence is 10:00Z.
        let c = check(CheckKind::Cron, Some("0 * * * *"), "UTC");
        let from = DateTime::parse_from_rfc3339("2026-08-24T09:00:00Z")
            .unwrap()
            .timestamp();
        let expected = DateTime::parse_from_rfc3339("2026-08-24T10:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(next_deadline(&c, from).unwrap(), expected + 600);
    }

    #[test]
    fn cron_deadline_honours_timezone() {
        // Daily at 02:00 Europe/Paris = 00:00Z in August (UTC+2).
        let c = check(CheckKind::Cron, Some("0 2 * * *"), "Europe/Paris");
        let from = DateTime::parse_from_rfc3339("2026-08-24T09:00:00Z")
            .unwrap()
            .timestamp();
        let expected = DateTime::parse_from_rfc3339("2026-08-25T00:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(next_deadline(&c, from).unwrap(), expected + 600);
    }

    #[test]
    fn rejects_bad_schedules() {
        assert!(validate(CheckKind::Cron, Some("not a cron"), "UTC", 0).is_err());
        assert!(validate(CheckKind::Cron, Some("* * * * *"), "Mars/Olympus", 0).is_err());
        assert!(validate(CheckKind::Cron, None, "UTC", 0).is_err());
        assert!(validate(CheckKind::Simple, None, "UTC", 0).is_err());
        assert!(validate(CheckKind::Simple, None, "UTC", 60).is_ok());
        assert!(validate(CheckKind::Cron, Some("*/5 * * * *"), "UTC", 0).is_ok());
    }
}
