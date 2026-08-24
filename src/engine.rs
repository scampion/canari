use std::time::Duration;

use sqlx::SqlitePool;
use tokio::time::MissedTickBehavior;

use crate::model::{Check, Status, now};
use crate::state::AppState;

/// How often late checks are looked for. Alert latency is at most one tick on
/// top of the configured grace.
const TICK: Duration = Duration::from_secs(10);

/// A state change the engine decided on, worth notifying about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Past due but still inside the grace period — recorded, not alerted on.
    Late,
    /// Grace exhausted: this is what raises an alert.
    Down,
}

/// Periodically move late checks through grace and into down.
///
/// Runs until aborted; a failing tick is logged and retried on the next one,
/// since the usual cause (a locked database) is transient.
pub async fn run(state: AppState) {
    let mut ticker = tokio::time::interval(TICK);
    // After a pause (suspended laptop, blocked task), catch up with one tick
    // rather than firing once per missed interval.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        match tick(&state.db, now()).await {
            Ok(events) => {
                for (check, event) in events {
                    match event {
                        Event::Late => {
                            tracing::info!(uuid = %check.uuid, name = %check.name, "check is late")
                        }
                        Event::Down => {
                            tracing::warn!(uuid = %check.uuid, name = %check.name, "check is down");
                            // Spawned: a slow webhook must not delay the next
                            // tick or the checks queued behind this one.
                            crate::notify::spawn(
                                state.clone(),
                                check,
                                crate::notify::Event::Down,
                            );
                        }
                    }
                }
            }
            Err(err) => tracing::error!(?err, "alert loop tick failed"),
        }
    }
}

/// One pass of the alert loop, with the clock passed in so tests can place
/// themselves anywhere on the timeline.
pub async fn tick(db: &SqlitePool, now_ts: i64) -> anyhow::Result<Vec<(Check, Event)>> {
    // Matches the partial index on (alert_after) — paused, down and never-pinged
    // checks are excluded by construction rather than filtered afterwards.
    let late = sqlx::query_as::<_, Check>(
        "SELECT * FROM checks \
         WHERE status IN ('up', 'grace') AND alert_after IS NOT NULL AND alert_after <= ? \
         ORDER BY id",
    )
    .bind(now_ts)
    .fetch_all(db)
    .await?;

    let mut events = Vec::new();

    for check in late {
        let due = check.alert_after.expect("filtered on IS NOT NULL");
        let (next_status, event) = if now_ts >= due + check.grace_s {
            // Also covers up -> down in one step, when the loop was stalled
            // long enough to skip the grace window entirely.
            (Status::Down, Event::Down)
        } else {
            (Status::Grace, Event::Late)
        };

        if next_status == check.status {
            continue;
        }

        // Compare-and-swap on the values the decision was made from: a ping
        // landing between the SELECT and this UPDATE changes both status and
        // alert_after, so the update matches nothing and no bogus alert fires.
        let updated = sqlx::query(
            "UPDATE checks SET status = ?, updated_at = ? \
             WHERE id = ? AND status = ? AND alert_after = ?",
        )
        .bind(next_status)
        .bind(now_ts)
        .bind(check.id)
        .bind(check.status)
        .bind(due)
        .execute(db)
        .await?;

        if updated.rows_affected() == 1 {
            events.push((check, event));
        }
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::model::CheckKind;
    use crate::store::{self, NewCheck, PingInput};

    /// A fixed point in the past: `record_ping` reschedules from the real
    /// clock, so tests that mix it with `tick` need T0 to be behind us.
    const T0: i64 = 1_700_000_000;

    async fn check_with(pool: &SqlitePool, status: Status, alert_after: Option<i64>) -> Check {
        let check = store::create_check(
            pool,
            NewCheck {
                name: "job".into(),
                description: String::new(),
                tags: String::new(),
                kind: CheckKind::Simple,
                period_s: 3600,
                grace_s: 600,
                cron_expr: None,
                tz: "UTC".into(),
            },
        )
        .await
        .unwrap();

        sqlx::query("UPDATE checks SET status = ?, alert_after = ? WHERE id = ?")
            .bind(status)
            .bind(alert_after)
            .bind(check.id)
            .execute(pool)
            .await
            .unwrap();

        store::get_check(pool, &check.uuid).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn up_becomes_grace_once_due() {
        let pool = db::connect_memory().await.unwrap();
        let check = check_with(&pool, Status::Up, Some(T0)).await;

        let events = tick(&pool, T0 + 1).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, Event::Late);

        let after = store::get_check(&pool, &check.uuid).await.unwrap().unwrap();
        assert_eq!(after.status, Status::Grace);
    }

    #[tokio::test]
    async fn grace_becomes_down_once_grace_is_spent() {
        let pool = db::connect_memory().await.unwrap();
        let check = check_with(&pool, Status::Grace, Some(T0)).await;

        let events = tick(&pool, T0 + 600).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, Event::Down);
        assert_eq!(
            store::get_check(&pool, &check.uuid)
                .await
                .unwrap()
                .unwrap()
                .status,
            Status::Down
        );
    }

    #[tokio::test]
    async fn stalled_loop_goes_straight_to_down() {
        let pool = db::connect_memory().await.unwrap();
        let check = check_with(&pool, Status::Up, Some(T0)).await;

        let events = tick(&pool, T0 + 86_400).await.unwrap();
        assert_eq!(events[0].1, Event::Down);
        assert_eq!(
            store::get_check(&pool, &check.uuid)
                .await
                .unwrap()
                .unwrap()
                .status,
            Status::Down
        );
    }

    #[tokio::test]
    async fn nothing_happens_before_the_due_date() {
        let pool = db::connect_memory().await.unwrap();
        check_with(&pool, Status::Up, Some(T0)).await;
        assert!(tick(&pool, T0 - 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn paused_and_down_checks_are_left_alone() {
        let pool = db::connect_memory().await.unwrap();
        check_with(&pool, Status::Paused, Some(T0)).await;
        check_with(&pool, Status::Down, Some(T0)).await;
        assert!(tick(&pool, T0 + 86_400).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_down_check_is_notified_once() {
        let pool = db::connect_memory().await.unwrap();
        check_with(&pool, Status::Up, Some(T0)).await;

        assert_eq!(tick(&pool, T0 + 600).await.unwrap().len(), 1);
        // Second pass: already down, so it is out of the query entirely.
        assert!(tick(&pool, T0 + 700).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_ping_clears_the_late_state() {
        let pool = db::connect_memory().await.unwrap();
        let check = check_with(&pool, Status::Grace, Some(T0)).await;

        store::record_ping(&pool, &check.uuid, PingInput::default())
            .await
            .unwrap()
            .unwrap();

        let after = store::get_check(&pool, &check.uuid).await.unwrap().unwrap();
        assert_eq!(after.status, Status::Up);
        // Rescheduled one period out from the ping, not from the old due date.
        assert!(after.alert_after.unwrap() > T0);
    }
}
