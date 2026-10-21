//! The Notifications ghd marked Done. GitHub's API keeps listing Done threads, so ghd hides these itself (see
//! `docs/adr/0002-inferring-done.md`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::inbox::{Inbox, Notification, ThreadId};

/// The threads ghd marked Done, each with the activity it had then (its `updated_at`). Like GitHub, ghd brings a
/// Done thread back to the Inbox when it gets newer activity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkedDone {
    threads: HashMap<ThreadId, DateTime<Utc>>,
}

impl MarkedDone {
    /// Notes that `notification` was marked Done, as it is now.
    pub fn insert(&mut self, notification: &Notification) {
        self.threads
            .insert(notification.thread, notification.updated_at);
    }

    /// Forgets that `thread` was marked Done: GitHub refused it.
    pub fn remove(&mut self, thread: ThreadId) {
        self.threads.remove(&thread);
    }

    /// Takes out of `inbox`, as GitHub listed it, the Notifications marked Done that have had no activity since.
    /// Forgets the others: they're back in the Inbox with newer activity, or GitHub doesn't list them any more.
    pub fn hide_from(&mut self, inbox: &mut Inbox) {
        self.threads.retain(|&thread, &mut done_at| {
            match inbox.get(thread).map(|n| n.updated_at) {
                Some(updated_at) if updated_at <= done_at => {
                    inbox.remove(thread);
                    true
                }
                Some(_) | None => false,
            }
        });
    }

    /// The threads, each with the activity it had when marked Done, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (ThreadId, DateTime<Utc>)> + '_ {
        self.threads
            .iter()
            .map(|(&thread, &done_at)| (thread, done_at))
    }
}

impl FromIterator<(ThreadId, DateTime<Utc>)> for MarkedDone {
    fn from_iter<I: IntoIterator<Item = (ThreadId, DateTime<Utc>)>>(threads: I) -> Self {
        Self {
            threads: threads.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::inbox::{Reason, SubjectKind};

    fn notification(thread: u64, updated_at: DateTime<Utc>) -> Notification {
        Notification {
            thread: ThreadId(thread),
            subject_url: format!("https://github.com/wcgw/ghd/pull/{thread}"),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::PullRequest,
            subject_title: format!("Subject {thread}"),
            reason: Reason::Mention,
            unread: false,
            updated_at,
        }
    }

    fn threads(inbox: &Inbox) -> Vec<u64> {
        inbox.iter().map(|n| n.thread.0).collect()
    }

    #[test]
    fn done_threads_stay_hidden_until_new_activity() {
        let then = "2026-10-21T12:00:00Z".parse().expect("valid RFC 3339");
        let mut done = MarkedDone::default();
        done.insert(&notification(1, then));

        let mut listed: Inbox = [notification(1, then), notification(2, then)]
            .into_iter()
            .collect();
        done.hide_from(&mut listed);
        assert_eq!(threads(&listed), [2]);
        assert_eq!(done.iter().count(), 1, "still Done");

        let mut active: Inbox = [notification(1, then + TimeDelta::minutes(5))]
            .into_iter()
            .collect();
        done.hide_from(&mut active);
        assert_eq!(threads(&active), [1], "back with new activity");
        assert_eq!(done.iter().count(), 0, "and no longer Done");
    }

    #[test]
    fn threads_github_stops_listing_are_forgotten() {
        let then = "2026-10-21T12:00:00Z".parse().expect("valid RFC 3339");
        let mut done = MarkedDone::default();
        done.insert(&notification(1, then));

        done.hide_from(&mut Inbox::default());

        assert_eq!(done.iter().count(), 0);
    }

    #[test]
    fn refused_done_is_forgotten() {
        let then = "2026-10-21T12:00:00Z".parse().expect("valid RFC 3339");
        let mut done = MarkedDone::default();
        done.insert(&notification(1, then));

        done.remove(ThreadId(1));

        let mut listed: Inbox = [notification(1, then)].into_iter().collect();
        done.hide_from(&mut listed);
        assert_eq!(threads(&listed), [1]);
    }
}
