use anyhow::Context;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::model::{Check, CheckKind, ChannelKind, PingKind, Status, now};
use crate::schedule;

/// Pings kept per check. Old ones are dropped as new ones arrive, so the
/// database stays bounded without a separate cleanup job.
const MAX_PINGS_PER_CHECK: i64 = 100;

/// Largest ping body retained. Anything longer is truncated rather than
/// rejected: a monitoring endpoint should not fail a client over payload size.
const MAX_BODY_BYTES: usize = 100 * 1024;

#[derive(Debug, Clone)]
pub struct NewCheck {
    pub name: String,
    pub description: String,
    pub tags: String,
    pub kind: CheckKind,
    pub period_s: i64,
    pub grace_s: i64,
    pub cron_expr: Option<String>,
    pub tz: String,
}

pub async fn create_check(db: &SqlitePool, new: NewCheck) -> anyhow::Result<Check> {
    schedule::validate(new.kind, new.cron_expr.as_deref(), &new.tz, new.period_s)?;
    if new.grace_s <= 0 {
        anyhow::bail!("grace must be positive");
    }

    let uuid = Uuid::new_v4().to_string();
    let ts = now();

    // `RETURNING *` / `SELECT *`: FromRow maps by column name, so extra columns
    // added by later migrations are simply ignored.
    let check = sqlx::query_as::<_, Check>(
        "INSERT INTO checks (uuid, name, description, tags, kind, period_s, grace_s, \
                             cron_expr, tz, status, created_at, updated_at, badge_token) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'new', ?, ?, lower(hex(randomblob(12)))) \
         RETURNING *",
    )
    .bind(&uuid)
    .bind(&new.name)
    .bind(&new.description)
    .bind(&new.tags)
    .bind(new.kind)
    .bind(new.period_s)
    .bind(new.grace_s)
    .bind(&new.cron_expr)
    .bind(&new.tz)
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
    .context("inserting check")?;

    Ok(check)
}

/// Apply an edit and, when the schedule moved, recompute the deadline from the
/// last ping so the change takes effect without waiting for the next one.
pub async fn update_check(db: &SqlitePool, uuid: &str, new: NewCheck) -> anyhow::Result<Check> {
    schedule::validate(new.kind, new.cron_expr.as_deref(), &new.tz, new.period_s)?;
    if new.grace_s <= 0 {
        anyhow::bail!("grace must be positive");
    }

    let updated = sqlx::query_as::<_, Check>(
        "UPDATE checks SET name = ?, description = ?, tags = ?, kind = ?, period_s = ?, \
                           grace_s = ?, cron_expr = ?, tz = ?, updated_at = ? \
         WHERE uuid = ? RETURNING *",
    )
    .bind(&new.name)
    .bind(&new.description)
    .bind(&new.tags)
    .bind(new.kind)
    .bind(new.period_s)
    .bind(new.grace_s)
    .bind(&new.cron_expr)
    .bind(&new.tz)
    .bind(now())
    .bind(uuid)
    .fetch_optional(db)
    .await?
    .with_context(|| format!("no check with uuid {uuid}"))?;

    if matches!(updated.status, Status::Up | Status::Grace)
        && let Some(last_ping) = updated.last_ping_at
    {
        let alert_after = schedule::next_due(&updated, last_ping)?;
        sqlx::query("UPDATE checks SET alert_after = ? WHERE id = ?")
            .bind(alert_after)
            .bind(updated.id)
            .execute(db)
            .await?;
        return Ok(Check {
            alert_after: Some(alert_after),
            ..updated
        });
    }

    Ok(updated)
}

/// Replace a check's channel set in one transaction, for the edit form.
pub async fn set_check_channels(
    db: &SqlitePool,
    check_id: i64,
    channel_ids: &[i64],
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM check_channels WHERE check_id = ?")
        .bind(check_id)
        .execute(&mut *tx)
        .await?;
    for id in channel_ids {
        sqlx::query("INSERT INTO check_channels (check_id, channel_id) VALUES (?, ?)")
            .bind(check_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn list_checks(db: &SqlitePool) -> anyhow::Result<Vec<Check>> {
    let checks = sqlx::query_as::<_, Check>("SELECT * FROM checks ORDER BY name, id")
        .fetch_all(db)
    .await?;
    Ok(checks)
}

pub async fn get_check(db: &SqlitePool, uuid: &str) -> anyhow::Result<Option<Check>> {
    let check = sqlx::query_as::<_, Check>("SELECT * FROM checks WHERE uuid = ?")
        .bind(uuid)
        .fetch_optional(db)
        .await?;
    Ok(check)
}

/// One row of a check's ping log, most recent first.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Ping {
    pub n: i64,
    pub ts: i64,
    pub kind: PingKind,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub remote_addr: Option<String>,
    pub user_agent: Option<String>,
    pub method: Option<String>,
    pub body: Option<String>,
}

pub async fn list_pings(db: &SqlitePool, check_id: i64, limit: i64) -> anyhow::Result<Vec<Ping>> {
    let pings = sqlx::query_as::<_, Ping>(
        "SELECT * FROM pings WHERE check_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(check_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(pings)
}

/// Suspend monitoring: a paused check is never late, so it drops out of the
/// alert loop's query entirely.
pub async fn pause_check(db: &SqlitePool, uuid: &str) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE checks SET status = 'paused', alert_after = NULL, updated_at = ? WHERE uuid = ?",
    )
    .bind(now())
    .bind(uuid)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Resume monitoring, rescheduling from now rather than from the last ping —
/// after a maintenance window, the job gets a full period to report in.
pub async fn resume_check(db: &SqlitePool, uuid: &str) -> anyhow::Result<bool> {
    let Some(check) = get_check(db, uuid).await? else {
        return Ok(false);
    };

    let (status, alert_after) = match check.last_ping_at {
        Some(_) => (Status::Up, Some(schedule::next_due(&check, now())?)),
        None => (Status::New, None),
    };

    sqlx::query("UPDATE checks SET status = ?, alert_after = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(alert_after)
        .bind(now())
        .bind(check.id)
        .execute(db)
        .await?;
    Ok(true)
}

pub async fn delete_check(db: &SqlitePool, uuid: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM checks WHERE uuid = ?")
        .bind(uuid)
        .execute(db)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Channel {
    pub id: i64,
    pub kind: ChannelKind,
    pub name: String,
    /// Kind-specific JSON, parsed by the notifier.
    pub config: String,
    pub enabled: bool,
    pub created_at: i64,
}

pub async fn create_channel(
    db: &SqlitePool,
    kind: ChannelKind,
    name: &str,
    config: &str,
) -> anyhow::Result<Channel> {
    // Parsing here means a malformed channel can never reach the alert path,
    // where the failure would be silent and delayed.
    crate::notify::validate_config(kind, config)?;

    let channel = sqlx::query_as::<_, Channel>(
        "INSERT INTO channels (kind, name, config, created_at) VALUES (?, ?, ?, ?) RETURNING *",
    )
    .bind(kind)
    .bind(name)
    .bind(config)
    .bind(now())
    .fetch_one(db)
    .await?;
    Ok(channel)
}

pub async fn list_channels(db: &SqlitePool) -> anyhow::Result<Vec<Channel>> {
    let channels = sqlx::query_as::<_, Channel>("SELECT * FROM channels ORDER BY id")
        .fetch_all(db)
        .await?;
    Ok(channels)
}

pub async fn get_channel(db: &SqlitePool, id: i64) -> anyhow::Result<Option<Channel>> {
    let channel = sqlx::query_as::<_, Channel>("SELECT * FROM channels WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(channel)
}

pub async fn delete_channel(db: &SqlitePool, id: i64) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM channels WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Channels an alert for this check should go to.
pub async fn channels_for_check(db: &SqlitePool, check_id: i64) -> anyhow::Result<Vec<Channel>> {
    let channels = sqlx::query_as::<_, Channel>(
        "SELECT c.* FROM channels c \
         JOIN check_channels cc ON cc.channel_id = c.id \
         WHERE cc.check_id = ? AND c.enabled = 1 \
         ORDER BY c.id",
    )
    .bind(check_id)
    .fetch_all(db)
    .await?;
    Ok(channels)
}

pub async fn attach_channel(db: &SqlitePool, check_id: i64, channel_id: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO check_channels (check_id, channel_id) VALUES (?, ?) \
         ON CONFLICT DO NOTHING",
    )
    .bind(check_id)
    .bind(channel_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn detach_channel(
    db: &SqlitePool,
    check_id: i64,
    channel_id: i64,
) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM check_channels WHERE check_id = ? AND channel_id = ?")
        .bind(check_id)
        .bind(channel_id)
        .execute(db)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn record_notification(
    db: &SqlitePool,
    check_id: i64,
    channel_id: i64,
    reason: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO notifications (check_id, channel_id, ts, reason, status, error) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(check_id)
    .bind(channel_id)
    .bind(now())
    .bind(reason)
    .bind(if error.is_some() { "failed" } else { "sent" })
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub read_only: bool,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// Mint an API key. The secret is returned once and never stored in the clear.
pub async fn create_api_key(
    db: &SqlitePool,
    name: &str,
    read_only: bool,
) -> anyhow::Result<(ApiKey, String)> {
    let secret = format!("ck_{}", crate::auth::generate_secret());

    let key = sqlx::query_as::<_, ApiKey>(
        "INSERT INTO api_keys (name, hash, read_only, created_at) VALUES (?, ?, ?, ?) \
         RETURNING id, name, read_only, created_at, last_used_at",
    )
    .bind(name)
    .bind(crate::auth::hash_secret(&secret))
    .bind(read_only)
    .bind(now())
    .fetch_one(db)
    .await?;

    Ok((key, secret))
}

pub async fn list_api_keys(db: &SqlitePool) -> anyhow::Result<Vec<ApiKey>> {
    let keys = sqlx::query_as::<_, ApiKey>(
        "SELECT id, name, read_only, created_at, last_used_at FROM api_keys ORDER BY id",
    )
    .fetch_all(db)
    .await?;
    Ok(keys)
}

pub async fn delete_api_key(db: &SqlitePool, id: i64) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM api_keys WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Look up a presented key by hash, recording the use.
pub async fn authenticate_api_key(
    db: &SqlitePool,
    presented: &str,
) -> anyhow::Result<Option<ApiKey>> {
    let hash = crate::auth::hash_secret(presented);
    let key = sqlx::query_as::<_, ApiKey>(
        "UPDATE api_keys SET last_used_at = ? WHERE hash = ? \
         RETURNING id, name, read_only, created_at, last_used_at",
    )
    .bind(now())
    .bind(hash)
    .fetch_optional(db)
    .await?;
    Ok(key)
}

/// What the client told us about this ping, beyond its kind.
#[derive(Debug, Default, Clone)]
pub struct PingInput {
    pub kind: Option<PingKind>,
    pub exit_code: Option<i64>,
    pub remote_addr: Option<String>,
    pub user_agent: Option<String>,
    pub method: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct PingOutcome {
    pub status: Status,
    /// Status before this ping, so the caller can spot a recovery or a fresh
    /// failure and notify exactly once.
    pub previous: Status,
    pub n: i64,
}

/// Record a ping and move the check to its new state, in one transaction.
///
/// Returns `None` when no check carries that uuid — the caller turns that into
/// a 404 without leaking whether the uuid ever existed.
pub async fn record_ping(
    db: &SqlitePool,
    uuid: &str,
    input: PingInput,
) -> anyhow::Result<Option<PingOutcome>> {
    let kind = input.kind.unwrap_or(PingKind::Success);
    let mut tx = db.begin().await?;

    let check = sqlx::query_as::<_, Check>("SELECT * FROM checks WHERE uuid = ?")
        .bind(uuid)
        .fetch_optional(&mut *tx)
        .await?;

    let Some(check) = check else {
        return Ok(None);
    };

    let ts = now();
    let mut status = check.status;
    let mut alert_after = check.alert_after;
    let mut last_ping_at = check.last_ping_at;
    let mut last_start_at = check.last_start_at;
    let mut last_duration_ms = check.last_duration_ms;
    // Only recorded on the ping that closes a start/finish pair.
    let mut duration_ms: Option<i64> = None;

    match kind {
        PingKind::Success | PingKind::Fail => {
            last_ping_at = Some(ts);
            duration_ms = check.last_start_at.map(|start| (ts - start).max(0) * 1000);
            if duration_ms.is_some() {
                last_duration_ms = duration_ms;
            }
            last_start_at = None;

            if kind == PingKind::Success {
                // A ping on a paused check resumes it: whoever is still running
                // the job clearly expects it to be monitored.
                status = Status::Up;
                alert_after = Some(schedule::next_due(&check, ts)?);
            } else {
                // Already down; nothing left to be late for.
                status = Status::Down;
                alert_after = None;
            }
        }
        PingKind::Start => {
            // Does not change the schedule: the job is late when its *result*
            // fails to arrive, not its announcement.
            last_start_at = Some(ts);
        }
        PingKind::Log => {}
    }

    let n = check.n_pings + 1;
    let body = input.body.map(|mut b| {
        if b.len() > MAX_BODY_BYTES {
            // Cut on a char boundary so the stored text stays valid UTF-8.
            let mut end = MAX_BODY_BYTES;
            while end > 0 && !b.is_char_boundary(end) {
                end -= 1;
            }
            b.truncate(end);
        }
        b
    });

    sqlx::query(
        "INSERT INTO pings (check_id, n, ts, kind, exit_code, duration_ms, remote_addr, \
                            user_agent, method, body) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(check.id)
    .bind(n)
    .bind(ts)
    .bind(kind)
    .bind(input.exit_code)
    .bind(duration_ms)
    .bind(&input.remote_addr)
    .bind(&input.user_agent)
    .bind(&input.method)
    .bind(&body)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE checks SET status = ?, last_ping_at = ?, last_start_at = ?, \
                           last_duration_ms = ?, alert_after = ?, n_pings = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(status)
    .bind(last_ping_at)
    .bind(last_start_at)
    .bind(last_duration_ms)
    .bind(alert_after)
    .bind(n)
    .bind(ts)
    .bind(check.id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "DELETE FROM pings WHERE check_id = ? AND id NOT IN \
             (SELECT id FROM pings WHERE check_id = ? ORDER BY id DESC LIMIT ?)",
    )
    .bind(check.id)
    .bind(check.id)
    .bind(MAX_PINGS_PER_CHECK)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(PingOutcome {
        status,
        previous: check.status,
        n,
    }))
}
