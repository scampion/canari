use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::cli::Command;

#[derive(Parser, Debug)]
#[command(
    name = "canari",
    version,
    about = "Dead man's switch monitoring for cron jobs"
)]
pub struct Config {
    /// SQLite database file (created on first run)
    #[arg(long, env = "CANARI_DB", default_value = "canari.db")]
    pub db: PathBuf,

    /// Address to listen on
    #[arg(long, env = "CANARI_LISTEN", default_value = "127.0.0.1:8000")]
    pub listen: SocketAddr,

    /// Public base URL, used to build the ping URLs handed out to clients
    #[arg(long, env = "CANARI_SITE_URL", default_value = "http://localhost:8000")]
    pub site_url: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Config {
    /// Base URL for ping endpoints, without a trailing slash.
    pub fn ping_base(&self) -> String {
        format!("{}/ping", self.site_url.trim_end_matches('/'))
    }

    /// Public badge URL for a check's badge token.
    pub fn badge_url(&self, badge_token: &str) -> String {
        format!(
            "{}/badge/{badge_token}.svg",
            self.site_url.trim_end_matches('/')
        )
    }
}
