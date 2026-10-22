//! Changes the user makes to Notifications: shown on screen at once, then sent to GitHub, and undone should GitHub
//! refuse them.

use std::fmt;

use tokio::time::Instant;

use crate::github::GitHub;
use crate::inbox::{Inbox, Notification, ThreadId};

/// A change to a Notification that is already on screen and on its way to GitHub. It holds the Notification as it
/// was before the change, so the change can be undone if GitHub refuses it.
#[derive(Debug)]
pub enum Change {
    MarkRead(Notification),
    MarkDone(Notification),
}

impl Change {
    /// Undoes the change in `inbox`, after GitHub refused it.
    pub fn undo(self, inbox: &mut Inbox) {
        match self {
            Self::MarkRead(n) => inbox.mark_unread(n.thread),
            Self::MarkDone(n) => inbox.insert(n),
        }
    }

    fn effect(&self) -> Effect {
        match self {
            Self::MarkRead(n) => Effect::Read(n.thread),
            Self::MarkDone(n) => Effect::Done(n.thread),
        }
    }
}

/// What the change does, e.g. `mark Done "Fix the build"`.
impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MarkRead(n) => write!(f, "mark read \"{}\"", n.subject_title),
            Self::MarkDone(n) => write!(f, "mark Done \"{}\"", n.subject_title),
        }
    }
}

/// GitHub's answer to a [`Change`].
pub struct Reply {
    pub change: Change,
    pub accepted: anyhow::Result<()>,
}

/// Sends `change` to GitHub.
///
/// # Cancel Safety
///
/// Not cancel safe: dropped while waiting for GitHub, the change may or may not have been made.
pub async fn send(github: &GitHub, change: Change) -> Reply {
    let accepted = match &change {
        Change::MarkRead(n) => github.mark_read(n.thread).await,
        Change::MarkDone(n) => github.mark_done(n.thread).await,
    };
    Reply { change, accepted }
}

/// The changes made on screen recently enough that an Inbox fetched from GitHub may not show them yet: those GitHub
/// hasn't confirmed, and those it confirmed only after the fetch began.
#[derive(Debug, Default)]
pub struct RecentChanges {
    /// In the order they were made.
    changes: Vec<Recent>,
}

#[derive(Debug)]
struct Recent {
    effect: Effect,
    /// When GitHub confirmed the change; `None` while it hasn't.
    confirmed_at: Option<Instant>,
}

/// What a change does to the Inbox, without the Notification it was made to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Read(ThreadId),
    Done(ThreadId),
}

impl RecentChanges {
    /// Notes that `change` was made on screen and sent to GitHub.
    pub fn made(&mut self, change: &Change) {
        self.changes.push(Recent {
            effect: change.effect(),
            confirmed_at: None,
        });
    }

    /// Notes that GitHub confirmed `change` at `at`.
    pub fn confirmed(&mut self, change: &Change, at: Instant) {
        if let Some(index) = self.unconfirmed(change) {
            self.changes[index].confirmed_at = Some(at);
        }
    }

    /// Forgets `change`, which GitHub refused.
    pub fn refused(&mut self, change: &Change) {
        if let Some(index) = self.unconfirmed(change) {
            self.changes.remove(index);
        }
    }

    /// Makes the recent changes again in `inbox`, which a fetch that began at `fetch_began` brought. Changes GitHub
    /// confirmed before then are in `inbox` already, and are forgotten.
    pub fn reapply(&mut self, inbox: &mut Inbox, fetch_began: Instant) {
        self.changes
            .retain(|recent| recent.confirmed_at.is_none_or(|at| at >= fetch_began));
        for recent in &self.changes {
            match recent.effect {
                Effect::Read(thread) => {
                    inbox.mark_read(thread);
                }
                Effect::Done(thread) => {
                    inbox.remove(thread);
                }
            }
        }
    }

    /// The oldest unconfirmed change with the same effect as `change`.
    fn unconfirmed(&self, change: &Change) -> Option<usize> {
        let effect = change.effect();
        self.changes
            .iter()
            .position(|recent| recent.effect == effect && recent.confirmed_at.is_none())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeDelta, Utc};

    use super::*;
    use crate::inbox::{ApiPaths, Reason, SubjectKind};

    fn notification(thread: u64) -> Notification {
        Notification {
            thread: ThreadId(thread),
            subject_url: format!("https://github.com/wcgw/ghd/issues/{thread}"),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::Issue,
            subject_title: format!("Issue {thread}"),
            reason: Reason::Mention,
            unread: true,
            // Thread 1 is the newest.
            updated_at: "2026-10-21T12:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("valid RFC 3339")
                - TimeDelta::hours(thread.try_into().expect("small thread ids")),
            api_paths: ApiPaths::default(),
        }
    }

    /// An Inbox fresh from GitHub, holding unread threads 1 to 3.
    fn fetched() -> Inbox {
        (1..=3).map(notification).collect()
    }

    fn threads(inbox: &Inbox) -> Vec<(u64, bool)> {
        inbox.iter().map(|n| (n.thread.0, n.unread)).collect()
    }

    #[test]
    fn unconfirmed_changes_are_made_again() {
        let mut recent = RecentChanges::default();
        recent.made(&Change::MarkDone(notification(1)));
        recent.made(&Change::MarkRead(notification(2)));

        let mut inbox = fetched();
        recent.reapply(&mut inbox, Instant::now());

        assert_eq!(threads(&inbox), [(2, false), (3, true)]);
    }

    #[test]
    fn changes_confirmed_before_the_fetch_began_are_forgotten() {
        let start = Instant::now();
        let mut recent = RecentChanges::default();
        let done = Change::MarkDone(notification(1));
        recent.made(&done);
        recent.confirmed(&done, start);

        let mut inbox = fetched();
        recent.reapply(&mut inbox, start + Duration::from_secs(1));
        assert_eq!(threads(&inbox).len(), 3, "GitHub's answer is trusted");

        let mut inbox = fetched();
        recent.reapply(&mut inbox, start - Duration::from_secs(1));
        assert_eq!(threads(&inbox).len(), 3, "forgotten for good");
    }

    #[test]
    fn changes_confirmed_after_the_fetch_began_are_made_again() {
        let start = Instant::now();
        let mut recent = RecentChanges::default();
        let done = Change::MarkDone(notification(1));
        recent.made(&done);
        recent.confirmed(&done, start + Duration::from_secs(1));

        let mut inbox = fetched();
        recent.reapply(&mut inbox, start);

        assert_eq!(threads(&inbox), [(2, true), (3, true)]);
    }

    #[test]
    fn refused_changes_are_forgotten() {
        let mut recent = RecentChanges::default();
        let done = Change::MarkDone(notification(1));
        recent.made(&done);
        recent.refused(&done);

        let mut inbox = fetched();
        recent.reapply(&mut inbox, Instant::now());

        assert_eq!(threads(&inbox).len(), 3);
    }

    #[test]
    fn undo_puts_the_notification_back() {
        let mut inbox = fetched();
        let removed = inbox.remove(ThreadId(2)).expect("thread 2 is in the Inbox");
        Change::MarkDone(removed).undo(&mut inbox);
        inbox.mark_read(ThreadId(3));
        Change::MarkRead(notification(3)).undo(&mut inbox);

        assert_eq!(threads(&inbox).len(), 3);
        assert!(inbox.iter().all(|n| n.unread));
    }
}
