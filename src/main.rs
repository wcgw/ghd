//! ghd: a terminal UI to GitHub, starting with notifications.

mod change;
mod config;
mod filter;
mod github;
mod inbox;
mod search;
mod ui;

use anyhow::Context as _;

use crate::config::Config;
use crate::github::GitHub;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Before taking over the terminal, so that a bad configuration or a missing token is reported as a plain error.
    let views = Config::load().await?.into_views();
    let github = GitHub::authenticate().await?;

    let mut terminal = ratatui::try_init().context("setting up the terminal")?;
    let outcome = ui::run(&mut terminal, &github, views).await;
    ratatui::restore();
    outcome
}
