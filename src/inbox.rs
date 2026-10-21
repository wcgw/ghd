//! The user's Inbox and its Notifications, in ghd's own terms (see `CONTEXT.md`).

use std::cmp::Reverse;
use std::fmt::{self, Write as _};

use chrono::{DateTime, Utc};

/// The user's Notifications that haven't been marked Done, read or not, most recently updated first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inbox {
    /// Sorted by `updated_at`, newest first.
    notifications: Vec<Notification>,
}

impl Inbox {
    /// The Notifications, most recently updated first.
    pub fn iter(&self) -> std::slice::Iter<'_, Notification> {
        self.notifications.iter()
    }

    /// Whether the Inbox holds no Notification at all.
    pub fn is_empty(&self) -> bool {
        self.notifications.is_empty()
    }

    /// Clears Unread on the Notification for `thread`. Returns whether it was unread: `false` when it was already
    /// read or the Inbox doesn't hold it.
    pub fn mark_read(&mut self, thread: ThreadId) -> bool {
        self.find_mut(thread)
            .is_some_and(|n| std::mem::replace(&mut n.unread, false))
    }

    /// Sets Unread on the Notification for `thread`, if the Inbox still holds it.
    pub fn mark_unread(&mut self, thread: ThreadId) {
        if let Some(n) = self.find_mut(thread) {
            n.unread = true;
        }
    }

    /// Takes the Notification for `thread` out of the Inbox; `None` if it doesn't hold one.
    pub fn remove(&mut self, thread: ThreadId) -> Option<Notification> {
        let index = self.notifications.iter().position(|n| n.thread == thread)?;
        Some(self.notifications.remove(index))
    }

    /// Puts `notification` in its place by `updated_at`, replacing any Notification for the same thread.
    pub fn insert(&mut self, notification: Notification) {
        self.remove(notification.thread);
        let index = self
            .notifications
            .partition_point(|n| n.updated_at >= notification.updated_at);
        self.notifications.insert(index, notification);
    }

    fn find_mut(&mut self, thread: ThreadId) -> Option<&mut Notification> {
        self.notifications.iter_mut().find(|n| n.thread == thread)
    }
}

/// Collects Notifications in any order; the Inbox orders them itself.
impl FromIterator<Notification> for Inbox {
    fn from_iter<I: IntoIterator<Item = Notification>>(notifications: I) -> Self {
        let mut notifications: Vec<_> = notifications.into_iter().collect();
        notifications.sort_by_key(|n| Reverse(n.updated_at));
        Self { notifications }
    }
}

/// GitHub's identifier for a Notification's thread, which stays the same as new activity arrives on its Subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

/// An entry in the user's GitHub notifications inbox, about a single Subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The thread this Notification is; what marking it read or Done acts on.
    pub thread: ThreadId,
    /// The Subject's page on GitHub's website.
    pub subject_url: String,
    /// The repository the Subject belongs to, as `owner/name`.
    pub repository: String,
    /// What kind of GitHub item the Subject is.
    pub subject_kind: SubjectKind,
    /// The Subject's title, as GitHub shows it in the inbox.
    pub subject_title: String,
    /// Why GitHub notified the user.
    pub reason: Reason,
    /// The Subject has activity the user hasn't seen yet.
    pub unread: bool,
    /// When the Subject last had activity the user was notified about.
    pub updated_at: DateTime<Utc>,
}

/// The kind of GitHub item a Notification is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectKind {
    PullRequest,
    Issue,
    Release,
    Discussion,
    Commit,
    CheckSuite,
    /// A kind ghd doesn't know, named as GitHub's API sent it.
    Other(String),
}

impl SubjectKind {
    /// The kind's name, as in Filters' `is:` (`pull-request`, `check-suite`, …); a kind ghd doesn't know has the
    /// API's name for it.
    pub fn name(&self) -> &str {
        match self {
            Self::PullRequest => "pull-request",
            Self::Issue => "issue",
            Self::Release => "release",
            Self::Discussion => "discussion",
            Self::Commit => "commit",
            Self::CheckSuite => "check-suite",
            Self::Other(name) => name,
        }
    }

    /// The subject type as the API names it; [`SubjectKind::from_api`] maps it back.
    pub fn api_name(&self) -> &str {
        match self {
            Self::PullRequest => "PullRequest",
            Self::Issue => "Issue",
            Self::Release => "Release",
            Self::Discussion => "Discussion",
            Self::Commit => "Commit",
            Self::CheckSuite => "CheckSuite",
            Self::Other(name) => name,
        }
    }

    /// Maps a subject type as the API names it (`PullRequest`, `Issue`, …); an unknown name becomes
    /// [`SubjectKind::Other`].
    pub fn from_api(name: &str) -> Self {
        match name {
            "PullRequest" => Self::PullRequest,
            "Issue" => Self::Issue,
            "Release" => Self::Release,
            "Discussion" => Self::Discussion,
            "Commit" => Self::Commit,
            "CheckSuite" => Self::CheckSuite,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// Declares [`Reason`] with its mapping to and from the API's names, so each reason is listed exactly once.
macro_rules! reasons {
    ($($(#[$doc:meta])* $variant:ident = $api_name:literal,)+) => {
        /// Why GitHub notified the user about a Subject.
        ///
        /// The known reasons are GitHub's [documented list]. GitHub adds reasons over time; one ghd doesn't know yet
        /// is kept as [`Reason::Other`] rather than rejected.
        ///
        /// [documented list]: https://docs.github.com/en/rest/activity/notifications#about-notification-reasons
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Reason {
            $($(#[$doc])* $variant,)+
            /// A reason ghd doesn't know yet, named as GitHub's API sent it.
            Other(String),
        }

        impl Reason {
            /// Maps a reason as the API names it (`review_requested`, …); an unknown name becomes
            /// [`Reason::Other`].
            pub fn from_api(name: &str) -> Self {
                match name {
                    $($api_name => Self::$variant,)+
                    other => Self::Other(other.to_owned()),
                }
            }

            /// The reason as the API names it; [`Reason::from_api`] maps it back.
            pub fn api_name(&self) -> &str {
                match self {
                    $(Self::$variant => $api_name,)+
                    Self::Other(name) => name,
                }
            }
        }
    };
}

reasons! {
    /// A Copilot cloud agent session finished.
    AgentSessionFinished = "agent_session_finished",
    /// The user was asked to review and approve a deployment.
    ApprovalRequested = "approval_requested",
    /// The user was assigned to the issue.
    Assign = "assign",
    /// The user created the thread.
    Author = "author",
    /// A GitHub Actions workflow run the user triggered completed.
    CiActivity = "ci_activity",
    /// The user commented on the thread.
    Comment = "comment",
    /// The user accepted an invitation to contribute to the repository.
    Invitation = "invitation",
    /// The user subscribed to the thread.
    Manual = "manual",
    /// Organization members asked to enable a feature such as Copilot.
    MemberFeatureRequested = "member_feature_requested",
    /// The user was @mentioned.
    Mention = "mention",
    /// The user, or a team they're on, was asked to review a pull request.
    ReviewRequested = "review_requested",
    /// The user was credited for contributing to a security advisory.
    SecurityAdvisoryCredit = "security_advisory_credit",
    /// GitHub found a security vulnerability in the user's repository.
    SecurityAlert = "security_alert",
    /// The user changed the thread's state, e.g. closed an issue or merged a pull request.
    StateChange = "state_change",
    /// The user is watching the repository.
    Subscribed = "subscribed",
    /// A team the user is on was mentioned.
    TeamMention = "team_mention",
}

impl Reason {
    /// A known reason by the name it shows as (`review-requested`); `None` for any other name.
    pub fn from_name(name: &str) -> Option<Self> {
        match Self::from_api(&name.replace('-', "_")) {
            Self::Other(_) => None,
            known => (known.to_string() == name).then_some(known),
        }
    }
}

/// A known reason shows hyphenated, as in GitHub's inbox filters (`review-requested`); [`Reason::Other`] shows as the
/// API sent it.
impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::Other(name) = self {
            return f.write_str(name);
        }
        for (i, word) in self.api_name().split('_').enumerate() {
            if i > 0 {
                f.write_char('-')?;
            }
            f.write_str(word)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Notification on thread `thread`, updated on 21 October 2026 at `hour`:00 UTC.
    fn notification(thread: u64, hour: u32, unread: bool) -> Notification {
        Notification {
            thread: ThreadId(thread),
            subject_url: format!("https://github.com/wcgw/ghd/issues/{thread}"),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::Issue,
            subject_title: format!("Issue {thread}"),
            reason: Reason::Mention,
            unread,
            updated_at: format!("2026-10-21T{hour:02}:00:00Z")
                .parse()
                .expect("valid RFC 3339"),
        }
    }

    fn threads(inbox: &Inbox) -> Vec<u64> {
        inbox.iter().map(|n| n.thread.0).collect()
    }

    fn unread_threads(inbox: &Inbox) -> Vec<u64> {
        inbox
            .iter()
            .filter(|n| n.unread)
            .map(|n| n.thread.0)
            .collect()
    }

    #[test]
    fn inbox_orders_newest_first() {
        let inbox: Inbox = [
            notification(1, 10, true),
            notification(2, 11, false),
            notification(3, 9, true),
        ]
        .into_iter()
        .collect();

        assert_eq!(threads(&inbox), [2, 1, 3]);
    }

    #[test]
    fn mark_read_reports_whether_anything_changed() {
        let mut inbox: Inbox = [notification(1, 10, true), notification(2, 11, false)]
            .into_iter()
            .collect();

        assert!(inbox.mark_read(ThreadId(1)));
        assert!(!inbox.mark_read(ThreadId(1)), "already read");
        assert!(!inbox.mark_read(ThreadId(2)), "already read");
        assert!(!inbox.mark_read(ThreadId(99)), "not in the Inbox");
        assert!(unread_threads(&inbox).is_empty());

        inbox.mark_unread(ThreadId(1));
        inbox.mark_unread(ThreadId(99));
        assert_eq!(unread_threads(&inbox), [1]);
    }

    #[test]
    fn removed_notification_goes_back_in_its_place() {
        let mut inbox: Inbox = [
            notification(1, 9, true),
            notification(2, 10, true),
            notification(3, 11, true),
        ]
        .into_iter()
        .collect();

        let removed = inbox.remove(ThreadId(2)).expect("thread 2 is in the Inbox");
        assert_eq!(threads(&inbox), [3, 1]);
        assert_eq!(inbox.remove(ThreadId(2)), None);

        inbox.insert(removed);
        assert_eq!(threads(&inbox), [3, 2, 1]);
    }

    #[test]
    fn insert_replaces_the_same_thread() {
        let mut inbox: Inbox = [notification(1, 9, true), notification(2, 10, true)]
            .into_iter()
            .collect();

        inbox.insert(notification(1, 11, false));

        assert_eq!(threads(&inbox), [1, 2]);
        assert_eq!(unread_threads(&inbox), [2]);
    }

    #[test]
    fn known_reason_shows_hyphenated() {
        assert_eq!(
            Reason::from_api("review_requested"),
            Reason::ReviewRequested
        );
        assert_eq!(Reason::ReviewRequested.to_string(), "review-requested");
        assert_eq!(
            Reason::AgentSessionFinished.to_string(),
            "agent-session-finished"
        );
        assert_eq!(Reason::Mention.to_string(), "mention");
    }

    #[test]
    fn unknown_reason_is_kept_as_sent() {
        let reason = Reason::from_api("brand_new_reason");
        assert_eq!(reason, Reason::Other("brand_new_reason".to_owned()));
        assert_eq!(reason.to_string(), "brand_new_reason");
    }

    #[test]
    fn api_names_map_back() {
        let kinds = [
            SubjectKind::PullRequest,
            SubjectKind::Issue,
            SubjectKind::Release,
            SubjectKind::Discussion,
            SubjectKind::Commit,
            SubjectKind::CheckSuite,
            SubjectKind::Other("RepositoryAdvisory".to_owned()),
        ];
        for kind in kinds {
            assert_eq!(SubjectKind::from_api(kind.api_name()), kind);
        }
        for reason in [
            Reason::ReviewRequested,
            Reason::Other("brand_new_reason".to_owned()),
        ] {
            assert_eq!(Reason::from_api(reason.api_name()), reason);
        }
    }

    #[test]
    fn unknown_subject_kind_is_kept_as_sent() {
        assert_eq!(
            SubjectKind::from_api("PullRequest"),
            SubjectKind::PullRequest
        );
        assert_eq!(
            SubjectKind::from_api("RepositoryVulnerabilityAlert"),
            SubjectKind::Other("RepositoryVulnerabilityAlert".to_owned())
        );
    }
}
