use anyhow::Context;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::model::{Check, CheckKind, PingKind, Status, now};
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
                             cron_expr, tz, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'new', ?, ?) \
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

pub async fn delete_check(db: &SqlitePool, uuid: &str) -> anyhow::Result<bool> {
    let result = sqlx::query("DELETE FROM checks WHERE uuid = ?")
        .bind(uuid)
        .execute(db)
        .await?;
    Ok(result.rows_affected() > 0)
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
                alert_after = Some(schedule::next_deadline(&check, ts)?);
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
    Ok(Some(PingOutcome { status, n }))
}
