//! The user's GitHub notifications, through the REST API.

use std::env::{self, VarError};
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use chrono::{DateTime, Local};
use http::header::{HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE, LAST_MODIFIED};
use http::{StatusCode, Uri};
use octocrab::models::activity::Notification as ApiNotification;
use octocrab::secrecy::SecretString;
use octocrab::{FromResponse as _, GitHubError, Octocrab, Page};

use crate::inbox::{Inbox, Notification, Reason, SubjectKind, ThreadId};

/// github.com's hostname.
const GITHUB_HOST: &str = "github.com";

/// The REST API root for github.com.
const GITHUB_API: &str = "https://api.github.com";

/// The Inbox's first page: read and unread Notifications, as many per page as the API allows.
const INBOX_FIRST_PAGE: &str = "/notifications?all=true&per_page=50";

/// How long to wait between polls of the Inbox until GitHub says.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// GitHub's response header for the shortest time between polls of the Inbox, in seconds.
const X_POLL_INTERVAL: HeaderName = HeaderName::from_static("x-poll-interval");

/// The GitHub identity ghd acts as: a user on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// `github.com`, or a GitHub Enterprise Server's hostname.
    host: String,
    /// The user's login; `None` for whichever account `gh` has active on `host`.
    user: Option<String>,
}

impl Account {
    /// The account of `user` on `host`; github.com when `host` is `None`, and `gh`'s active account there when
    /// `user` is.
    ///
    /// # Errors
    ///
    /// Fails if `host` isn't a bare hostname (`github.example.com`, no scheme or path), is a GHE.com one (not
    /// supported yet), or if `user` is empty.
    pub fn new(host: Option<&str>, user: Option<&str>) -> anyhow::Result<Self> {
        let host = host.unwrap_or(GITHUB_HOST).to_ascii_lowercase();
        if host.is_empty() || host.contains("://") || host.contains(['/', ' ']) {
            bail!("`host` should be a hostname like `github.example.com`, not \"{host}\"");
        }
        if host == "ghe.com" || host.ends_with(".ghe.com") {
            bail!("GHE.com hosts like \"{host}\" aren't supported yet");
        }
        if user.is_some_and(str::is_empty) {
            bail!("`user` can't be empty; leave it out to use `gh`'s active account");
        }
        Ok(Self {
            host,
            user: user.map(str::to_owned),
        })
    }

    fn is_github_com(&self) -> bool {
        self.host == GITHUB_HOST
    }

    /// The REST API root, without a trailing `/`.
    fn api_root(&self) -> String {
        if self.is_github_com() {
            GITHUB_API.to_owned()
        } else {
            format!("https://{}/api/v3", self.host)
        }
    }

    /// The website root, without a trailing `/`.
    fn web_root(&self) -> String {
        format!("https://{}", self.host)
    }

    /// The environment variables that may hold a token for this host, in order of precedence: the ones `gh` reads
    /// for it, so that a github.com token is never sent to another host.
    fn token_variables(&self) -> [&'static str; 2] {
        if self.is_github_com() {
            ["GH_TOKEN", "GITHUB_TOKEN"]
        } else {
            ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
        }
    }

    /// The arguments to `gh` that print this account's token.
    fn gh_auth_token_args(&self) -> Vec<&str> {
        let mut args = vec!["auth", "token", "--hostname", &self.host];
        if let Some(user) = &self.user {
            args.extend(["--user", user]);
        }
        args
    }
}

/// github.com, as `gh`'s active account there.
impl Default for Account {
    fn default() -> Self {
        Self {
            host: GITHUB_HOST.to_owned(),
            user: None,
        }
    }
}

/// A GitHub API client acting as the user's Account.
pub struct GitHub {
    client: Octocrab,
    /// The host the Account is on, for advice in errors.
    host: String,
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
    /// A client acting as `account`, with a token borrowed from the environment: the variables `gh` reads for
    /// the account's host (`GH_TOKEN`, then `GITHUB_TOKEN` for github.com; `GH_ENTERPRISE_TOKEN`, then
    /// `GITHUB_ENTERPRISE_TOKEN` for others), then `gh auth token`. Makes no request to GitHub.
    ///
    /// # Errors
    ///
    /// Fails when none of those yields a token.
    pub async fn authenticate(account: &Account) -> anyhow::Result<Self> {
        Self::with_token(borrow_token(account).await?, account, &account.api_root())
    }

    /// A client for `account` sending `token` to the REST API rooted at `api_root`.
    fn with_token(token: SecretString, account: &Account, api_root: &str) -> anyhow::Result<Self> {
        let client = Octocrab::builder()
            .personal_token(token)
            .base_uri(api_root)
            .with_context(|| format!("invalid GitHub API address {api_root}"))?
            .build()
            .context("building the GitHub client")?;
        Ok(Self {
            client,
            host: account.host.clone(),
            web_root: account.web_root(),
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
            .map_err(|error| self.explain(error))?;
        self.note_poll_interval(response.headers());
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }

        let response = octocrab::map_github_error(response)
            .await
            .map_err(|error| self.explain(error))?;
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
            .map_err(|error| self.explain(error))
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
            .map_err(|error| self.explain(error))
    }

    /// Puts `error` in the user's terms: what GitHub answered, or that it couldn't be reached, and what to do about
    /// it where there's something to do.
    fn explain(&self, error: octocrab::Error) -> anyhow::Error {
        match error {
            octocrab::Error::GitHub { source, .. } => {
                let answer = format!(
                    "GitHub answered {}: {}",
                    source.status_code.as_u16(),
                    source.message
                );
                match self.advice(&source) {
                    Some(advice) => anyhow!("{answer}; {advice}"),
                    None => anyhow!(answer),
                }
            }
            error @ (octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. }) => {
                // The layers in between (octocrab's service, hyper's client) repeat themselves; the root cause
                // says what happened, like a failed DNS lookup or a refused connection.
                let mut cause: &dyn std::error::Error = &error;
                while let Some(source) = cause.source() {
                    cause = source;
                }
                anyhow!("couldn't reach {}: {cause}", self.host)
            }
            error => anyhow::Error::new(error),
        }
    }

    /// What the user can do about GitHub's `error`, if anything.
    fn advice(&self, error: &GitHubError) -> Option<String> {
        let rate_limited = error
            .headers
            .as_ref()
            .and_then(|headers| headers.get("x-ratelimit-remaining"))
            .is_some_and(|remaining| remaining == "0");
        match error.status_code.as_u16() {
            401 => Some(format!(
                "the token may have expired or been revoked: run `gh auth login --hostname {}`, or set a new one",
                self.host
            )),
            403 | 429 if rate_limited || error.status_code == StatusCode::TOO_MANY_REQUESTS => {
                let until = error
                    .rate_limit_reset
                    .and_then(|reset| DateTime::from_timestamp(i64::try_from(reset).ok()?, 0))
                    .map(|reset| format!(" until {}", reset.with_timezone(&Local).format("%H:%M")));
                Some(format!(
                    "the rate limit is used up{}",
                    until.unwrap_or_default()
                ))
            }
            403 => Some(
                "fine-grained personal access tokens and GitHub App tokens can't read notifications: run \
                 `gh auth login`, or use a classic token with the `notifications` scope"
                    .to_owned(),
            ),
            _ => None,
        }
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

/// Borrows a token for `account`; see [`GitHub::authenticate`].
async fn borrow_token(account: &Account) -> anyhow::Result<SecretString> {
    for name in account.token_variables() {
        match env::var(name) {
            Ok(token) if !token.is_empty() => return Ok(token.into()),
            Ok(_) | Err(VarError::NotPresent) => {}
            Err(VarError::NotUnicode(_)) => bail!("{name} is set but isn't valid UTF-8"),
        }
    }

    let [variable, _] = account.token_variables();
    let login = format!("gh auth login --hostname {}", account.host);
    let output = tokio::process::Command::new("gh")
        .args(account.gh_auth_token_args())
        .output()
        .await
        .with_context(|| {
            format!("no GitHub token: set {variable}, or install the `gh` CLI and run `{login}`")
        })?;
    if !output.status.success() {
        bail!(
            "no GitHub token: `gh {}` failed: {}; run `{login}`",
            account.gh_auth_token_args().join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let token = String::from_utf8(output.stdout)
        .context("`gh auth token` printed a token that isn't UTF-8")?;
    Ok(token.trim().to_owned().into())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
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
        GitHub::with_token(
            "test-token".to_owned().into(),
            &Account::default(),
            &server.uri(),
        )
        .expect("mock server address is valid")
    }

    #[tokio::test]
    async fn subject_page_follows_the_api_url() {
        let github = GitHub::with_token(
            "test-token".to_owned().into(),
            &Account::default(),
            GITHUB_API,
        )
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

        assert_eq!(
            format!("{error:#}"),
            "GitHub answered 403: Resource not accessible by personal access token; fine-grained personal access \
             tokens and GitHub App tokens can't read notifications: run `gh auth login`, or use a classic token \
             with the `notifications` scope"
        );
    }

    #[tokio::test]
    async fn bad_credentials_suggest_logging_in_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "message": "Bad credentials",
                "documentation_url": "https://docs.github.com/rest",
            })))
            .mount(&server)
            .await;

        let error = github_for(&server)
            .await
            .inbox()
            .await
            .expect_err("GitHub answers 401");

        assert_eq!(
            format!("{error:#}"),
            "GitHub answered 401: Bad credentials; the token may have expired or been revoked: run \
             `gh auth login --hostname github.com`, or set a new one"
        );
    }

    #[tokio::test]
    async fn unreachable_host_is_named() {
        // Nothing listens on the port once the listener is dropped. (A dropped wiremock server goes back to a pool
        // and keeps answering.)
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("a free port")
            .port();
        let address = format!("http://127.0.0.1:{port}");
        let github = GitHub::with_token(
            "test-token".to_owned().into(),
            &Account::default(),
            &address,
        )
        .expect("valid address");

        let error = github.inbox().await.expect_err("nothing answers");

        let message = format!("{error:#}");
        assert!(
            message.starts_with("couldn't reach github.com: "),
            "{message}"
        );
        assert!(!message.contains("Service Error"), "{message}");
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

        assert_eq!(
            format!("{error:#}"),
            "GitHub answered 403: API rate limit exceeded; the rate limit is used up"
        );
    }

    #[tokio::test]
    async fn rate_limit_says_when_it_resets() {
        let reset = Utc::now() + TimeDelta::minutes(30);
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("x-ratelimit-reset", reset.timestamp().to_string())
                    .set_body_json(
                        json!({ "message": "You have exceeded a secondary rate limit" }),
                    ),
            )
            .mount(&server)
            .await;

        let error = github_for(&server)
            .await
            .mark_done(ThreadId(1))
            .await
            .expect_err("GitHub answers 429");

        let until = reset.with_timezone(&Local).format("%H:%M").to_string();
        assert!(
            format!("{error:#}").ends_with(&format!("the rate limit is used up until {until}")),
            "{error:#}"
        );
    }

    #[test]
    fn github_com_account_uses_gh_defaults() {
        let account = Account::new(None, None).expect("github.com is valid");

        assert_eq!(account, Account::default());
        assert_eq!(account.api_root(), "https://api.github.com");
        assert_eq!(account.web_root(), "https://github.com");
        assert_eq!(account.token_variables(), ["GH_TOKEN", "GITHUB_TOKEN"]);
        assert_eq!(
            account.gh_auth_token_args(),
            ["auth", "token", "--hostname", "github.com"]
        );
    }

    #[test]
    fn enterprise_server_account_never_gets_a_github_com_token() {
        let account =
            Account::new(Some("GitHub.Example.com"), Some("alexsnaps")).expect("valid host");

        assert_eq!(account.api_root(), "https://github.example.com/api/v3");
        assert_eq!(account.web_root(), "https://github.example.com");
        assert_eq!(
            account.token_variables(),
            ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
        );
        assert_eq!(
            account.gh_auth_token_args(),
            [
                "auth",
                "token",
                "--hostname",
                "github.example.com",
                "--user",
                "alexsnaps"
            ]
        );
    }

    #[test]
    fn account_rejects_what_isnt_a_hostname() {
        for host in [
            "",
            "https://github.example.com",
            "github.example.com/api",
            "git hub.com",
            "octocorp.ghe.com",
        ] {
            assert!(Account::new(Some(host), None).is_err(), "{host:?}");
        }
        assert!(Account::new(None, Some("")).is_err());
    }
}
