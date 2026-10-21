//! The user's GitHub notifications, through the REST API.

use std::env::{self, VarError};
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use http::header::{HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE, LAST_MODIFIED};
use http::{StatusCode, Uri};
use octocrab::models::activity::Notification as ApiNotification;
use octocrab::secrecy::SecretString;
use octocrab::{FromResponse as _, GitHubError, Octocrab, Page};

use crate::inbox::{Inbox, Notification, Reason, SubjectKind, ThreadId};

/// The REST API root for github.com.
const GITHUB_API: &str = "https://api.github.com";

/// The website root for github.com.
const GITHUB_WEB: &str = "https://github.com";

/// The Inbox's first page: read and unread Notifications, as many per page as the API allows.
const INBOX_FIRST_PAGE: &str = "/notifications?all=true&per_page=50";

/// How long to wait between polls of the Inbox until GitHub says.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// GitHub's response header for the shortest time between polls of the Inbox, in seconds.
const X_POLL_INTERVAL: HeaderName = HeaderName::from_static("x-poll-interval");

/// A GitHub API client acting as the user's Account.
pub struct GitHub {
    client: Octocrab,
    /// Where the website lives, without a trailing `/`; Subjects' pages are below it.
    web_root: String,
    /// What GitHub has said so far about polling the Inbox.
    polling: Mutex<Polling>,
}

/// What GitHub has said about polling the Inbox.
struct Polling {
    /// The `Last-Modified` of the last whole Inbox fetched; `None` until one is.
    last_modified: Option<HeaderValue>,
    /// The shortest time between polls GitHub last asked for.
    interval: Duration,
}

impl GitHub {
    /// A client for github.com using a token borrowed from the environment: `GH_TOKEN`, then `GITHUB_TOKEN`, then
    /// `gh auth token` (the `gh` CLI's active account). Makes no request to GitHub.
    ///
    /// # Errors
    ///
    /// Fails when none of those yields a token.
    pub async fn authenticate() -> anyhow::Result<Self> {
        Self::with_token(borrow_token().await?, GITHUB_API, GITHUB_WEB)
    }

    /// A client sending `token` to the REST API rooted at `api_root`, for the website at `web_root`.
    fn with_token(token: SecretString, api_root: &str, web_root: &str) -> anyhow::Result<Self> {
        let client = Octocrab::builder()
            .personal_token(token)
            .base_uri(api_root)
            .with_context(|| format!("invalid GitHub API address {api_root}"))?
            .build()
            .context("building the GitHub client")?;
        Ok(Self {
            client,
            web_root: web_root.trim_end_matches('/').to_owned(),
            polling: Mutex::new(Polling {
                last_modified: None,
                interval: DEFAULT_POLL_INTERVAL,
            }),
        })
    }

    /// The whole Inbox, read and unread, fetched page by page. Remembers when GitHub says it last changed, for
    /// [`GitHub::inbox_if_changed`].
    ///
    /// # Errors
    ///
    /// Fails if any page can't be fetched. When GitHub rejects the token itself, the error says which tokens can
    /// read notifications.
    ///
    /// # Cancel Safety
    ///
    /// Cancel safe: dropping the future discards the pages fetched so far, and nothing changes on GitHub.
    pub async fn inbox(&self) -> anyhow::Result<Inbox> {
        self.fetch_inbox(None)
            .await?
            .ok_or_else(|| anyhow!("GitHub answered 304 Not Modified to an unconditional request"))
    }

    /// The whole Inbox if it changed since the last one fetched; `None` when GitHub says it hasn't, an answer that
    /// doesn't count against the rate limit. The same as [`GitHub::inbox`] until a whole Inbox has been fetched.
    ///
    /// # Errors
    ///
    /// As for [`GitHub::inbox`].
    ///
    /// # Cancel Safety
    ///
    /// Cancel safe, as [`GitHub::inbox`] is.
    pub async fn inbox_if_changed(&self) -> anyhow::Result<Option<Inbox>> {
        let last_modified = self.polling().last_modified.clone();
        self.fetch_inbox(last_modified).await
    }

    /// How long GitHub wants clients to wait between polls of the Inbox: the last `X-Poll-Interval` it sent, or 60
    /// seconds until it sends one.
    pub fn poll_interval(&self) -> Duration {
        self.polling().interval
    }

    /// The whole Inbox; `None` if GitHub answers that it hasn't changed since `if_modified_since`.
    async fn fetch_inbox(
        &self,
        if_modified_since: Option<HeaderValue>,
    ) -> anyhow::Result<Option<Inbox>> {
        let Some(first) = self
            .inbox_page(Uri::from_static(INBOX_FIRST_PAGE), if_modified_since)
            .await?
        else {
            return Ok(None);
        };
        let mut notifications = first.notifications;
        let mut next = first.next;
        // Once the first page has changed, every page needs fetching again: the rest are asked unconditionally.
        while let Some(uri) = next {
            let page = self
                .inbox_page(uri, None)
                .await?
                .context("GitHub answered 304 Not Modified to an unconditional request")?;
            notifications.extend(page.notifications);
            next = page.next;
        }

        // Only now that every page has arrived, so that a fetch that failed half-way is retried in full.
        self.polling().last_modified = first.last_modified;
        Ok(Some(
            notifications
                .into_iter()
                .map(|api| self.notification_from_api(api))
                .collect(),
        ))
    }

    /// The Inbox page at `uri`; `None` if GitHub answers that it hasn't changed since `if_modified_since`. Notes the
    /// poll interval GitHub asks for.
    async fn inbox_page(
        &self,
        uri: Uri,
        if_modified_since: Option<HeaderValue>,
    ) -> anyhow::Result<Option<InboxPage>> {
        // Octocrab's raw GET rather than its typed builder: polling needs the response's `Last-Modified`,
        // `X-Poll-Interval` and 304 status, which the typed builder hides.
        let headers =
            if_modified_since.map(|value| HeaderMap::from_iter([(IF_MODIFIED_SINCE, value)]));
        let response = self
            .client
            ._get_with_headers(uri, headers)
            .await
            .context("fetching notifications")?;
        self.note_poll_interval(response.headers());
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }

        let response = octocrab::map_github_error(response)
            .await
            .map_err(explain_rejection)?;
        let last_modified = response.headers().get(LAST_MODIFIED).cloned();
        let page = Page::<ApiNotification>::from_response(response)
            .await
            .context("reading notifications")?;
        Ok(Some(InboxPage {
            notifications: page.items,
            next: page.next,
            last_modified,
        }))
    }

    /// Remembers the poll interval that `headers` ask for, if they ask for one of at least a second.
    fn note_poll_interval(&self, headers: &HeaderMap) {
        let seconds = headers
            .get(X_POLL_INTERVAL)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
            .filter(|&seconds| seconds > 0);
        if let Some(seconds) = seconds {
            self.polling().interval = Duration::from_secs(seconds);
        }
    }

    fn polling(&self) -> MutexGuard<'_, Polling> {
        // Never held across an `.await`, nor while anything can panic.
        self.polling
            .lock()
            .expect("the polling lock is never poisoned")
    }

    /// Marks `thread` read on GitHub.
    ///
    /// # Errors
    ///
    /// Fails if GitHub can't be reached or refuses the change.
    ///
    /// # Cancel Safety
    ///
    /// Not cancel safe: dropped while waiting for GitHub, the thread may or may not end up read.
    pub async fn mark_read(&self, thread: ThreadId) -> anyhow::Result<()> {
        self.client
            .activity()
            .notifications()
            .mark_as_read(thread.0.into())
            .await
            .map_err(explain_rejection)
    }

    /// Marks `thread` Done on GitHub, which takes it out of the Inbox. The API can't undo this.
    ///
    /// # Errors
    ///
    /// Fails if GitHub can't be reached or refuses the change.
    ///
    /// # Cancel Safety
    ///
    /// Not cancel safe: dropped while waiting for GitHub, the thread may or may not end up Done.
    pub async fn mark_done(&self, thread: ThreadId) -> anyhow::Result<()> {
        self.client
            .activity()
            .notifications()
            .mark_as_done(thread.0.into())
            .await
            .map_err(explain_rejection)
    }

    /// Maps the API's representation of a Notification onto ghd's.
    fn notification_from_api(&self, api: ApiNotification) -> Notification {
        let repository = api.repository.full_name.unwrap_or(api.repository.name);
        Notification {
            thread: ThreadId(api.id.into_inner()),
            subject_url: self
                .subject_page(api.subject.url.as_ref().map(|url| url.path()), &repository),
            repository,
            subject_kind: SubjectKind::from_api(&api.subject.r#type),
            subject_title: api.subject.title,
            reason: Reason::from_api(&api.reason),
            unread: api.unread,
            updated_at: api.updated_at,
        }
    }

    /// The website page for a Subject whose API URL has the path `api_path`. Falls back to the page of `repository`
    /// (`owner/name`) when there's no API URL, or it doesn't point at a page of its own (a release's API URL has
    /// its numeric id, but its page uses the tag).
    fn subject_page(&self, api_path: Option<&str>, repository: &str) -> String {
        let segments: Vec<&str> = api_path
            .unwrap_or_default()
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        // API paths are `repos/{owner}/{name}/…`, under `api/v3/` on GitHub Enterprise Server.
        let below_repos = match segments.as_slice() {
            ["repos", rest @ ..] | ["api", "v3", "repos", rest @ ..] => rest,
            _ => &[],
        };
        let page = match below_repos {
            [owner, name, "pulls", number] => format!("{owner}/{name}/pull/{number}"),
            [owner, name, kind @ ("issues" | "discussions"), number] => {
                format!("{owner}/{name}/{kind}/{number}")
            }
            [owner, name, "commits", sha] => format!("{owner}/{name}/commit/{sha}"),
            [owner, name, "releases", _] => format!("{owner}/{name}/releases"),
            _ => repository.to_owned(),
        };
        format!("{}/{page}", self.web_root)
    }
}

/// One page of the Inbox, as the API sent it.
struct InboxPage {
    notifications: Vec<ApiNotification>,
    /// The page after this one; `None` on the last page.
    next: Option<Uri>,
    /// The `Last-Modified` GitHub sent with the page, if any.
    last_modified: Option<HeaderValue>,
}

impl fmt::Debug for GitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Octocrab` isn't `Debug`.
        f.debug_struct("GitHub").finish_non_exhaustive()
    }
}

/// Borrows a token: `GH_TOKEN`, then `GITHUB_TOKEN` (the variables `gh` itself honours), then `gh auth token`.
async fn borrow_token() -> anyhow::Result<SecretString> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        match env::var(name) {
            Ok(token) if !token.is_empty() => return Ok(token.into()),
            Ok(_) | Err(VarError::NotPresent) => {}
            Err(VarError::NotUnicode(_)) => bail!("{name} is set but isn't valid UTF-8"),
        }
    }

    let output = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .context(
            "no GitHub token: set GH_TOKEN, or install the `gh` CLI and run `gh auth login`",
        )?;
    if !output.status.success() {
        bail!(
            "no GitHub token: `gh auth token` failed ({}): {}; run `gh auth login`",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let token = String::from_utf8(output.stdout)
        .context("`gh auth token` printed a token that isn't UTF-8")?;
    Ok(token.trim().to_owned().into())
}

/// Adds a hint to an error that GitHub rejected the token for: notifications can only be read with a classic
/// personal access token or an OAuth app token such as `gh`'s.
fn explain_rejection(error: octocrab::Error) -> anyhow::Error {
    let token_rejected =
        matches!(&error, octocrab::Error::GitHub { source, .. } if is_token_rejection(source));
    let error = anyhow::Error::new(error);
    if token_rejected {
        error.context(
            "GitHub rejected the token: fine-grained personal access tokens can't read notifications; \
             use `gh auth login`, or a classic token with the `notifications` scope",
        )
    } else {
        error
    }
}

/// Whether GitHub refused the request because of the token, rather than because the rate limit ran out (which
/// GitHub also reports as 403).
fn is_token_rejection(error: &GitHubError) -> bool {
    let rate_limited = error
        .headers
        .as_ref()
        .and_then(|headers| headers.get("x-ratelimit-remaining"))
        .is_some_and(|remaining| remaining == "0");
    match error.status_code.as_u16() {
        401 => true,
        403 => !rate_limited,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    /// A notification as the API sends it.
    fn api_notification(id: u64, reason: &str, updated_at: &str) -> Value {
        json!({
            "id": id.to_string(),
            "unread": true,
            "reason": reason,
            "updated_at": updated_at,
            "last_read_at": null,
            "url": format!("https://api.github.com/notifications/threads/{id}"),
            "subject": {
                "title": format!("Subject {id}"),
                "url": format!("https://api.github.com/repos/wcgw/ghd/issues/{id}"),
                "latest_comment_url": null,
                "type": "Issue",
            },
            "repository": {
                "id": 1,
                "name": "ghd",
                "full_name": "wcgw/ghd",
                "url": "https://api.github.com/repos/wcgw/ghd",
            },
        })
    }

    async fn github_for(server: &MockServer) -> GitHub {
        GitHub::with_token("test-token".to_owned().into(), &server.uri(), GITHUB_WEB)
            .expect("mock server address is valid")
    }

    #[tokio::test]
    async fn subject_page_follows_the_api_url() {
        let github = GitHub::with_token("test-token".to_owned().into(), GITHUB_API, GITHUB_WEB)
            .expect("github.com addresses are valid");
        let cases = [
            (
                Some("/repos/o/r/pulls/12"),
                "https://github.com/o/r/pull/12",
            ),
            (
                Some("/repos/o/r/issues/7"),
                "https://github.com/o/r/issues/7",
            ),
            (
                Some("/repos/o/r/discussions/3"),
                "https://github.com/o/r/discussions/3",
            ),
            (
                Some("/repos/o/r/commits/abc123"),
                "https://github.com/o/r/commit/abc123",
            ),
            (
                Some("/repos/o/r/releases/987654"),
                "https://github.com/o/r/releases",
            ),
            (
                Some("/api/v3/repos/o/r/pulls/12"),
                "https://github.com/o/r/pull/12",
            ),
            (Some("/repos/o/r/check-suites/5"), "https://github.com/o/r"),
            (Some("/somewhere/else"), "https://github.com/o/r"),
            (None, "https://github.com/o/r"),
        ];
        for (api_path, expected) in cases {
            assert_eq!(
                github.subject_page(api_path, "o/r"),
                expected,
                "{api_path:?}"
            );
        }
    }

    #[tokio::test]
    async fn mark_read_and_done_target_the_thread() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/notifications/threads/42"))
            .respond_with(ResponseTemplate::new(205))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/notifications/threads/42"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let github = github_for(&server).await;

        github
            .mark_read(ThreadId(42))
            .await
            .expect("GitHub answers 205");
        github
            .mark_done(ThreadId(42))
            .await
            .expect("GitHub answers 204");
    }

    #[tokio::test]
    async fn refused_done_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/notifications/threads/42"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
            )
            .mount(&server)
            .await;

        let error = github_for(&server)
            .await
            .mark_done(ThreadId(42))
            .await
            .expect_err("GitHub answers 404");

        assert!(format!("{error:#}").contains("Not Found"), "{error:#}");
    }

    const LAST_MODIFIED_AT: &str = "Wed, 21 Oct 2026 18:51:47 GMT";

    /// A first Inbox page holding one notification, sent with [`LAST_MODIFIED_AT`].
    fn first_page() -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("last-modified", LAST_MODIFIED_AT)
            .set_body_json(json!([api_notification(
                1,
                "mention",
                "2026-10-21T08:00:00Z"
            )]))
    }

    /// The `If-Modified-Since` each request `server` received asked with, in order.
    async fn conditions(server: &MockServer) -> Vec<Option<String>> {
        server
            .received_requests()
            .await
            .expect("the mock server records requests")
            .iter()
            .map(|request| {
                let value = request.headers.get("if-modified-since")?;
                Some(value.to_str().expect("ASCII header").to_owned())
            })
            .collect()
    }

    #[tokio::test]
    async fn poll_interval_follows_what_github_asks() {
        let server = MockServer::start().await;
        let github = github_for(&server).await;
        assert_eq!(
            github.poll_interval(),
            Duration::from_secs(60),
            "before GitHub says"
        );

        Mock::given(method("GET"))
            .respond_with(first_page().insert_header("x-poll-interval", "90"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        github.inbox().await.expect("GitHub answers 200");
        assert_eq!(github.poll_interval(), Duration::from_secs(90));

        Mock::given(method("GET"))
            .respond_with(first_page().insert_header("x-poll-interval", "0"))
            .mount(&server)
            .await;
        github.inbox().await.expect("GitHub answers 200");
        assert_eq!(
            github.poll_interval(),
            Duration::from_secs(90),
            "0 isn't an interval"
        );
    }

    #[tokio::test]
    async fn unchanged_inbox_costs_one_conditional_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(first_page())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            // Not wiremock's `header`, which splits values at commas, and HTTP dates have one.
            .and(|request: &Request| {
                request
                    .headers
                    .get("if-modified-since")
                    .is_some_and(|value| value == LAST_MODIFIED_AT)
            })
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        let github = github_for(&server).await;

        github.inbox().await.expect("GitHub answers 200");
        let unchanged = github.inbox_if_changed().await.expect("GitHub answers 304");

        assert!(unchanged.is_none());
        assert_eq!(
            conditions(&server).await,
            [None, Some(LAST_MODIFIED_AT.to_owned())]
        );
    }

    #[tokio::test]
    async fn changed_inbox_is_fetched_whole() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(first_page())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param_is_missing("page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!(
                            r#"<{}/notifications?all=true&per_page=50&page=2>; rel="next""#,
                            server.uri()
                        ),
                    )
                    .set_body_json(json!([api_notification(
                        2,
                        "mention",
                        "2026-10-21T09:00:00Z"
                    )])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([api_notification(
                    1,
                    "mention",
                    "2026-10-21T08:00:00Z"
                )])),
            )
            .mount(&server)
            .await;
        let github = github_for(&server).await;

        github.inbox().await.expect("GitHub answers 200");
        let changed = github
            .inbox_if_changed()
            .await
            .expect("GitHub answers 200")
            .expect("the Inbox changed");

        assert_eq!(changed.iter().count(), 2);
        assert_eq!(
            conditions(&server).await,
            [None, Some(LAST_MODIFIED_AT.to_owned()), None],
            "only the first page is asked conditionally"
        );
    }

    #[tokio::test]
    async fn fetch_that_failed_half_way_is_retried_in_full() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param_is_missing("page"))
            .respond_with(first_page().insert_header(
                "link",
                format!(
                    r#"<{}/notifications?all=true&per_page=50&page=2>; rel="next""#,
                    server.uri()
                ),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let github = github_for(&server).await;

        github.inbox().await.expect_err("the second page fails");
        let inbox = github
            .inbox_if_changed()
            .await
            .expect("every page is served");

        assert!(inbox.is_some(), "fetched, not taken as unchanged");
        assert_eq!(conditions(&server).await, [None, None, None, None]);
    }

    #[tokio::test]
    async fn inbox_follows_every_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .and(query_param("all", "true"))
            .and(query_param("per_page", "50"))
            .and(query_param_is_missing("page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!(
                            r#"<{}/notifications?all=true&per_page=50&page=2>; rel="next""#,
                            server.uri()
                        ),
                    )
                    .set_body_json(json!([
                        api_notification(1, "review_requested", "2026-10-21T08:00:00Z"),
                        api_notification(2, "mention", "2026-10-21T09:00:00Z"),
                    ])),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([api_notification(
                    3,
                    "brand_new_reason",
                    "2026-10-21T10:00:00Z"
                )])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let inbox = github_for(&server)
            .await
            .inbox()
            .await
            .expect("both pages are served");

        let reasons: Vec<_> = inbox.iter().map(|n| n.reason.clone()).collect();
        assert_eq!(
            reasons,
            [
                Reason::Other("brand_new_reason".to_owned()),
                Reason::Mention,
                Reason::ReviewRequested,
            ]
        );
        let newest = inbox
            .iter()
            .next()
            .expect("the Inbox holds three Notifications");
        assert_eq!(newest.thread, ThreadId(3));
        assert_eq!(newest.subject_url, "https://github.com/wcgw/ghd/issues/3");
    }

    #[tokio::test]
    async fn rejected_token_explains_which_tokens_work() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message": "Resource not accessible by personal access token",
            })))
            .mount(&server)
            .await;

        let error = github_for(&server)
            .await
            .inbox()
            .await
            .expect_err("GitHub answers 403");

        let message = format!("{error:#}");
        assert!(message.contains("fine-grained"), "{message}");
        assert!(message.contains("Resource not accessible"), "{message}");
    }

    #[tokio::test]
    async fn exhausted_rate_limit_is_not_blamed_on_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .set_body_json(json!({ "message": "API rate limit exceeded" })),
            )
            .mount(&server)
            .await;

        let error = github_for(&server)
            .await
            .inbox()
            .await
            .expect_err("GitHub answers 403");

        let message = format!("{error:#}");
        assert!(!message.contains("fine-grained"), "{message}");
        assert!(message.contains("rate limit"), "{message}");
    }
}
