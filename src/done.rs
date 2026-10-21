//! What the user told ghd about threads being Done, which GitHub's API can't tell it (see
//! `docs/adr/0002-inferring-done.md`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::inbox::{Listing, Notification, ThreadId};

/// The user's word on threads, as of the activity each had then (its `updated_at`): Done (marked Done in ghd), or not
/// Done (brought back to ghd's Inbox from the Done list). A word holds until the thread gets newer activity or GitHub
/// lists it unread, as GitHub brings a Done thread back to the inbox then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoneMarks {
    marks: HashMap<ThreadId, DoneMark>,
}

/// The user's word on one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoneMark {
    /// Done, or brought back to the Inbox.
    pub done: bool,
    /// The thread's `updated_at` when the user said so.
    pub as_of: DateTime<Utc>,
}

impl DoneMarks {
    /// Notes that `notification` was marked Done, as it is now.
    pub fn mark_done(&mut self, notification: &Notification) {
        self.mark(notification, true);
    }

    /// Notes that `notification` was brought back to the Inbox, as it is now, whether ghd marked it Done or took it
    /// for Done.
    pub fn mark_not_done(&mut self, notification: &Notification) {
        self.mark(notification, false);
    }

    /// Forgets the word on `thread`: GitHub refused to mark it Done.
    pub fn forget(&mut self, thread: ThreadId) {
        self.marks.remove(&thread);
    }

    /// Puts each thread the user has a word on in the list it belongs in, of `listing` as GitHub listed it: the Done
    /// list or the Inbox. Forgets the words GitHub has overtaken: on threads it lists unread, with newer activity, or
    /// not at all.
    pub fn apply(&mut self, listing: &mut Listing) {
        self.marks
            .retain(|&thread, mark| match listing.get(thread) {
                Some(n) if !n.unread && n.updated_at <= mark.as_of => {
                    listing.put(thread, mark.done);
                    true
                }
                Some(_) | None => false,
            });
    }

    /// The threads with their marks, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (ThreadId, DoneMark)> + '_ {
        self.marks.iter().map(|(&thread, &mark)| (thread, mark))
    }

    fn mark(&mut self, notification: &Notification, done: bool) {
        self.marks.insert(
            notification.thread,
            DoneMark {
                done,
                as_of: notification.updated_at,
            },
        );
    }
}

impl FromIterator<(ThreadId, DoneMark)> for DoneMarks {
    fn from_iter<I: IntoIterator<Item = (ThreadId, DoneMark)>>(marks: I) -> Self {
        Self {
            marks: marks.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::inbox::{Inbox, Reason, SubjectKind};

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

    fn then() -> DateTime<Utc> {
        "2026-10-21T12:00:00Z".parse().expect("valid RFC 3339")
    }

    fn listing(inbox: &[Notification], done: &[Notification]) -> Listing {
        Listing {
            inbox: inbox.iter().cloned().collect(),
            done: done.iter().cloned().collect(),
        }
    }

    fn threads(list: &Inbox) -> Vec<u64> {
        list.iter().map(|n| n.thread.0).collect()
    }

    #[test]
    fn done_threads_go_to_the_done_list_until_new_activity() {
        let mut marks = DoneMarks::default();
        marks.mark_done(&notification(1, then()));

        let mut listed = listing(&[notification(1, then()), notification(2, then())], &[]);
        marks.apply(&mut listed);
        assert_eq!(threads(&listed.inbox), [2]);
        assert_eq!(threads(&listed.done), [1]);
        assert_eq!(marks.iter().count(), 1, "still Done");

        let mut active = listing(&[notification(1, then() + TimeDelta::minutes(5))], &[]);
        marks.apply(&mut active);
        assert_eq!(threads(&active.inbox), [1], "back with new activity");
        assert_eq!(marks.iter().count(), 0, "and the word is forgotten");
    }

    #[test]
    fn unread_threads_show_though_marked_done() {
        let mut marks = DoneMarks::default();
        marks.mark_done(&notification(1, then()));
        // Marked unread on github.com, say: no newer activity, but GitHub's inbox shows it again.
        let unread = Notification {
            unread: true,
            ..notification(1, then())
        };

        let mut listed = listing(&[unread], &[]);
        marks.apply(&mut listed);

        assert_eq!(threads(&listed.inbox), [1]);
        assert_eq!(marks.iter().count(), 0, "no longer Done");
    }

    #[test]
    fn brought_back_threads_stay_in_the_inbox_though_they_seem_done() {
        let mut marks = DoneMarks::default();
        marks.mark_not_done(&notification(1, then()));

        // GitHub's data makes it seem Done: the client put it in the Done list.
        let mut listed = listing(&[], &[notification(1, then())]);
        marks.apply(&mut listed);

        assert_eq!(threads(&listed.inbox), [1]);
        assert!(listed.done.is_empty());
        assert_eq!(marks.iter().count(), 1, "still the user's word");
    }

    #[test]
    fn threads_github_stops_listing_are_forgotten() {
        let mut marks = DoneMarks::default();
        marks.mark_done(&notification(1, then()));

        marks.apply(&mut Listing::default());

        assert_eq!(marks.iter().count(), 0);
    }

    #[test]
    fn refused_done_is_forgotten() {
        let mut marks = DoneMarks::default();
        marks.mark_done(&notification(1, then()));

        marks.forget(ThreadId(1));

        let mut listed = listing(&[notification(1, then())], &[]);
        marks.apply(&mut listed);
        assert_eq!(threads(&listed.inbox), [1]);
    }
}
