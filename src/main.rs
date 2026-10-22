//! ghd: a terminal UI to GitHub, starting with notifications.

mod assessment;
mod cache;
mod change;
mod config;
mod done;
mod fetched;
mod files;
mod filter;
mod github;
mod heatmap;
mod inbox;
mod markdown;
mod preview;
mod search;
mod state;
mod theme;
mod ui;
mod xdg;

use anyhow::Context as _;

use crate::cache::Cache;
use crate::config::Config;
use crate::github::GitHub;
use crate::theme::Theme;
use crate::ui::KindLabels;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Before taking over the terminal, so that a bad configuration or a missing token is reported as a plain error.
    let config = Config::load().await?;
    let theme = Theme::load(config.theme(), xdg::config_dir().as_deref()).await?;
    let github = GitHub::authenticate(config.account()).await?;
    let cache = Cache::for_account(config.account());
    let kind_labels = if config.icons() {
        KindLabels::Icons
    } else {
        KindLabels::Text
    };
    // Before taking over the terminal too: the first time, voight downloads its model, showing its progress.
    let assessor = match config.voight() {
        #[cfg(feature = "voight")]
        Some(device) => Some(assessment::Assessor::start(device).await.context(
            "couldn't fetch voight's model; set voight = \"off\" in the configuration to start without it",
        )?),
        #[cfg(not(feature = "voight"))]
        Some(_) => unreachable!("the configuration can't turn voight on in a ghd built without it"),
        None => None,
    };
    let views = config.into_views();

    let mut terminal = ratatui::try_init().context("setting up the terminal")?;
    let outcome = ui::run(
        &mut terminal,
        &github,
        views,
        theme,
        kind_labels,
        &cache,
        assessor,
    )
    .await;
    ratatui::restore();
    outcome
}
