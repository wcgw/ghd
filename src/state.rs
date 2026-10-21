//! Subjects' States: where each pull request and issue stands on GitHub (see `CONTEXT.md`), looked up apart from
//! the Inbox.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::inbox::{Inbox, Notification, SubjectKind, ThreadId};

/// Where a Subject stands on GitHub. Only pull requests and issues have one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectState {
    /// An open pull request that's ready for review, or an open issue.
    Open,
    /// An open pull request that's still a draft.
    Draft,
    Merged,
    /// A pull request closed without being merged.
    Closed,
    /// An issue closed as completed.
    Completed,
    /// An issue closed as not planned, or as a duplicate.
    NotPlanned,
}

impl SubjectState {
    const ALL: [Self; 6] = [
        Self::Open,
        Self::Draft,
        Self::Merged,
        Self::Closed,
        Self::Completed,
        Self::NotPlanned,
    ];

    /// The State's name, as in theme files' `[styles.state]` and in the cache.
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Draft => "draft",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Completed => "completed",
            Self::NotPlanned => "not_planned",
        }
    }

    /// The State called `name`; `None` for any other name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.name() == name)
    }
}

/// What's known of a Subject's State, and as of when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// `None` when GitHub had no State to give: it couldn't find the Subject, or the token can't see it.
    pub state: Option<SubjectState>,
    /// The Notification's `updated_at` when the State was looked up: activity after it may have changed the State.
    pub as_of: DateTime<Utc>,
}

/// A Notification whose Subject's State to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    pub thread: ThreadId,
    /// The Subject's page on GitHub's website.
    pub subject_url: String,
    /// The Notification's `updated_at`: the State looked up is as of then.
    pub as_of: DateTime<Utc>,
}

/// The Subjects' States known so far, by thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct States {
    known: HashMap<ThreadId, Known>,
}

impl States {
    /// The State of `notification`'s Subject, as last looked up: possibly from before its latest activity, while the
    /// newer one is looked up. `None` if it's never been looked up, or there's none.
    pub fn get(&self, notification: &Notification) -> Option<SubjectState> {
        self.known.get(&notification.thread)?.state
    }

    /// The Notifications in `inbox` whose Subject's State to look up: pull requests and issues never looked up, or
    /// with activity since.
    pub fn stale(&self, inbox: &Inbox) -> Vec<Lookup> {
        inbox
            .iter()
            .filter(|n| {
                matches!(
                    n.subject_kind,
                    SubjectKind::PullRequest | SubjectKind::Issue
                )
            })
            .filter(|n| {
                self.known
                    .get(&n.thread)
                    .is_none_or(|known| known.as_of < n.updated_at)
            })
            .map(|n| Lookup {
                thread: n.thread,
                subject_url: n.subject_url.clone(),
                as_of: n.updated_at,
            })
            .collect()
    }

    /// Notes what was looked up for `asked`: the States in `found`, and that GitHub had none for the others.
    pub fn found(&mut self, asked: &[Lookup], found: &HashMap<ThreadId, SubjectState>) {
        for lookup in asked {
            self.known.insert(
                lookup.thread,
                Known {
                    state: found.get(&lookup.thread).copied(),
                    as_of: lookup.as_of,
                },
            );
        }
    }

    /// Forgets the States of Subjects whose Notifications `inbox` doesn't hold.
    pub fn retain_in(&mut self, inbox: &Inbox) {
        self.known.retain(|&thread, _| inbox.contains(thread));
    }

    /// What's known, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (ThreadId, Known)> + '_ {
        self.known.iter().map(|(&thread, &known)| (thread, known))
    }
}

impl FromIterator<(ThreadId, Known)> for States {
    fn from_iter<I: IntoIterator<Item = (ThreadId, Known)>>(known: I) -> Self {
        Self {
            known: known.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::inbox::Reason;

    fn notification(thread: u64, kind: SubjectKind, updated_at: DateTime<Utc>) -> Notification {
        Notification {
            thread: ThreadId(thread),
            subject_url: format!("https://github.com/wcgw/ghd/pull/{thread}"),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: kind,
            subject_title: format!("Subject {thread}"),
            reason: Reason::Mention,
            unread: true,
            updated_at,
        }
    }

    fn threads(lookups: &[Lookup]) -> Vec<u64> {
        let mut threads: Vec<u64> = lookups.iter().map(|lookup| lookup.thread.0).collect();
        threads.sort_unstable();
        threads
    }

    #[test]
    fn names_map_back() {
        for state in SubjectState::ALL {
            assert_eq!(SubjectState::from_name(state.name()), Some(state));
        }
        assert_eq!(SubjectState::from_name("reopened"), None);
    }

    #[test]
    fn pull_requests_and_issues_are_looked_up_until_known() {
        let now = Utc::now();
        let inbox: Inbox = [
            notification(1, SubjectKind::PullRequest, now),
            notification(2, SubjectKind::Issue, now),
            notification(3, SubjectKind::Release, now),
            notification(4, SubjectKind::Discussion, now),
        ]
        .into_iter()
        .collect();
        let mut states = States::default();

        let stale = states.stale(&inbox);
        assert_eq!(threads(&stale), [1, 2], "only pull requests and issues");

        states.found(
            &stale,
            &HashMap::from([(ThreadId(1), SubjectState::Merged)]),
        );
        assert!(
            states.stale(&inbox).is_empty(),
            "issue 2 had no State, and that's known too"
        );
        assert_eq!(
            states.get(inbox.get(ThreadId(1)).expect("in the Inbox")),
            Some(SubjectState::Merged)
        );
        assert_eq!(
            states.get(inbox.get(ThreadId(2)).expect("in the Inbox")),
            None
        );
    }

    #[test]
    fn new_activity_makes_a_state_stale_but_it_still_shows() {
        let then = Utc::now();
        let mut states = States::default();
        let before: Inbox = [notification(1, SubjectKind::PullRequest, then)]
            .into_iter()
            .collect();
        states.found(
            &states.stale(&before),
            &HashMap::from([(ThreadId(1), SubjectState::Open)]),
        );

        let after: Inbox = [notification(
            1,
            SubjectKind::PullRequest,
            then + TimeDelta::minutes(5),
        )]
        .into_iter()
        .collect();

        assert_eq!(threads(&states.stale(&after)), [1]);
        assert_eq!(
            states.get(after.get(ThreadId(1)).expect("in the Inbox")),
            Some(SubjectState::Open),
            "until the newer one arrives"
        );
    }

    #[test]
    fn states_leave_with_their_notifications() {
        let now = Utc::now();
        let mut states = States::default();
        let inbox: Inbox = [
            notification(1, SubjectKind::PullRequest, now),
            notification(2, SubjectKind::PullRequest, now),
        ]
        .into_iter()
        .collect();
        states.found(
            &states.stale(&inbox),
            &HashMap::from([
                (ThreadId(1), SubjectState::Open),
                (ThreadId(2), SubjectState::Closed),
            ]),
        );

        states.retain_in(
            &[notification(2, SubjectKind::PullRequest, now)]
                .into_iter()
                .collect(),
        );

        assert_eq!(
            states
                .iter()
                .map(|(thread, _)| thread.0)
                .collect::<Vec<_>>(),
            [2]
        );
    }
}
