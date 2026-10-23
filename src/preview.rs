//! Previews: what ghd shows of a Subject without opening it (see `CONTEXT.md`), fetched apart from the Inbox.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::fetched::Fetched;
use crate::inbox::{Listing, Notification, ThreadId};
use crate::state::SubjectState;

/// What a Preview shows of a Subject: a header, then its body or one of its comments, from the one its Notification's
/// Reason calls for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// How GitHub refers to the Subject: `#123` for a pull request, an issue or a Discussion, a release's tag, a
    /// commit's short SHA.
    pub reference: String,
    pub title: String,
    /// `None` for a kind of Subject with no State.
    pub state: Option<SubjectState>,
    /// The author's login; `None` when there's no GitHub account to name (deleted, or a commit by someone without
    /// one).
    pub author: Option<String>,
    pub labels: Vec<String>,
    /// When it was closed or merged, and by whom; `None` while it's open, and for Subjects that never close.
    pub closed: Option<Closed>,
    /// A pull request's changes and how far along it is; `None` for any other Subject.
    pub pull_request: Option<PullRequestSummary>,
    /// A Discussion's category and whether it's answered; `None` for any other Subject.
    pub discussion: Option<DiscussionSummary>,
    /// The Subject's own text: a pull request's or an issue's description, a Discussion's, a release's notes, a
    /// commit's message.
    pub body: Body,
    /// The comments on the Subject, oldest first, as far back as ghd looks; none without text.
    pub comments: Vec<Comment>,
    /// Where the Preview opens: on the comment its Notification is about, or on the body.
    pub opens_on: Place,
    /// Something to know about what's shown, said above it.
    pub note: Option<Note>,
}

impl Preview {
    /// What's at `place`; the body, for a comment this Preview doesn't have.
    pub fn at(&self, place: Place) -> Shown<'_> {
        match place {
            Place::Comment(index) => self
                .comments
                .get(index)
                .map_or(Shown::Body(&self.body), Shown::Comment),
            Place::Body => Shown::Body(&self.body),
        }
    }

    /// The place after `place`: the next comment, the first after the body; `None` after the last.
    pub fn after(&self, place: Place) -> Option<Place> {
        let next = match place {
            Place::Body => 0,
            Place::Comment(index) => index + 1,
        };
        (next < self.comments.len()).then_some(Place::Comment(next))
    }
}

/// A place in a Preview: the Subject's body, or one of its comments, by its index in [`Preview::comments`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    Body,
    Comment(usize),
}

impl Place {
    /// The place before this one: the previous comment, the body before the first; `None` before the body.
    pub fn before(self) -> Option<Self> {
        match self {
            Self::Body => None,
            Self::Comment(0) => Some(Self::Body),
            Self::Comment(index) => Some(Self::Comment(index - 1)),
        }
    }
}

/// How a Subject came to be closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// Who closed or merged it; `None` when GitHub doesn't say.
    pub by: Option<String>,
    pub at: DateTime<Utc>,
    /// A pull request merged, not just closed.
    pub merged: bool,
}

/// A pull request's changes, and how far along it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSummary {
    /// Lines added.
    pub additions: u64,
    /// Lines removed.
    pub deletions: u64,
    pub changed_files: u64,
    pub commits: u64,
    /// `None` when it has no checks, or they couldn't be looked up.
    pub ci: Option<Ci>,
    /// `None` when the repository requires no review, or it couldn't be looked up.
    pub review: Option<ReviewDecision>,
}

/// How a pull request's checks went, all of them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ci {
    Passing,
    Failing,
    Pending,
}

/// Where a pull request stands with the reviews it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// A Discussion's category, and whether one of its comments is the accepted answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionSummary {
    pub category: String,
    pub answered: bool,
}

/// What a Preview shows to read at a [`Place`], under its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown<'p> {
    Body(&'p Body),
    Comment(&'p Comment),
}

/// A comment on a Subject, or a pull request review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The author's login; `None` when the account is gone.
    pub author: Option<String>,
    /// When it was published: for a review, when it was submitted.
    pub published_at: DateTime<Utc>,
    /// The file a comment on a pull request's changes is about; `None` for any other comment.
    pub path: Option<String>,
    /// In Markdown.
    pub body: String,
    /// What the Preview says of it besides who wrote it; `None` for a comment like any other.
    pub mark: Option<Mark>,
}

/// What a Preview says of a comment, besides who wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// It mentions the user, or one of their teams, in a Preview of a mention.
    MentionsYou,
    /// It's a Discussion's accepted answer.
    Answer,
}

/// Something to know about what a Preview shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The Subject's own text mentions the user: said above it.
    BodyMentionsYou,
    /// Why ghd couldn't tell which comments mention the user: said above whatever's shown.
    MentionNotLookedFor(String),
}

/// A Subject's own text, empty when it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Markdown(String),
    /// Text GitHub shows as it is, like a commit's message.
    Plain(String),
}

/// The Previews fetched so far, each for its Notification's activity at the time: newer activity calls for a new
/// one. Kept for this run only.
#[derive(Debug, Default)]
pub struct Previews {
    by_thread: HashMap<ThreadId, (DateTime<Utc>, Fetched<Box<Preview>>)>,
}

impl Previews {
    /// Where the Preview of `notification`, with the activity it has now, stands; `None` if it's never been
    /// fetched, or only for older activity.
    pub fn get(&self, notification: &Notification) -> Option<&Fetched<Box<Preview>>> {
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
        self.by_thread
            .insert(thread, (as_of, Fetched::from_result(preview.map(Box::new))));
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
    use crate::inbox::{ApiPaths, Inbox, Reason, SubjectKind};

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
            closed: None,
            pull_request: None,
            discussion: None,
            body: Body::Markdown("Hello".to_owned()),
            comments: Vec::new(),
            opens_on: Place::Body,
            note: None,
        }
    }

    fn comment(body: &str) -> Comment {
        Comment {
            author: Some("hubot".to_owned()),
            published_at: then(),
            path: None,
            body: body.to_owned(),
            mark: None,
        }
    }

    #[test]
    fn places_go_from_the_body_through_the_comments() {
        let commented = Preview {
            comments: vec![comment("First"), comment("Second")],
            ..preview()
        };

        assert_eq!(commented.after(Place::Body), Some(Place::Comment(0)));
        assert_eq!(commented.after(Place::Comment(0)), Some(Place::Comment(1)));
        assert_eq!(commented.after(Place::Comment(1)), None);
        assert_eq!(Place::Comment(1).before(), Some(Place::Comment(0)));
        assert_eq!(Place::Comment(0).before(), Some(Place::Body));
        assert_eq!(Place::Body.before(), None);
        assert_eq!(
            commented.at(Place::Comment(1)),
            Shown::Comment(&commented.comments[1])
        );
        assert_eq!(
            commented.at(Place::Comment(2)),
            Shown::Body(&commented.body),
            "a comment it doesn't have"
        );
        assert_eq!(preview().after(Place::Body), None);
    }

    #[test]
    fn preview_holds_until_newer_activity() {
        let mut previews = Previews::default();
        let seen = notification(1, then());
        previews.fetching(seen.thread, seen.updated_at);
        assert_eq!(previews.get(&seen), Some(&Fetched::Fetching));

        previews.fetched(seen.thread, seen.updated_at, Ok(preview()));
        assert_eq!(
            previews.get(&seen),
            Some(&Fetched::Done(Box::new(preview())))
        );

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

        assert_eq!(
            previews.get(&seen),
            Some(&Fetched::Done(Box::new(preview())))
        );
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
            done: Inbox::default(),
        });

        assert!(previews.get(&kept).is_some());
        assert_eq!(previews.get(&gone), None);
    }
}
