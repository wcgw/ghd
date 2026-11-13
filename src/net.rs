//! ghd's side that talks to GitHub, and to voight: carries out [`Io`] effects on tasks of their own, each sending
//! its answer back through the kit's inbox as a [`Msg`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use wcgw_kit::runtime::{Inbox, Perform};

use crate::app::{Fetch, Ghd, Io, Msg};
use crate::change::{self, Reply};
use crate::github::GitHub;

/// The changes on their way to GitHub, by id: for quitting to wait for them.
pub type InFlight = Rc<RefCell<HashMap<u64, JoinHandle<Reply>>>>;

/// What carries out ghd's effects.
pub struct Net {
    github: Arc<GitHub>,
    /// Where answers go.
    inbox: Inbox<Msg>,
    in_flight: InFlight,
    /// voight, when it's on.
    #[cfg(feature = "voight")]
    assessor: Option<voight::assessor::Assessor<crate::assessment::Key>>,
}

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Net")
            .field("github", &self.github)
            .field("in_flight", &self.in_flight.borrow().len())
            .finish_non_exhaustive()
    }
}

impl Net {
    /// Talking to `github`, answering through `inbox`, the changes sent kept in `in_flight`.
    pub fn new(github: Arc<GitHub>, inbox: Inbox<Msg>, in_flight: InFlight) -> Self {
        Self {
            github,
            inbox,
            in_flight,
            #[cfg(feature = "voight")]
            assessor: None,
        }
    }

    /// Has `assessor` assess the texts ghd asks voight about.
    #[cfg(feature = "voight")]
    #[must_use]
    pub fn with_assessor(
        self,
        assessor: Option<voight::assessor::Assessor<crate::assessment::Key>>,
    ) -> Self {
        Self { assessor, ..self }
    }

    /// Runs `work` on a task of its own, sending what it comes to back.
    fn spawn<F>(&self, work: impl FnOnce(Arc<GitHub>) -> F)
    where
        F: Future<Output = Msg> + Send + 'static,
    {
        let (future, inbox) = (work(Arc::clone(&self.github)), self.inbox.clone());
        tokio::spawn(async move {
            inbox.send(future.await);
        });
    }
}

/// `error` as a line to show.
fn said(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

impl Perform<Ghd> for Net {
    fn perform(&mut self, effect: Io) {
        match effect {
            Io::FetchInbox(fetch) => self.spawn(|github| async move {
                let began = Instant::now();
                let listing = match fetch {
                    Fetch::Whole => github.listing().await.map(Some),
                    Fetch::IfChanged => github.listing_if_changed().await,
                };
                Msg::Fetched {
                    began,
                    listing: listing.map_err(|error| said(&error)),
                    poll_after: github.poll_interval(),
                }
            }),
            Io::LookUp(asked) => self.spawn(|github| async move {
                let subjects: Vec<_> = asked
                    .iter()
                    .map(|lookup| (lookup.thread, lookup.subject_url.as_str()))
                    .collect();
                let found = github
                    .subject_states(&subjects)
                    .await
                    .map_err(|error| said(&error));
                Msg::LookedUp { asked, found }
            }),
            Io::FetchPreview(wanted) => self.spawn(|github| async move {
                Msg::PreviewFetched {
                    preview: github
                        .preview(&wanted.request)
                        .await
                        .map_err(|error| said(&error)),
                    thread: wanted.thread,
                    as_of: wanted.as_of,
                }
            }),
            Io::FetchFiles {
                thread,
                as_of,
                request,
            } => self.spawn(|github| async move {
                Msg::FilesFetched {
                    files: github
                        .changed_files(&request)
                        .await
                        .map_err(|error| said(&error)),
                    thread,
                    as_of,
                }
            }),
            Io::FetchText {
                thread,
                contents_path,
            } => self.spawn(|github| async move {
                Msg::TextFetched {
                    text: github
                        .file_text(&contents_path)
                        .await
                        .map_err(|error| said(&error)),
                    thread,
                    contents_path,
                }
            }),
            Io::Send { id, change } => {
                let (github, inbox) = (Arc::clone(&self.github), self.inbox.clone());
                let task = tokio::spawn(async move {
                    let reply = change::send(&github, change).await;
                    inbox.send(Msg::Settled {
                        id,
                        change: reply.change.clone(),
                        accepted: reply.accepted.as_ref().map_err(said).copied(),
                    });
                    reply
                });
                self.in_flight.borrow_mut().insert(id, task);
            }
            #[cfg(feature = "voight")]
            Io::Assess { key, text } => {
                if let Some(assessor) = &self.assessor {
                    assessor.assess(key, text);
                }
            }
            // Without voight, ghd never asks it anything.
            #[cfg(not(feature = "voight"))]
            Io::Assess { .. } => {}
        }
    }
}
