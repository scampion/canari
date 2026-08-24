use anyhow::Context as _;
use clap::Subcommand;
use serde_json::json;

use crate::model::{ChannelKind, CheckKind, format_duration, format_ts, now, parse_duration};
use crate::notify;
use crate::state::AppState;
use crate::store::{self, NewCheck};

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the HTTP server (default when no command is given)
    Serve,

    /// Manage checks without going through the web interface
    #[command(subcommand)]
    Check(CheckCmd),

    /// Manage notification channels
    #[command(subcommand)]
    Channel(ChannelCmd),

    /// Operator account and instance settings
    #[command(subcommand)]
    Admin(AdminCmd),
}

#[derive(Subcommand, Debug)]
pub enum AdminCmd {
    /// Set the web interface password (prompts when not given inline)
    SetPassword {
        /// Password; omit to be prompted without echo
        #[arg(long)]
        password: Option<String>,
    },
}

pub async fn run_admin(state: &AppState, cmd: &AdminCmd) -> anyhow::Result<()> {
    match cmd {
        AdminCmd::SetPassword { password } => {
            crate::auth::set_password_interactive(&state.db, password.as_deref()).await?;
            println!("password updated — existing sessions were signed out");
        }
    }
    Ok(())
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

        /// Notification channel ids to attach (repeatable)
        #[arg(long = "channel")]
        channels: Vec<i64>,
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

    /// Suspend monitoring for a check
    Pause { uuid: String },

    /// Resume monitoring, giving the job a full period to report in
    Resume { uuid: String },

    /// Send this check's alerts to a channel
    Attach { uuid: String, channel_id: i64 },

    /// Stop sending this check's alerts to a channel
    Detach { uuid: String, channel_id: i64 },

    /// Delete a check and its ping history
    Rm { uuid: String },
}

#[derive(Subcommand, Debug)]
pub enum ChannelCmd {
    /// Add an HTTP webhook channel
    AddWebhook {
        name: String,

        /// Target URL; placeholders such as $NAME and $UUID are substituted
        #[arg(long)]
        url: String,

        #[arg(long, default_value = "POST")]
        method: String,

        /// Extra header as "Name: value" (repeatable)
        #[arg(long = "header")]
        headers: Vec<String>,

        /// Body template. Omit for canari's default JSON payload
        #[arg(long)]
        body: Option<String>,
    },

    /// Add an ntfy channel
    AddNtfy {
        name: String,

        #[arg(long)]
        topic: String,

        #[arg(long, default_value = "https://ntfy.sh")]
        server: String,

        /// Access token for protected topics
        #[arg(long)]
        token: Option<String>,

        /// Fixed priority (1-5); by default 4 for down and 3 for up
        #[arg(long)]
        priority: Option<u8>,
    },

    /// List channels
    Ls,

    /// Send a sample alert through a channel
    Test { id: i64 },

    /// Delete a channel
    Rm { id: i64 },
}

pub async fn run_check(state: &AppState, cmd: &CheckCmd) -> anyhow::Result<()> {
    let db = &state.db;

    match cmd {
        CheckCmd::Add {
            name,
            period,
            grace,
            cron,
            tz,
            tags,
            description,
            channels,
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

            for channel_id in channels {
                store::get_channel(db, *channel_id)
                    .await?
                    .with_context(|| format!("no channel with id {channel_id}"))?;
                store::attach_channel(db, check.id, *channel_id).await?;
            }

            println!("{}", check.name);
            println!("  schedule  {}", check.schedule_summary());
            println!("  ping url  {}/{}", state.config.ping_base(), check.uuid);
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
            println!("  schedule  {}", check.schedule_summary());
            println!("  ping url  {}/{}", state.config.ping_base(), check.uuid);
            println!("  created   {}", format_ts(check.created_at));
            println!("  updated   {}", format_ts(check.updated_at));
            match check.last_ping_at {
                Some(t) => println!(
                    "  last ping {} ({} ago)",
                    format_ts(t),
                    format_duration(ts - t)
                ),
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

            let channels = store::channels_for_check(db, check.id).await?;
            if channels.is_empty() {
                println!("  channels  none — alerts go nowhere");
            } else {
                let names: Vec<String> = channels
                    .iter()
                    .map(|c| format!("{} (#{}, {})", c.name, c.id, c.kind))
                    .collect();
                println!("  channels  {}", names.join(", "));
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

        CheckCmd::Pause { uuid } => {
            if store::pause_check(db, uuid).await? {
                println!("paused {uuid}");
            } else {
                anyhow::bail!("no check with uuid {uuid}");
            }
        }

        CheckCmd::Resume { uuid } => {
            if store::resume_check(db, uuid).await? {
                println!("resumed {uuid}");
            } else {
                anyhow::bail!("no check with uuid {uuid}");
            }
        }

        CheckCmd::Attach { uuid, channel_id } => {
            let check = store::get_check(db, uuid)
                .await?
                .with_context(|| format!("no check with uuid {uuid}"))?;
            let channel = store::get_channel(db, *channel_id)
                .await?
                .with_context(|| format!("no channel with id {channel_id}"))?;
            store::attach_channel(db, check.id, channel.id).await?;
            println!("{} -> {}", check.name, channel.name);
        }

        CheckCmd::Detach { uuid, channel_id } => {
            let check = store::get_check(db, uuid)
                .await?
                .with_context(|| format!("no check with uuid {uuid}"))?;
            if store::detach_channel(db, check.id, *channel_id).await? {
                println!("detached channel #{channel_id} from {}", check.name);
            } else {
                anyhow::bail!("channel #{channel_id} was not attached to {uuid}");
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

pub async fn run_channel(state: &AppState, cmd: &ChannelCmd) -> anyhow::Result<()> {
    let db = &state.db;

    match cmd {
        ChannelCmd::AddWebhook {
            name,
            url,
            method,
            headers,
            body,
        } => {
            let mut header_map = serde_json::Map::new();
            for header in headers {
                let (key, value) = header
                    .split_once(':')
                    .with_context(|| format!("header {header:?} is not \"Name: value\""))?;
                header_map.insert(key.trim().to_string(), json!(value.trim()));
            }

            let config = json!({
                "url": url,
                "method": method,
                "headers": header_map,
                "body": body,
            });
            let channel =
                store::create_channel(db, ChannelKind::Webhook, name, &config.to_string()).await?;
            println!("channel #{} {} ({})", channel.id, channel.name, channel.kind);
        }

        ChannelCmd::AddNtfy {
            name,
            topic,
            server,
            token,
            priority,
        } => {
            let config = json!({
                "server": server,
                "topic": topic,
                "token": token,
                "priority": priority,
            });
            let channel =
                store::create_channel(db, ChannelKind::Ntfy, name, &config.to_string()).await?;
            println!("channel #{} {} ({})", channel.id, channel.name, channel.kind);
            println!("  target  {}/{}", server.trim_end_matches('/'), topic);
        }

        ChannelCmd::Ls => {
            let channels = store::list_channels(db).await?;
            if channels.is_empty() {
                println!("no channels yet — alerts have nowhere to go");
                return Ok(());
            }
            println!(
                "{:<5} {:<9} {:<20} {:<9} {:<12} {}",
                "ID", "KIND", "NAME", "STATE", "ADDED", "TARGET"
            );
            for channel in channels {
                println!(
                    "{:<5} {:<9} {:<20} {:<9} {:<12} {}",
                    channel.id,
                    channel.kind,
                    truncate(&channel.name, 20),
                    if channel.enabled { "enabled" } else { "disabled" },
                    &format_ts(channel.created_at)[..10],
                    truncate(&describe_target(&channel), 50)
                );
            }
        }

        ChannelCmd::Test { id } => {
            let channel = store::get_channel(db, *id)
                .await?
                .with_context(|| format!("no channel with id {id}"))?;
            match notify::send_test(state, &channel).await {
                Ok(()) => println!("sent a test alert through {}", channel.name),
                Err(err) => anyhow::bail!("delivery failed: {err}"),
            }
        }

        ChannelCmd::Rm { id } => {
            if store::delete_channel(db, *id).await? {
                println!("deleted channel #{id}");
            } else {
                anyhow::bail!("no channel with id {id}");
            }
        }
    }

    Ok(())
}

/// Best-effort one-line summary of where a channel sends, for listings.
fn describe_target(channel: &store::Channel) -> String {
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

/// Shorten free-form text (user agents, ping bodies) for table output.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}
