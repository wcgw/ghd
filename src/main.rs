//! ghd: a terminal UI to GitHub, starting with notifications.

mod app;
mod assessment;
mod cache;
mod change;
mod config;
mod done;
mod drawn;
mod fetched;
mod files;
mod filter;
mod github;
mod heatmap;
mod hl;
mod inbox;
mod keymap;
mod markdown;
mod net;
mod preview;
mod runtime;
mod state;
mod syntax;
mod xdg;

use crate::cache::Cache;
use crate::config::Config;
use crate::github::GitHub;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Before taking over the terminal, so that a bad configuration or a missing token is reported as a plain error.
    let config = Config::load().await?;
    let github = GitHub::authenticate(config.account()).await?;
    let cache = Cache::for_account(config.account());
    runtime::run(config, github, cache).await
}
