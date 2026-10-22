//! Previews of Subjects: from the REST API, and from GraphQL what REST can't give in one go (the comments to look
//! through for a mention, a pull request's checks and review decision, and Discussions).

use std::cmp::Reverse;
use std::sync::MutexGuard;

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use futures_util::future::OptionFuture;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::{Account, GitHub, below_repos, issue_state, pull_request_state};
use crate::inbox::{Notification, Reason};
use crate::markdown;
use crate::preview::{
    Body, Ci, Closed, Comment, DiscussionSummary, Note, Preview, PullRequestSummary,
    ReviewDecision, Shown, Why,
};

/// What GraphQL adds to a pull request's or an issue's Preview: `$mention` asks for the comments to look through
/// for a mention.
const EXTRAS_QUERY: &str = "query($url: URI!, $mention: Boolean!) { resource(url: $url) { \
     ... on PullRequest { statusCheckRollup { state } reviewDecision \
       comments(last: 30) @include(if: $mention) { nodes { ...Said } } \
       reviews(last: 10) @include(if: $mention) { nodes { ...Said comments(last: 10) { nodes { ...Said path } } } } } \
     ... on Issue { comments(last: 30) @include(if: $mention) { nodes { ...Said } } } } } \
     fragment Said on Comment { author { login } body createdAt }";

/// A Discussion's Preview, all from GraphQL: the REST API has no Discussions.
const DISCUSSION_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { \
     repository(owner: $owner, name: $name) { discussion(number: $number) { \
       number title body author { login } category { name } labels(first: 20) { nodes { name } } \
       closed closedAt isAnswered answer { ...Said } \
       comments(last: 30) { nodes { ...Said replies(last: 5) { nodes { ...Said } } } } } } } \
     fragment Said on Comment { author { login } body createdAt }";

/// What to fetch for a Notification's Preview, and what to show of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    subject: PreviewSubject,
    /// The Subject's path in the REST API.
    subject_path: String,
    /// The Subject's page on GitHub's website, by which GraphQL finds a pull request or an issue.
    page: String,
    /// The latest comment's path in the REST API, if GitHub points to a comment.
    comment_path: Option<String>,
    wanted: Wanted,
}

/// The kinds of Subject ghd previews.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PreviewSubject {
    PullRequest,
    Issue,
    Release,
    Commit,
    Discussion {
        owner: String,
        name: String,
        number: u64,
    },
}

/// What a Preview shows under its header, by the Notification's Reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// The Subject's description: what a review request or an assignment is about.
    Description,
    /// The latest comment to mention the user, or one of their teams (`team`); or the description, if it does.
    Mention { team: bool },
    /// The latest comment, or else the Subject's body.
    LatestComment,
}

impl Wanted {
    fn for_reason(reason: &Reason) -> Self {
        match reason {
            Reason::ReviewRequested | Reason::Assign => Self::Description,
            Reason::Mention => Self::Mention { team: false },
            Reason::TeamMention => Self::Mention { team: true },
            // Reasons GitHub adds later too.
            _ => Self::LatestComment,
        }
    }
}

impl PreviewRequest {
    /// What to fetch for `notification`'s Preview; `None` when ghd can't preview its Subject: a check suite, or a
    /// Subject GitHub gives no path for.
    pub fn for_notification(notification: &Notification) -> Option<Self> {
        let subject_path = notification.api_paths.subject.as_deref()?;
        let subject = match below_repos(subject_path).as_slice() {
            [_, _, "pulls", _] => PreviewSubject::PullRequest,
            [_, _, "issues", _] => PreviewSubject::Issue,
            [_, _, "releases", _] => PreviewSubject::Release,
            [_, _, "commits", _] => PreviewSubject::Commit,
            [owner, name, "discussions", number] => PreviewSubject::Discussion {
                owner: (*owner).to_owned(),
                name: (*name).to_owned(),
                number: number.parse().ok()?,
            },
            _ => return None,
        };
        // GitHub points to the Subject itself when there's no comment yet.
        let comment_path = notification
            .api_paths
            .latest_comment
            .clone()
            .filter(|path| {
                matches!(
                    below_repos(path).as_slice(),
                    [_, _, "issues" | "pulls", "comments", _]
                        | [_, _, "comments", _]
                        | [_, _, "pulls", _, "reviews", _]
                )
            });
        Some(Self {
            subject,
            subject_path: subject_path.to_owned(),
            page: notification.subject_url.clone(),
            comment_path,
            wanted: Wanted::for_reason(&notification.reason),
        })
    }
}

/// Who a mention of the user names, as far as it's been looked up this run: looked up once, when first needed.
#[derive(Debug)]
pub(super) struct Mentionable {
    /// The user's login, or why it couldn't be looked up.
    login: Option<Result<String, String>>,
    /// The user's teams as `org/team`, or why they couldn't be looked up.
    teams: Option<Result<Vec<String>, String>>,
}

impl Mentionable {
    /// What's known of who mentions name, acting as `account`: its login, if the configuration says.
    pub(super) fn of(account: &Account) -> Self {
        Self {
            login: account.user.clone().map(Ok),
            teams: None,
        }
    }
}

impl GitHub {
    /// The Preview `request` asks for, its parts fetched at the same time. Only reads: the Notification stays
    /// unread, and in the Inbox.
    ///
    /// # Errors
    ///
    /// Fails if GitHub can't be reached, or doesn't give the Subject or the latest comment. What GraphQL adds to a
    /// pull request or an issue is left out if it fails, a mention noting why; a Discussion, all from GraphQL,
    /// fails with it.
    ///
    /// # Cancel Safety
    ///
    /// Cancel safe: it only reads.
    pub async fn preview(&self, request: &PreviewRequest) -> anyhow::Result<Preview> {
        if let PreviewSubject::Discussion {
            owner,
            name,
            number,
        } = &request.subject
        {
            return self
                .discussion_preview(owner, name, *number, request.wanted)
                .await;
        }
        let latest = OptionFuture::from(
            request
                .comment_path
                .as_deref()
                .filter(|_| request.wanted != Wanted::Description)
                .map(|path| self.get::<ApiComment>(path)),
        );
        let (subject, latest, extras, handles) = tokio::join!(
            self.preview_subject(request),
            latest,
            self.extras(request),
            OptionFuture::from(self.handles_for(request.wanted)),
        );
        let mut preview = subject?;
        let latest = latest.transpose()?.map(|comment| comment.into_comment());
        let extras = extras.map(|extras| extras.map_err(|error| format!("{error:#}")));
        if let (Some(summary), Some(Ok(extras))) = (&mut preview.pull_request, &extras) {
            summary.ci = extras.ci;
            summary.review = extras.review;
        }
        match (request.wanted, handles) {
            (Wanted::Mention { .. }, Some(handles)) => {
                let comments = extras
                    .unwrap_or_else(|| Ok(Extras::default()))
                    .map(|extras| extras.comments);
                show_mention(&mut preview, handles, comments, latest);
            }
            (Wanted::LatestComment | Wanted::Mention { .. }, _) => {
                if let Some(latest) = latest {
                    preview.shown = Shown::Comment(latest);
                }
            }
            (Wanted::Description, _) => {}
        }
        Ok(preview)
    }

    /// The Preview of `request`'s Subject, from the REST API alone, showing its body.
    async fn preview_subject(&self, request: &PreviewRequest) -> anyhow::Result<Preview> {
        let path = request.subject_path.as_str();
        let labels = |labels: Vec<ApiLabel>| labels.into_iter().map(|label| label.name).collect();
        let preview = |reference, title, author: Option<ApiUser>, body| Preview {
            reference,
            title,
            state: None,
            author: author.map(|user| user.login),
            labels: Vec::new(),
            closed: None,
            pull_request: None,
            discussion: None,
            shown: Shown::Body(body),
            note: None,
        };
        Ok(match &request.subject {
            PreviewSubject::PullRequest => {
                let pull: ApiPullRequest = self.get(path).await?;
                let closed = match (pull.merged, pull.state.as_str()) {
                    (true, _) => pull.merged_at.or(pull.closed_at).map(|at| Closed {
                        by: pull.merged_by.map(|user| user.login),
                        at,
                        merged: true,
                    }),
                    (false, "closed") => pull.closed_at.map(|at| Closed {
                        by: None,
                        at,
                        merged: false,
                    }),
                    (false, _) => None,
                };
                Preview {
                    state: pull_request_state(&pull.state, pull.draft, pull.merged),
                    labels: labels(pull.labels),
                    closed,
                    pull_request: Some(PullRequestSummary {
                        additions: pull.additions,
                        deletions: pull.deletions,
                        changed_files: pull.changed_files,
                        commits: pull.commits,
                        ci: None,
                        review: None,
                    }),
                    ..preview(
                        format!("#{}", pull.number),
                        pull.title,
                        pull.user,
                        Body::Markdown(pull.body.unwrap_or_default()),
                    )
                }
            }
            PreviewSubject::Issue => {
                let issue: ApiIssue = self.get(path).await?;
                let closed = (issue.state == "closed")
                    .then_some(issue.closed_at)
                    .flatten()
                    .map(|at| Closed {
                        by: issue.closed_by.map(|user| user.login),
                        at,
                        merged: false,
                    });
                Preview {
                    state: issue_state(&issue.state, issue.state_reason.as_deref()),
                    labels: labels(issue.labels),
                    closed,
                    ..preview(
                        format!("#{}", issue.number),
                        issue.title,
                        issue.user,
                        Body::Markdown(issue.body.unwrap_or_default()),
                    )
                }
            }
            PreviewSubject::Release => {
                let release: ApiRelease = self.get(path).await?;
                let title = release
                    .name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| release.tag_name.clone());
                preview(
                    release.tag_name,
                    title,
                    release.author,
                    Body::Markdown(release.body.unwrap_or_default()),
                )
            }
            PreviewSubject::Commit => {
                let commit: ApiCommit = self.get(path).await?;
                let title = commit
                    .commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                preview(
                    commit.sha.chars().take(7).collect(),
                    title,
                    commit.author,
                    Body::Plain(commit.commit.message),
                )
            }
            PreviewSubject::Discussion { .. } => {
                unreachable!("Discussions are previewed from GraphQL alone")
            }
        })
    }

    /// What GraphQL adds to `request`'s Preview; `None` when it needs nothing from it: an issue not about a
    /// mention, a release, a commit.
    async fn extras(&self, request: &PreviewRequest) -> Option<anyhow::Result<Extras>> {
        let mention = matches!(request.wanted, Wanted::Mention { .. });
        match request.subject {
            PreviewSubject::PullRequest => {}
            PreviewSubject::Issue if mention => {}
            _ => return None,
        }
        let looked_up = self
            .graphql::<ResourceData>(
                EXTRAS_QUERY,
                json!({ "url": request.page, "mention": mention }),
            )
            .await
            .and_then(|data| {
                data.resource.ok_or_else(|| {
                    anyhow!("GitHub's GraphQL API found nothing at {}", request.page)
                })
            });
        Some(looked_up.map(|resource| {
            let comments = resource.comments.into_iter().flat_map(Nodes::into_iter);
            let reviews = resource
                .reviews
                .into_iter()
                .flat_map(Nodes::into_iter)
                .flat_map(|review| {
                    let comments = review.comments.into_iter().flat_map(Nodes::into_iter);
                    std::iter::once(review.said)
                        .chain(comments)
                        .collect::<Vec<_>>()
                });
            Extras {
                ci: resource
                    .status_check_rollup
                    .and_then(|rollup| match rollup.state.as_str() {
                        "SUCCESS" => Some(Ci::Passing),
                        "FAILURE" | "ERROR" => Some(Ci::Failing),
                        "PENDING" | "EXPECTED" => Some(Ci::Pending),
                        _ => None,
                    }),
                review: resource
                    .review_decision
                    .as_deref()
                    .and_then(|decision| match decision {
                        "APPROVED" => Some(ReviewDecision::Approved),
                        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
                        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
                        _ => None,
                    }),
                comments: comments.chain(reviews).map(Said::into_comment).collect(),
            }
        }))
    }

    /// The Preview of Discussion `number` in `owner/name`, showing what `wanted` calls for: the accepted answer, if
    /// there's one, rather than the latest comment.
    async fn discussion_preview(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        wanted: Wanted,
    ) -> anyhow::Result<Preview> {
        let (data, handles) = tokio::join!(
            self.graphql::<RepositoryData>(
                DISCUSSION_QUERY,
                json!({ "owner": owner, "name": name, "number": number }),
            ),
            OptionFuture::from(self.handles_for(wanted)),
        );
        let Some(discussion) = data?
            .repository
            .and_then(|repository| repository.discussion)
        else {
            bail!("GitHub has no Discussion #{number} in {owner}/{name}");
        };
        let comments: Vec<Comment> = discussion
            .comments
            .into_iter()
            .flat_map(|comment| std::iter::once(comment.said).chain(comment.replies))
            .map(Said::into_comment)
            .collect();
        let latest = discussion
            .answer
            .map(|answer| Comment {
                why: Why::Answer,
                ..answer.into_comment()
            })
            .or_else(|| comments.iter().max_by_key(|c| c.created_at).cloned());
        let mut preview = Preview {
            reference: format!("#{}", discussion.number),
            title: discussion.title,
            state: None,
            author: discussion.author.map(|author| author.login),
            labels: discussion
                .labels
                .into_iter()
                .flat_map(Nodes::into_iter)
                .map(|label| label.name)
                .collect(),
            closed: discussion
                .closed_at
                .filter(|_| discussion.closed)
                .map(|at| Closed {
                    by: None,
                    at,
                    merged: false,
                }),
            pull_request: None,
            discussion: Some(DiscussionSummary {
                category: discussion.category.name,
                answered: discussion.is_answered.unwrap_or(false),
            }),
            shown: Shown::Body(Body::Markdown(discussion.body)),
            note: None,
        };
        match (wanted, handles) {
            (Wanted::Mention { .. }, Some(handles)) => {
                show_mention(&mut preview, handles, Ok(comments), latest);
            }
            _ => {
                if let Some(latest) = latest {
                    preview.shown = Shown::Comment(latest);
                }
            }
        }
        Ok(preview)
    }

    /// The handles to look for in comments when `wanted` is a mention; `None` for anything else.
    fn handles_for(
        &self,
        wanted: Wanted,
    ) -> Option<impl Future<Output = Result<Vec<String>, String>> + '_> {
        match wanted {
            Wanted::Mention { team } => Some(self.handles(team)),
            Wanted::Description | Wanted::LatestComment => None,
        }
    }

    /// The handles a mention of the user can use: their login, or (`team`) their teams as `org/team`. Looked up
    /// once a run, and so is a failure to: it's not tried again for every Preview.
    async fn handles(&self, team: bool) -> Result<Vec<String>, String> {
        if team {
            if let Some(teams) = self.mentionable().teams.clone() {
                return teams;
            }
            // The first hundred: enough for anyone but an organization's owners.
            let teams = self
                .get::<Vec<ApiTeam>>("/user/teams?per_page=100")
                .await
                .map(|teams| {
                    teams
                        .into_iter()
                        .map(|team| format!("{}/{}", team.organization.login, team.slug))
                        .collect()
                })
                .map_err(|error| format!("couldn't look up your teams: {error:#}"));
            self.mentionable().teams = Some(teams.clone());
            teams
        } else {
            if let Some(login) = self.mentionable().login.clone() {
                return login.map(|login| vec![login]);
            }
            let login = self
                .get::<ApiUser>("/user")
                .await
                .map(|user| user.login)
                .map_err(|error| format!("couldn't look up who you are: {error:#}"));
            self.mentionable().login = Some(login.clone());
            login.map(|login| vec![login])
        }
    }

    fn mentionable(&self) -> MutexGuard<'_, Mentionable> {
        // Never held across an `.await`, nor while anything can panic.
        self.mentionable
            .lock()
            .expect("the mentionable lock is never poisoned")
    }

    /// The `data` GitHub's GraphQL API answers `query` with, given `variables`.
    async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> anyhow::Result<T> {
        let answer: GraphQlAnswer<T> = self
            .client
            .post(
                &self.graphql_url,
                Some(&json!({ "query": query, "variables": variables })),
            )
            .await
            .map_err(|error| self.explain(error))?;
        answer.data.ok_or_else(|| {
            anyhow!(
                "GitHub's GraphQL API answered with errors: {}",
                serde_json::Value::from(answer.errors.unwrap_or_default())
            )
        })
    }
}

/// Puts in `preview`, which shows its body, what a mention calls for: the latest of `comments` to mention one of
/// `handles`, else the body if it mentions one; and otherwise `latest`, noting why if the mention couldn't be
/// looked for.
fn show_mention(
    preview: &mut Preview,
    handles: Result<Vec<String>, String>,
    comments: Result<Vec<Comment>, String>,
    latest: Option<Comment>,
) {
    match handles.and_then(|handles| comments.map(|comments| (handles, comments))) {
        Ok((handles, mut comments)) => {
            let mentions = |text: &str| {
                handles
                    .iter()
                    .any(|handle| markdown::mentions(text, handle))
            };
            comments.sort_by_key(|comment| Reverse(comment.created_at));
            if let Some(comment) = comments.into_iter().find(|comment| mentions(&comment.body)) {
                preview.shown = Shown::Comment(Comment {
                    why: Why::MentionsYou,
                    ..comment
                });
                return;
            }
            if let Shown::Body(Body::Markdown(body) | Body::Plain(body)) = &preview.shown
                && mentions(body)
            {
                preview.note = Some(Note::BodyMentionsYou);
                return;
            }
        }
        Err(why) => preview.note = Some(Note::MentionNotLookedFor(why)),
    }
    if let Some(latest) = latest {
        preview.shown = Shown::Comment(latest);
    }
}

/// What GraphQL adds to a pull request's or an issue's Preview.
#[derive(Debug, Default)]
struct Extras {
    ci: Option<Ci>,
    review: Option<ReviewDecision>,
    /// The comments to look through for a mention, in no particular order; empty unless asked for.
    comments: Vec<Comment>,
}

/// A GitHub account, as the REST API names it.
#[derive(Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Deserialize)]
struct ApiLabel {
    name: String,
}

/// A team, as the REST API describes the user's.
#[derive(Deserialize)]
struct ApiTeam {
    slug: String,
    organization: ApiUser,
}

/// An issue, as the REST API describes it.
#[derive(Deserialize)]
struct ApiIssue {
    number: u64,
    title: String,
    state: String,
    state_reason: Option<String>,
    user: Option<ApiUser>,
    #[serde(default)]
    labels: Vec<ApiLabel>,
    body: Option<String>,
    closed_by: Option<ApiUser>,
    closed_at: Option<DateTime<Utc>>,
}

/// A pull request, as the REST API describes it.
#[derive(Deserialize)]
struct ApiPullRequest {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged: bool,
    user: Option<ApiUser>,
    #[serde(default)]
    labels: Vec<ApiLabel>,
    body: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(default)]
    changed_files: u64,
    #[serde(default)]
    commits: u64,
    merged_by: Option<ApiUser>,
    merged_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
}

/// A release, as the REST API describes it.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    name: Option<String>,
    author: Option<ApiUser>,
    body: Option<String>,
}

/// A commit, as the REST API describes it.
#[derive(Deserialize)]
struct ApiCommit {
    sha: String,
    /// The GitHub account of the commit's author; `None` when the author's email isn't one's.
    author: Option<ApiUser>,
    commit: ApiCommitDetails,
}

#[derive(Deserialize)]
struct ApiCommitDetails {
    message: String,
}

/// A comment on an issue, a pull request (its conversation or its changes) or a commit, or a pull request review,
/// as the REST API describes it.
#[derive(Deserialize)]
struct ApiComment {
    user: Option<ApiUser>,
    body: Option<String>,
    /// A review's is `submitted_at`.
    #[serde(alias = "submitted_at")]
    created_at: DateTime<Utc>,
    /// The file, for a comment on a pull request's changes.
    path: Option<String>,
}

impl ApiComment {
    /// The comment, as the latest.
    fn into_comment(self) -> Comment {
        Comment {
            author: self.user.map(|user| user.login),
            created_at: self.created_at,
            path: self.path,
            body: self.body.unwrap_or_default(),
            why: Why::Latest,
        }
    }
}

/// A GraphQL answer: the data asked for, or the errors that kept GitHub from answering.
#[derive(Deserialize)]
struct GraphQlAnswer<T> {
    data: Option<T>,
    errors: Option<Vec<serde_json::Value>>,
}

/// A list of GraphQL nodes, of which any can be null (one the token can't see, say).
#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<Option<T>>,
}

impl<T> IntoIterator for Nodes<T> {
    type Item = T;
    type IntoIter = std::iter::Flatten<std::vec::IntoIter<Option<T>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.nodes.into_iter().flatten()
    }
}

#[derive(Deserialize)]
struct ResourceData {
    resource: Option<GraphQlSubject>,
}

/// A pull request or an issue, as [`EXTRAS_QUERY`] describes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlSubject {
    status_check_rollup: Option<GraphQlRollup>,
    review_decision: Option<String>,
    comments: Option<Nodes<Said>>,
    reviews: Option<Nodes<GraphQlReview>>,
}

#[derive(Deserialize)]
struct GraphQlRollup {
    state: String,
}

#[derive(Deserialize)]
struct GraphQlReview {
    #[serde(flatten)]
    said: Said,
    comments: Option<Nodes<Said>>,
}

/// Something someone said on GitHub, as GraphQL's `Said` fragment describes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Said {
    author: Option<ApiUser>,
    body: String,
    created_at: DateTime<Utc>,
    /// The file, for a comment on a pull request's changes.
    path: Option<String>,
}

impl Said {
    /// What was said, as a comment, as the latest.
    fn into_comment(self) -> Comment {
        Comment {
            author: self.author.map(|author| author.login),
            created_at: self.created_at,
            path: self.path,
            body: self.body,
            why: Why::Latest,
        }
    }
}

#[derive(Deserialize)]
struct RepositoryData {
    repository: Option<GraphQlRepository>,
}

#[derive(Deserialize)]
struct GraphQlRepository {
    discussion: Option<GraphQlDiscussion>,
}

/// A Discussion, as [`DISCUSSION_QUERY`] describes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlDiscussion {
    number: u64,
    title: String,
    body: String,
    author: Option<ApiUser>,
    category: ApiLabel,
    labels: Option<Nodes<ApiLabel>>,
    closed: bool,
    closed_at: Option<DateTime<Utc>>,
    is_answered: Option<bool>,
    answer: Option<Said>,
    comments: Nodes<GraphQlDiscussionComment>,
}

#[derive(Deserialize)]
struct GraphQlDiscussionComment {
    #[serde(flatten)]
    said: Said,
    replies: Nodes<Said>,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::tests::github_for;
    use super::*;
    use crate::inbox::{ApiPaths, SubjectKind, ThreadId};
    use crate::state::SubjectState;

    /// A Notification for `reason` whose Subject and latest comment the REST API has at `subject` and
    /// `latest_comment`.
    fn notification_at(
        subject: Option<&str>,
        latest_comment: Option<&str>,
        reason: Reason,
    ) -> Notification {
        Notification {
            thread: ThreadId(7),
            subject_url: "https://github.com/wcgw/ghd/issues/7".to_owned(),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::Issue,
            subject_title: "Subject 7".to_owned(),
            reason,
            unread: true,
            updated_at: "2026-10-22T12:00:00Z".parse().expect("valid RFC 3339"),
            api_paths: ApiPaths {
                subject: subject.map(str::to_owned),
                latest_comment: latest_comment.map(str::to_owned),
            },
        }
    }

    fn preview_request(
        subject: &str,
        latest_comment: Option<&str>,
        reason: Reason,
    ) -> PreviewRequest {
        PreviewRequest::for_notification(&notification_at(Some(subject), latest_comment, reason))
            .expect("a Subject ghd previews")
    }

    fn time(at: &str) -> DateTime<Utc> {
        at.parse().expect("valid RFC 3339")
    }

    /// Answers a GET of `at` with `body`, once, and only if it carries the token.
    async fn serve(server: &MockServer, at: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(at))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    /// Answers GraphQL queries whose variables include `variables` with `data`, once.
    async fn serve_graphql(server: &MockServer, variables: Value, data: Value) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_partial_json(json!({ "variables": variables })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
            .expect(1)
            .mount(server)
            .await;
    }

    /// An issue as the REST API describes it, open, by octocat.
    fn api_issue(number: u64, body: &str) -> Value {
        json!({
            "number": number,
            "title": "Add previews",
            "state": "open",
            "user": { "login": "octocat" },
            "labels": [],
            "body": body,
        })
    }

    /// A comment as the REST API and GraphQL both describe it, well enough.
    fn said(author: &str, body: &str, at: &str) -> Value {
        json!({
            "user": { "login": author },
            "author": { "login": author },
            "body": body,
            "created_at": at,
            "createdAt": at,
        })
    }

    #[test]
    fn previews_are_of_subjects_ghd_can_show() {
        let previewable = |subject| {
            PreviewRequest::for_notification(&notification_at(subject, None, Reason::Subscribed))
                .is_some()
        };
        assert!(previewable(Some("/repos/wcgw/ghd/pulls/7")));
        assert!(previewable(Some("/repos/wcgw/ghd/issues/7")));
        assert!(previewable(Some("/repos/wcgw/ghd/releases/1")));
        assert!(previewable(Some("/repos/wcgw/ghd/commits/0123abc")));
        assert!(previewable(Some("/repos/wcgw/ghd/discussions/7")));
        assert!(previewable(Some("/api/v3/repos/wcgw/ghd/issues/7")));
        assert!(!previewable(Some("/repos/wcgw/ghd/discussions/latest")));
        assert!(!previewable(Some("/repos/wcgw/ghd/check-suites/1")));
        assert!(!previewable(None));
    }

    #[test]
    fn only_a_comment_is_fetched_as_the_latest_comment() {
        let issue = "/repos/wcgw/ghd/issues/7";
        let comment_of = |latest| preview_request(issue, latest, Reason::Comment).comment_path;
        for comment in [
            "/repos/wcgw/ghd/issues/comments/70",
            "/repos/wcgw/ghd/pulls/comments/71",
            "/repos/wcgw/ghd/comments/72",
            "/repos/wcgw/ghd/pulls/7/reviews/73",
        ] {
            assert_eq!(comment_of(Some(comment)).as_deref(), Some(comment));
        }
        assert_eq!(
            comment_of(Some(issue)),
            None,
            "GitHub points to the Subject when there's no comment"
        );
        assert_eq!(comment_of(None), None);
    }

    #[test]
    fn reasons_decide_what_a_preview_shows() {
        for (reason, wanted) in [
            (Reason::ReviewRequested, Wanted::Description),
            (Reason::Assign, Wanted::Description),
            (Reason::Mention, Wanted::Mention { team: false }),
            (Reason::TeamMention, Wanted::Mention { team: true }),
            (Reason::Comment, Wanted::LatestComment),
            (Reason::StateChange, Wanted::LatestComment),
            (Reason::Other("brand_new".to_owned()), Wanted::LatestComment),
        ] {
            assert_eq!(Wanted::for_reason(&reason), wanted, "{reason}");
        }
    }

    #[tokio::test]
    async fn preview_shows_the_latest_comment_under_the_subject() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/repos/wcgw/ghd/issues/7",
            json!({
                "number": 7,
                "title": "Add previews",
                "state": "closed",
                "state_reason": "not_planned",
                "user": { "login": "octocat" },
                "labels": [{ "name": "ui" }, { "name": "help wanted" }],
                "body": "The description",
                "closed_by": { "login": "hubot" },
                "closed_at": "2026-10-22T10:00:00Z",
            }),
        )
        .await;
        serve(
            &server,
            "/repos/wcgw/ghd/issues/comments/70",
            said("hubot", "Closing, sorry", "2026-10-22T11:00:00Z"),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                Some("/repos/wcgw/ghd/issues/comments/70"),
                Reason::StateChange,
            ))
            .await
            .expect("both are served");

        assert_eq!(
            preview,
            Preview {
                reference: "#7".to_owned(),
                title: "Add previews".to_owned(),
                state: Some(SubjectState::NotPlanned),
                author: Some("octocat".to_owned()),
                labels: vec!["ui".to_owned(), "help wanted".to_owned()],
                closed: Some(Closed {
                    by: Some("hubot".to_owned()),
                    at: time("2026-10-22T10:00:00Z"),
                    merged: false,
                }),
                pull_request: None,
                discussion: None,
                shown: Shown::Comment(Comment {
                    author: Some("hubot".to_owned()),
                    created_at: time("2026-10-22T11:00:00Z"),
                    path: None,
                    body: "Closing, sorry".to_owned(),
                    why: Why::Latest,
                }),
                note: None,
            }
        );
    }

    #[tokio::test]
    async fn preview_without_a_comment_shows_the_body() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/repos/wcgw/ghd/pulls/8",
            json!({
                "number": 8,
                "title": "Draft it",
                "state": "open",
                "draft": true,
                "merged": false,
                "user": null,
                "labels": [],
                "body": null,
            }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/pulls/8",
                Some("/repos/wcgw/ghd/pulls/8"),
                Reason::Subscribed,
            ))
            .await
            .expect("served; GraphQL failing only leaves out the checks");

        assert_eq!(preview.reference, "#8");
        assert_eq!(preview.state, Some(SubjectState::Draft));
        assert_eq!(preview.author, None, "a deleted account");
        assert_eq!(preview.closed, None);
        assert_eq!(preview.shown, Shown::Body(Body::Markdown(String::new())));
    }

    #[tokio::test]
    async fn review_request_shows_the_description_and_how_far_along_it_is() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/repos/wcgw/ghd/pulls/8",
            json!({
                "number": 8,
                "title": "Add previews",
                "state": "open",
                "body": "What it does",
                "additions": 120,
                "deletions": 30,
                "changed_files": 7,
                "commits": 3,
            }),
        )
        .await;
        serve_graphql(
            &server,
            json!({ "url": "https://github.com/wcgw/ghd/issues/7", "mention": false }),
            json!({ "resource": {
                "statusCheckRollup": { "state": "FAILURE" },
                "reviewDecision": "CHANGES_REQUESTED",
            } }),
        )
        .await;

        // No latest comment is served: fetching it would fail the Preview.
        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/pulls/8",
                Some("/repos/wcgw/ghd/issues/comments/70"),
                Reason::ReviewRequested,
            ))
            .await
            .expect("served");

        assert_eq!(
            preview.shown,
            Shown::Body(Body::Markdown("What it does".to_owned()))
        );
        assert_eq!(
            preview.pull_request,
            Some(PullRequestSummary {
                additions: 120,
                deletions: 30,
                changed_files: 7,
                commits: 3,
                ci: Some(Ci::Failing),
                review: Some(ReviewDecision::ChangesRequested),
            })
        );
    }

    #[tokio::test]
    async fn merged_pull_request_says_who_merged_it() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/repos/wcgw/ghd/pulls/8",
            json!({
                "number": 8,
                "title": "T",
                "state": "closed",
                "merged": true,
                "body": "",
                "merged_by": { "login": "alxs" },
                "merged_at": "2026-10-22T09:00:00Z",
                "closed_at": "2026-10-22T09:00:00Z",
            }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/pulls/8",
                None,
                Reason::StateChange,
            ))
            .await
            .expect("served");

        assert_eq!(preview.state, Some(SubjectState::Merged));
        assert_eq!(
            preview.closed,
            Some(Closed {
                by: Some("alxs".to_owned()),
                at: time("2026-10-22T09:00:00Z"),
                merged: true,
            })
        );
    }

    #[tokio::test]
    async fn mention_shows_the_latest_comment_mentioning_you() {
        let server = MockServer::start().await;
        serve(&server, "/user", json!({ "login": "alxs" })).await;
        serve(&server, "/repos/wcgw/ghd/issues/7", api_issue(7, "Body")).await;
        serve(
            &server,
            "/repos/wcgw/ghd/issues/comments/72",
            said("hubot", "Merging soon", "2026-10-22T11:30:00Z"),
        )
        .await;
        serve_graphql(
            &server,
            json!({ "mention": true }),
            json!({ "resource": { "comments": { "nodes": [
                said("octocat", "@alxs what do you think?", "2026-10-22T10:00:00Z"),
                said("hubot", "@alxs-bot ran the tests", "2026-10-22T11:00:00Z"),
                null,
            ] } } }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                Some("/repos/wcgw/ghd/issues/comments/72"),
                Reason::Mention,
            ))
            .await
            .expect("served");

        assert_eq!(
            preview.shown,
            Shown::Comment(Comment {
                author: Some("octocat".to_owned()),
                created_at: time("2026-10-22T10:00:00Z"),
                path: None,
                body: "@alxs what do you think?".to_owned(),
                why: Why::MentionsYou,
            })
        );
        assert_eq!(preview.note, None);
    }

    #[tokio::test]
    async fn mention_in_a_review_comment_names_its_file() {
        let server = MockServer::start().await;
        let github = GitHub::with_token(
            "test-token".to_owned().into(),
            &Account::new(None, Some("alxs")).expect("a valid login"),
            &server.uri(),
        )
        .expect("mock server address is valid");
        serve(
            &server,
            "/repos/wcgw/ghd/pulls/8",
            json!({ "number": 8, "title": "T", "state": "open", "body": "" }),
        )
        .await;
        serve_graphql(
            &server,
            json!({ "mention": true }),
            json!({ "resource": {
                "comments": { "nodes": [said("octocat", "@alxs early", "2026-10-22T09:00:00Z")] },
                "reviews": { "nodes": [{
                    "author": { "login": "hubot" },
                    "body": "",
                    "createdAt": "2026-10-22T10:00:00Z",
                    "comments": { "nodes": [{
                        "author": { "login": "hubot" },
                        "body": "@alxs is this right?",
                        "createdAt": "2026-10-22T10:00:00Z",
                        "path": "src/ui.rs",
                    }] },
                }] },
            } }),
        )
        .await;

        let preview = github
            .preview(&preview_request(
                "/repos/wcgw/ghd/pulls/8",
                None,
                Reason::Mention,
            ))
            .await
            .expect("served, without looking up who the user is");

        let Shown::Comment(comment) = preview.shown else {
            panic!("shows a comment, not {:?}", preview.shown);
        };
        assert_eq!(
            (comment.path.as_deref(), comment.why),
            (Some("src/ui.rs"), Why::MentionsYou),
            "the newest mention"
        );
    }

    #[tokio::test]
    async fn mention_in_the_description_shows_it() {
        let server = MockServer::start().await;
        serve(&server, "/user", json!({ "login": "alxs" })).await;
        serve(
            &server,
            "/repos/wcgw/ghd/issues/7",
            api_issue(7, "cc @alxs"),
        )
        .await;
        serve_graphql(
            &server,
            json!({ "mention": true }),
            json!({ "resource": { "comments": { "nodes": [
                said("hubot", "Not you", "2026-10-22T10:00:00Z"),
            ] } } }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                None,
                Reason::Mention,
            ))
            .await
            .expect("served");

        assert_eq!(
            preview.shown,
            Shown::Body(Body::Markdown("cc @alxs".to_owned()))
        );
        assert_eq!(preview.note, Some(Note::BodyMentionsYou));
    }

    #[tokio::test]
    async fn mention_not_found_shows_the_latest_comment() {
        let server = MockServer::start().await;
        serve(&server, "/user", json!({ "login": "alxs" })).await;
        serve(&server, "/repos/wcgw/ghd/issues/7", api_issue(7, "Body")).await;
        serve(
            &server,
            "/repos/wcgw/ghd/issues/comments/72",
            said("hubot", "Latest", "2026-10-22T11:30:00Z"),
        )
        .await;
        serve_graphql(
            &server,
            json!({ "mention": true }),
            json!({ "resource": { "comments": { "nodes": [] } } }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                Some("/repos/wcgw/ghd/issues/comments/72"),
                Reason::Mention,
            ))
            .await
            .expect("served");

        let Shown::Comment(comment) = preview.shown else {
            panic!("shows a comment, not {:?}", preview.shown);
        };
        assert_eq!(
            (comment.body.as_str(), comment.why),
            ("Latest", Why::Latest)
        );
        assert_eq!(preview.note, None);
    }

    #[tokio::test]
    async fn team_mention_looks_for_your_teams() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/user/teams",
            json!([{ "slug": "maintainers", "organization": { "login": "wcgw" } }]),
        )
        .await;
        serve(&server, "/repos/wcgw/ghd/issues/7", api_issue(7, "Body")).await;
        serve_graphql(
            &server,
            json!({ "mention": true }),
            json!({ "resource": { "comments": { "nodes": [
                said("octocat", "@wcgw/maintainers please look", "2026-10-22T10:00:00Z"),
            ] } } }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                None,
                Reason::TeamMention,
            ))
            .await
            .expect("served");

        assert!(
            matches!(&preview.shown, Shown::Comment(c) if c.why == Why::MentionsYou),
            "{:?}",
            preview.shown
        );
    }

    #[tokio::test]
    async fn not_knowing_who_you_are_is_noted_and_not_asked_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(json!({ "message": "Forbidden" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/wcgw/ghd/issues/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(api_issue(7, "Body")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/wcgw/ghd/issues/comments/72"))
            .respond_with(ResponseTemplate::new(200).set_body_json(said(
                "hubot",
                "Latest",
                "2026-10-22T11:30:00Z",
            )))
            .mount(&server)
            .await;
        let github = github_for(&server).await;
        let request = preview_request(
            "/repos/wcgw/ghd/issues/7",
            Some("/repos/wcgw/ghd/issues/comments/72"),
            Reason::Mention,
        );

        let first = github.preview(&request).await.expect("served");
        let second = github.preview(&request).await.expect("served");

        for preview in [first, second] {
            assert!(
                matches!(&preview.shown, Shown::Comment(c) if c.why == Why::Latest),
                "{:?}",
                preview.shown
            );
            let Some(Note::MentionNotLookedFor(why)) = preview.note else {
                panic!("says why: {:?}", preview.note);
            };
            assert!(why.starts_with("couldn't look up who you are"), "{why}");
        }
    }

    #[tokio::test]
    async fn failed_search_for_the_mention_is_noted() {
        let server = MockServer::start().await;
        serve(&server, "/user", json!({ "login": "alxs" })).await;
        serve(&server, "/repos/wcgw/ghd/issues/7", api_issue(7, "Body")).await;
        serve(
            &server,
            "/repos/wcgw/ghd/issues/comments/72",
            said("hubot", "Latest", "2026-10-22T11:30:00Z"),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(
                ResponseTemplate::new(502).set_body_json(json!({ "message": "Bad Gateway" })),
            )
            .mount(&server)
            .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                Some("/repos/wcgw/ghd/issues/comments/72"),
                Reason::Mention,
            ))
            .await
            .expect("the Subject and its latest comment are enough");

        assert!(
            matches!(&preview.shown, Shown::Comment(c) if c.why == Why::Latest),
            "{:?}",
            preview.shown
        );
        let Some(Note::MentionNotLookedFor(why)) = preview.note else {
            panic!("says why: {:?}", preview.note);
        };
        assert!(why.contains("502"), "{why}");
    }

    #[tokio::test]
    async fn discussion_shows_its_accepted_answer() {
        let server = MockServer::start().await;
        serve_graphql(
            &server,
            json!({ "owner": "wcgw", "name": "ghd", "number": 12 }),
            json!({ "repository": { "discussion": {
                "number": 12,
                "title": "How do I theme it?",
                "body": "Question",
                "author": { "login": "octocat" },
                "category": { "name": "Q&A" },
                "labels": { "nodes": [{ "name": "themes" }] },
                "closed": false,
                "closedAt": null,
                "isAnswered": true,
                "answer": said("alxs", "Like so", "2026-10-20T10:00:00Z"),
                "comments": { "nodes": [{
                    "author": { "login": "hubot" },
                    "body": "Thanks!",
                    "createdAt": "2026-10-21T10:00:00Z",
                    "replies": { "nodes": [] },
                }] },
            } } }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/discussions/12",
                None,
                Reason::Subscribed,
            ))
            .await
            .expect("served");

        assert_eq!(
            (preview.reference.as_str(), preview.labels.as_slice()),
            ("#12", ["themes".to_owned()].as_slice())
        );
        assert_eq!(
            preview.discussion,
            Some(DiscussionSummary {
                category: "Q&A".to_owned(),
                answered: true,
            })
        );
        let Shown::Comment(comment) = preview.shown else {
            panic!("shows a comment, not {:?}", preview.shown);
        };
        assert_eq!(
            (comment.body.as_str(), comment.why),
            ("Like so", Why::Answer)
        );
    }

    #[tokio::test]
    async fn discussion_mention_is_found_in_replies() {
        let server = MockServer::start().await;
        serve(&server, "/user", json!({ "login": "alxs" })).await;
        serve_graphql(
            &server,
            json!({ "number": 12 }),
            json!({ "repository": { "discussion": {
                "number": 12,
                "title": "T",
                "body": "",
                "author": null,
                "category": { "name": "Ideas" },
                "labels": null,
                "closed": true,
                "closedAt": "2026-10-22T08:00:00Z",
                "isAnswered": null,
                "answer": null,
                "comments": { "nodes": [{
                    "author": { "login": "hubot" },
                    "body": "Top",
                    "createdAt": "2026-10-21T10:00:00Z",
                    "replies": { "nodes": [
                        said("octocat", "@alxs thoughts?", "2026-10-21T11:00:00Z"),
                    ] },
                }] },
            } } }),
        )
        .await;

        let preview = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/discussions/12",
                None,
                Reason::Mention,
            ))
            .await
            .expect("served");

        assert!(preview.closed.is_some_and(|closed| !closed.merged));
        assert!(
            matches!(&preview.shown, Shown::Comment(c) if c.why == Why::MentionsYou && c.body == "@alxs thoughts?"),
            "{:?}",
            preview.shown
        );
    }

    #[tokio::test]
    async fn missing_discussion_fails_the_preview() {
        let server = MockServer::start().await;
        serve_graphql(
            &server,
            json!({ "number": 12 }),
            json!({ "repository": { "discussion": null } }),
        )
        .await;

        let error = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/discussions/12",
                None,
                Reason::Subscribed,
            ))
            .await
            .expect_err("nothing to show");

        assert!(
            format!("{error:#}").contains("no Discussion #12 in wcgw/ghd"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn release_and_commit_previews_show_their_text() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/repos/wcgw/ghd/releases/1",
            json!({
                "tag_name": "v0.1.0",
                "name": "First",
                "author": { "login": "octocat" },
                "body": "## Notes",
            }),
        )
        .await;
        serve(
            &server,
            "/repos/wcgw/ghd/commits/0123456789abcdef",
            json!({
                "sha": "0123456789abcdef",
                "author": null,
                "commit": { "message": "Fix the thing\n\nIt was broken." },
            }),
        )
        .await;
        let github = github_for(&server).await;

        let release = github
            .preview(&preview_request(
                "/repos/wcgw/ghd/releases/1",
                None,
                Reason::Subscribed,
            ))
            .await
            .expect("served");
        let commit = github
            .preview(&preview_request(
                "/repos/wcgw/ghd/commits/0123456789abcdef",
                None,
                Reason::Subscribed,
            ))
            .await
            .expect("served");

        assert_eq!(
            (release.reference.as_str(), release.title.as_str()),
            ("v0.1.0", "First")
        );
        assert_eq!(
            release.shown,
            Shown::Body(Body::Markdown("## Notes".to_owned()))
        );
        assert_eq!(
            (commit.reference.as_str(), commit.title.as_str()),
            ("0123456", "Fix the thing")
        );
        assert_eq!(
            commit.shown,
            Shown::Body(Body::Plain("Fix the thing\n\nIt was broken.".to_owned()))
        );
    }

    #[tokio::test]
    async fn enterprise_server_previews_stay_under_its_api() {
        let server = MockServer::start().await;
        serve(
            &server,
            "/api/v3/repos/wcgw/ghd/issues/7",
            api_issue(7, "B"),
        )
        .await;
        let github = GitHub::with_token(
            "test-token".to_owned().into(),
            &Account::default(),
            &format!("{}/api/v3", server.uri()),
        )
        .expect("mock server address is valid");

        let preview = github
            .preview(&preview_request(
                "/api/v3/repos/wcgw/ghd/issues/7",
                None,
                Reason::Subscribed,
            ))
            .await;

        assert_eq!(preview.expect("served").reference, "#7");
    }

    #[tokio::test]
    async fn missing_subject_fails_the_preview() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
            )
            .mount(&server)
            .await;

        let error = github_for(&server)
            .await
            .preview(&preview_request(
                "/repos/wcgw/ghd/issues/7",
                None,
                Reason::Subscribed,
            ))
            .await
            .expect_err("nothing to show");

        assert!(format!("{error:#}").contains("404: Not Found"), "{error:#}");
    }
}
