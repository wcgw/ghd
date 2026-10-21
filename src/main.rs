//! ghd: a terminal UI to GitHub, starting with notifications.

mod github;
mod inbox;
mod ui;

use anyhow::Context as _;

use crate::github::GitHub;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Before taking over the terminal, so a missing token is reported as a plain error.
    let github = GitHub::authenticate().await?;

    let mut terminal = ratatui::try_init().context("setting up the terminal")?;
    let outcome = ui::run(&mut terminal, &github).await;
    ratatui::restore();
    outcome
}
