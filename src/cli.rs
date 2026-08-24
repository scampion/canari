use anyhow::Context as _;
use clap::Subcommand;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::model::{CheckKind, format_duration, format_ts, now, parse_duration};
use crate::store::{self, NewCheck};

/// Shorten free-form text (user agents, ping bodies) for table output.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the HTTP server (default when no command is given)
    Serve,

    /// Manage checks without going through the web interface
    #[command(subcommand)]
    Check(CheckCmd),
}

#[derive(Subcommand, Debug)]
pub enum CheckCmd {
    /// Create a check and print its ping URL
    Add {
        /// Display name
        name: String,

        /// Expected interval between pings, e.g. 5m, 1h30m, 1d
        #[arg(long, default_value = "1d", conflicts_with = "cron")]
        period: String,

        /// How late a ping may be before the check goes down
        #[arg(long, default_value = "1h")]
        grace: String,

        /// Cron expression; replaces --period with a schedule
        #[arg(long)]
        cron: Option<String>,

        /// Timezone the cron expression is evaluated in
        #[arg(long, default_value = "UTC")]
        tz: String,

        /// Space-separated tags
        #[arg(long, default_value = "")]
        tags: String,

        #[arg(long, default_value = "")]
        description: String,
    },

    /// List checks and their current state
    Ls,

    /// Show one check in detail, with its recent pings
    Show {
        uuid: String,

        /// Number of pings to display
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },

    /// Delete a check and its ping history
    Rm { uuid: String },
}

pub async fn run(db: &SqlitePool, config: &Config, cmd: &CheckCmd) -> anyhow::Result<()> {
    match cmd {
        CheckCmd::Add {
            name,
            period,
            grace,
            cron,
            tz,
            tags,
            description,
        } => {
            let kind = if cron.is_some() {
                CheckKind::Cron
            } else {
                CheckKind::Simple
            };
            let check = store::create_check(
                db,
                NewCheck {
                    name: name.clone(),
                    description: description.clone(),
                    tags: tags.clone(),
                    kind,
                    period_s: parse_duration(period)?,
                    grace_s: parse_duration(grace)?,
                    cron_expr: cron.clone(),
                    tz: tz.clone(),
                },
            )
            .await?;

            println!("{}", check.name);
            match kind {
                CheckKind::Simple => println!(
                    "  schedule  every {} (grace {})",
                    format_duration(check.period_s),
                    format_duration(check.grace_s)
                ),
                CheckKind::Cron => println!(
                    "  schedule  {} [{}] (grace {})",
                    check.cron_expr.as_deref().unwrap_or(""),
                    check.tz,
                    format_duration(check.grace_s)
                ),
            }
            println!("  ping url  {}/{}", config.ping_base(), check.uuid);
        }

        CheckCmd::Ls => {
            let checks = store::list_checks(db).await?;
            if checks.is_empty() {
                println!("no checks yet — create one with `canari check add <name>`");
                return Ok(());
            }

            let ts = now();
            println!(
                "{:<7} {:<24} {:<12} {:<12} {}",
                "STATUS", "NAME", "LAST PING", "LATE IN", "UUID"
            );
            for check in checks {
                let last = match check.last_ping_at {
                    Some(t) => format!("{} ago", format_duration(ts - t)),
                    None => "never".to_string(),
                };
                let late_in = match check.alert_after {
                    Some(t) if t > ts => format_duration(t - ts),
                    Some(_) => "overdue".to_string(),
                    None => "-".to_string(),
                };
                println!(
                    "{:<7} {:<24} {:<12} {:<12} {}",
                    check.status, check.name, last, late_in, check.uuid
                );
            }
        }

        CheckCmd::Show { uuid, limit } => {
            let check = store::get_check(db, uuid)
                .await?
                .with_context(|| format!("no check with uuid {uuid}"))?;
            let ts = now();

            println!("{}  [{}]", check.name, check.status);
            if !check.description.is_empty() {
                println!("  {}", check.description);
            }
            if !check.tags.is_empty() {
                println!("  tags      {}", check.tags);
            }
            match check.kind {
                CheckKind::Simple => println!(
                    "  schedule  every {} (grace {})",
                    format_duration(check.period_s),
                    format_duration(check.grace_s)
                ),
                CheckKind::Cron => println!(
                    "  schedule  {} [{}] (grace {})",
                    check.cron_expr.as_deref().unwrap_or(""),
                    check.tz,
                    format_duration(check.grace_s)
                ),
            }
            println!("  ping url  {}/{}", config.ping_base(), check.uuid);
            println!("  created   {}", format_ts(check.created_at));
            println!("  updated   {}", format_ts(check.updated_at));
            match check.last_ping_at {
                Some(t) => println!("  last ping {} ({} ago)", format_ts(t), format_duration(ts - t)),
                None => println!("  last ping never"),
            }
            if let Some(started) = check.last_start_at {
                println!("  running   since {}", format_ts(started));
            }
            if let Some(d) = check.last_duration_ms {
                println!("  duration  {}", format_duration(d / 1000));
            }
            match check.alert_after {
                Some(t) if t > ts => println!("  late in   {}", format_duration(t - ts)),
                Some(t) => println!("  overdue   since {}", format_ts(t)),
                None => println!("  late in   -"),
            }

            let pings = store::list_pings(db, check.id, *limit).await?;
            if pings.is_empty() {
                println!("\nno pings yet");
                return Ok(());
            }
            println!(
                "\n{:<5} {:<21} {:<8} {:<5} {:<9} {:<6} {:<16} {}",
                "#", "WHEN", "KIND", "EXIT", "DURATION", "VIA", "FROM", "AGENT"
            );
            for ping in pings {
                println!(
                    "{:<5} {:<21} {:<8} {:<5} {:<9} {:<6} {:<16} {}",
                    ping.n,
                    format_ts(ping.ts),
                    ping.kind,
                    ping.exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".into()),
                    ping.duration_ms
                        .map(|d| format_duration(d / 1000))
                        .unwrap_or_else(|| "-".into()),
                    ping.method.as_deref().unwrap_or("-"),
                    ping.remote_addr.as_deref().unwrap_or("-"),
                    truncate(ping.user_agent.as_deref().unwrap_or("-"), 32),
                );
                if let Some(body) = ping.body.as_deref() {
                    println!(
                        "      body: {}",
                        truncate(body.lines().next().unwrap_or(""), 70)
                    );
                }
            }
        }

        CheckCmd::Rm { uuid } => {
            if store::delete_check(db, uuid).await? {
                println!("deleted {uuid}");
            } else {
                anyhow::bail!("no check with uuid {uuid}");
            }
        }
    }

    Ok(())
}
