//! ghd: a terminal UI to GitHub, starting with notifications.

mod cache;
mod change;
mod config;
mod filter;
mod github;
mod inbox;
mod search;
mod theme;
mod ui;
mod xdg;

use anyhow::Context as _;

use crate::cache::Cache;
use crate::config::Config;
use crate::github::GitHub;
use crate::theme::Theme;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Before taking over the terminal, so that a bad configuration or a missing token is reported as a plain error.
    let config = Config::load().await?;
    let theme = Theme::load(config.theme(), xdg::config_dir().as_deref()).await?;
    let github = GitHub::authenticate(config.account()).await?;
    let cache = Cache::for_account(config.account());
    let views = config.into_views();

    let mut terminal = ratatui::try_init().context("setting up the terminal")?;
    let outcome = ui::run(&mut terminal, &github, views, theme, &cache).await;
    ratatui::restore();
    outcome
}
