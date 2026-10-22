//! Previews: what ghd shows of a Subject without opening it (see `CONTEXT.md`), fetched apart from the Inbox.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::inbox::{Listing, Notification, ThreadId};
use crate::state::SubjectState;

/// What a Preview shows of a Subject: a header, then one thing to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// How GitHub refers to the Subject: `#123` for a pull request or an issue, a release's tag, a commit's
    /// short SHA.
    pub reference: String,
    pub title: String,
    /// `None` for a kind of Subject with no State.
    pub state: Option<SubjectState>,
    /// The author's login; `None` when there's no GitHub account to name (deleted, or a commit by someone without
    /// one).
    pub author: Option<String>,
    pub labels: Vec<String>,
    pub shown: Shown,
}

/// The one thing a Preview shows to read, under its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    /// The latest comment on the Subject, as GitHub points to it.
    Comment(Comment),
    /// The Subject's own text: a pull request's or an issue's description, a release's notes, a commit's message.
    Body(Body),
}

/// A comment on a Subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The author's login; `None` when the account is gone.
    pub author: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The file a comment on a pull request's changes is about; `None` for any other comment.
    pub path: Option<String>,
    /// In Markdown.
    pub body: String,
}

/// A Subject's own text, empty when it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Markdown(String),
    /// Text GitHub shows as it is, like a commit's message.
    Plain(String),
}

/// Where a Notification's Preview stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// Being fetched.
    Fetching,
    Done(Preview),
    /// Why it couldn't be fetched.
    Failed(String),
}

/// The Previews fetched so far, each for its Notification's activity at the time: newer activity calls for a new
/// one. Kept for this run only.
#[derive(Debug, Default)]
pub struct Previews {
    by_thread: HashMap<ThreadId, (DateTime<Utc>, Fetched)>,
}

impl Previews {
    /// Where the Preview of `notification`, with the activity it has now, stands; `None` if it's never been
    /// fetched, or only for older activity.
    pub fn get(&self, notification: &Notification) -> Option<&Fetched> {
        self.by_thread
            .get(&notification.thread)
            .filter(|(as_of, _)| *as_of == notification.updated_at)
            .map(|(_, fetched)| fetched)
    }

    /// Notes that the Preview of `thread`, with the activity it had `as_of`, is being fetched.
    pub fn fetching(&mut self, thread: ThreadId, as_of: DateTime<Utc>) {
        self.by_thread.insert(thread, (as_of, Fetched::Fetching));
    }

    /// Takes in the outcome of fetching the Preview of `thread` with the activity it had `as_of`.
    pub fn fetched(
        &mut self,
        thread: ThreadId,
        as_of: DateTime<Utc>,
        preview: anyhow::Result<Preview>,
    ) {
        let fetched = match preview {
            Ok(preview) => Fetched::Done(preview),
            Err(error) => Fetched::Failed(format!("{error:#}")),
        };
        self.by_thread.insert(thread, (as_of, fetched));
    }

    /// Forgets the Preview of `thread`, so that it's fetched again.
    pub fn forget(&mut self, thread: ThreadId) {
        self.by_thread.remove(&thread);
    }

    /// Forgets `thread`'s Preview if fetching it failed, so that it's tried again.
    pub fn forget_failed(&mut self, thread: ThreadId) {
        if let Some((_, Fetched::Failed(_))) = self.by_thread.get(&thread) {
            self.by_thread.remove(&thread);
        }
    }

    /// Forgets the Previews of threads neither of `listing`'s lists holds.
    pub fn retain_in(&mut self, listing: &Listing) {
        self.by_thread.retain(|&thread, _| listing.contains(thread));
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use chrono::TimeDelta;

    use super::*;
    use crate::inbox::{ApiPaths, Reason, SubjectKind};

    fn notification(thread: u64, updated_at: DateTime<Utc>) -> Notification {
        Notification {
            thread: ThreadId(thread),
            subject_url: format!("https://github.com/wcgw/ghd/issues/{thread}"),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::Issue,
            subject_title: format!("Subject {thread}"),
            reason: Reason::Mention,
            unread: true,
            updated_at,
            api_paths: ApiPaths::default(),
        }
    }

    fn then() -> DateTime<Utc> {
        "2026-10-22T12:00:00Z".parse().expect("valid RFC 3339")
    }

    fn preview() -> Preview {
        Preview {
            reference: "#1".to_owned(),
            title: "Subject 1".to_owned(),
            state: Some(SubjectState::Open),
            author: Some("octocat".to_owned()),
            labels: Vec::new(),
            shown: Shown::Body(Body::Markdown("Hello".to_owned())),
        }
    }

    #[test]
    fn preview_holds_until_newer_activity() {
        let mut previews = Previews::default();
        let seen = notification(1, then());
        previews.fetching(seen.thread, seen.updated_at);
        assert_eq!(previews.get(&seen), Some(&Fetched::Fetching));

        previews.fetched(seen.thread, seen.updated_at, Ok(preview()));
        assert_eq!(previews.get(&seen), Some(&Fetched::Done(preview())));

        let newer = notification(1, then() + TimeDelta::minutes(5));
        assert_eq!(previews.get(&newer), None, "wants fetching again");
    }

    #[test]
    fn failed_preview_is_forgotten_to_try_again() {
        let mut previews = Previews::default();
        let seen = notification(1, then());
        previews.fetched(seen.thread, seen.updated_at, Err(anyhow!("timed out")));
        assert_eq!(
            previews.get(&seen),
            Some(&Fetched::Failed("timed out".to_owned()))
        );

        previews.forget_failed(seen.thread);

        assert_eq!(previews.get(&seen), None);
    }

    #[test]
    fn fetched_preview_is_not_forgotten_as_failed() {
        let mut previews = Previews::default();
        let seen = notification(1, then());
        previews.fetched(seen.thread, seen.updated_at, Ok(preview()));

        previews.forget_failed(seen.thread);

        assert_eq!(previews.get(&seen), Some(&Fetched::Done(preview())));
    }

    #[test]
    fn previews_leave_with_their_notifications() {
        let mut previews = Previews::default();
        let kept = notification(1, then());
        let gone = notification(2, then());
        previews.fetched(kept.thread, kept.updated_at, Ok(preview()));
        previews.fetched(gone.thread, gone.updated_at, Ok(preview()));

        previews.retain_in(&Listing {
            inbox: [kept.clone()].into_iter().collect(),
            done: Default::default(),
        });

        assert!(previews.get(&kept).is_some());
        assert_eq!(previews.get(&gone), None);
    }
}
