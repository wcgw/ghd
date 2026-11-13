//! ghd in the kit's runtime (the kit's D2): the terminal, the event loop and the effects are the kit's; ghd brings
//! its GitHub side, and what it does when it quits: waiting for the changes still on their way to GitHub, and
//! keeping the Inbox for next time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use chrono::Utc;

use crate::app::Ghd;
use crate::cache::{Cache, Snapshot};
use crate::change::{Change, Reply};
use crate::config::Config;
use crate::github::GitHub;
use crate::net::Net;

/// Longest that quitting waits for changes still on their way to GitHub.
const QUIT_GRACE: Duration = Duration::from_secs(10);

/// Runs ghd on the terminal until the user quits, as `config` says, acting as its Account on `github`. Until the
/// first fetch arrives, it shows what `cache` kept from last time; on quitting, it keeps the Inbox there for next
/// time. With voight on, voight assesses the text Previews show, once its model is loaded.
///
/// # Errors
///
/// Fails if voight's model can't be fetched, if the terminal can't be drawn on or read from, if changes made just
/// before quitting were refused or didn't reach GitHub in time, or if the Inbox can't be kept in `cache`. Failing to
/// fetch the Inbox, or a refused change while ghd runs, isn't an error here: the screen says so, and the next poll
/// tries again.
pub async fn run(config: Config, github: GitHub, cache: Cache) -> anyhow::Result<()> {
    let (inbox, mailbox) = wcgw_kit::runtime::channel();
    let ignored = config.ignored().to_vec();
    #[cfg(feature = "voight")]
    let assessor = match config.voight() {
        // Before taking over the terminal: the first time, voight downloads its model, showing its progress.
        Some(device) => {
            let inbox = inbox.clone();
            let started = tokio::task::spawn_blocking(move || {
                crate::assessment::start(device, move |event| {
                    inbox.send(crate::app::Msg::Voight(event));
                })
            })
            .await
            .context("starting voight stopped")?;
            Some(started.context(
                "couldn't fetch voight's model; set voight = \"off\" in the configuration to start without it",
            )?)
        }
        None => None,
    };
    // The configuration can't turn voight on in a ghd built without it.
    #[cfg(not(feature = "voight"))]
    let assessor = config.voight().map(|_| ());
    let voight = assessor.is_some();
    let views = config.into_views();

    let (ghd, mut kit) = Ghd::restored(views, voight, cache.load().await);
    if !ignored.is_empty() {
        kit.warn(format!(
            "config.toml's {} is read no more: :colorscheme and :set icons= go in init.vim",
            ignored.join(" and ")
        ));
    }
    let in_flight = Rc::new(RefCell::new(HashMap::new()));
    let net = Net::new(Arc::new(github), inbox.clone(), Rc::clone(&in_flight));
    #[cfg(feature = "voight")]
    let net = net.with_assessor(assessor);
    let (ghd, _) = wcgw_kit::runtime::run_returning("ghd", ghd, kit, net, (inbox, mailbox))
        .await
        .context("running the terminal")?;

    let mut snapshot = ghd.snapshot(Utc::now());
    let unsettled = ghd.unsettled();
    let mut problems = Vec::new();
    // Dropping the changes still in flight could lose ones the screen already showed as made.
    let waiting: Vec<_> = in_flight
        .borrow_mut()
        .drain()
        .filter(|(id, _)| unsettled.contains(id))
        .map(|(_, task)| task)
        .collect();
    if !waiting.is_empty() {
        let unconfirmed = waiting.len();
        eprintln!("Sending {unconfirmed} change(s) to GitHub…");
        let all = async {
            let mut replies = Vec::with_capacity(waiting.len());
            for task in waiting {
                if let Ok(reply) = task.await {
                    replies.push(reply);
                }
            }
            replies
        };
        match tokio::time::timeout(QUIT_GRACE, all).await {
            Ok(replies) => {
                for Reply { change, accepted } in replies {
                    if let Err(error) = accepted {
                        problems.push(format!("GitHub refused to {change}: {error:#}"));
                        // Undone, so that the Inbox kept for next time doesn't have it.
                        if let Some(snapshot) = &mut snapshot {
                            undo(snapshot, change);
                        }
                    }
                }
            }
            Err(_) => problems.push(format!(
                "GitHub didn't confirm {unconfirmed} change(s) made just before quitting; they may not have been made"
            )),
        }
    }
    if let Some(snapshot) = snapshot
        && let Err(error) = cache.save(&snapshot).await
    {
        problems.push(format!("couldn't keep the Inbox for next time: {error:#}"));
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("\n"));
    }
    Ok(())
}

/// Undoes `change`, which GitHub refused, in `snapshot`.
fn undo(snapshot: &mut Snapshot, change: Change) {
    if let Change::MarkDone(n) = &change {
        snapshot.marks.forget(n.thread);
        snapshot.listing.done.remove(n.thread);
    }
    change.undo(&mut snapshot.listing.inbox);
}
