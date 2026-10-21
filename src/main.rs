//! ghd: a terminal UI to GitHub, starting with notifications.

mod change;
mod config;
mod filter;
mod github;
mod inbox;
mod search;
mod theme;
mod ui;

use anyhow::Context as _;

use crate::config::Config;
use crate::github::GitHub;
use crate::theme::Theme;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Before taking over the terminal, so that a bad configuration or a missing token is reported as a plain error.
    let config = Config::load().await?;
    let theme = Theme::load(config.theme(), config::dir().as_deref()).await?;
    let github = GitHub::authenticate(config.account()).await?;
    let views = config.into_views();

    let mut terminal = ratatui::try_init().context("setting up the terminal")?;
    let outcome = ui::run(&mut terminal, &github, views, theme).await;
    ratatui::restore();
    outcome
}
