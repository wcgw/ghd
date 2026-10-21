//! The user's Inbox and its Notifications, in ghd's own terms (see `CONTEXT.md`).

use std::cmp::Reverse;
use std::fmt::{self, Write as _};

use chrono::{DateTime, Utc};

/// The user's Notifications that haven't been marked Done, read or not, most recently updated first.
#[derive(Debug, Default)]
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

    /// How many Notifications have activity the user hasn't seen yet.
    pub fn unread_count(&self) -> usize {
        self.notifications.iter().filter(|n| n.unread).count()
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

/// An entry in the user's GitHub notifications inbox, about a single Subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
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

            /// The reason as the API names it.
            fn api_name(&self) -> &str {
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

    fn notification(updated_at: &str, unread: bool) -> Notification {
        Notification {
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::Issue,
            subject_title: updated_at.to_owned(),
            reason: Reason::Mention,
            unread,
            updated_at: updated_at.parse().expect("test timestamps are RFC 3339"),
        }
    }

    #[test]
    fn inbox_orders_newest_first() {
        let inbox: Inbox = [
            notification("2026-10-20T00:00:00Z", true),
            notification("2026-10-21T00:00:00Z", false),
            notification("2026-10-19T00:00:00Z", true),
        ]
        .into_iter()
        .collect();

        let titles: Vec<_> = inbox.iter().map(|n| n.subject_title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "2026-10-21T00:00:00Z",
                "2026-10-20T00:00:00Z",
                "2026-10-19T00:00:00Z"
            ]
        );
        assert_eq!(inbox.unread_count(), 2);
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
