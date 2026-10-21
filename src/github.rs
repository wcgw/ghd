//! The user's GitHub notifications, through the REST API.

use std::env::{self, VarError};
use std::fmt;

use anyhow::{Context as _, bail};
use http::Uri;
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

/// A GitHub API client acting as the user's Account.
pub struct GitHub {
    client: Octocrab,
    /// Where the website lives, without a trailing `/`; Subjects' pages are below it.
    web_root: String,
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
        })
    }

    /// The whole Inbox, read and unread, fetched page by page.
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
        let mut notifications = Vec::new();
        let mut page_uri = Some(Uri::from_static(INBOX_FIRST_PAGE));

        // Pages are fetched with octocrab's raw GET rather than its typed builder: polling needs the response's
        // `Last-Modified`, `X-Poll-Interval` and 304 status, which the typed builder hides.
        while let Some(uri) = page_uri {
            let response = self
                .client
                ._get_with_headers(uri, None)
                .await
                .context("fetching notifications")?;
            let response = octocrab::map_github_error(response)
                .await
                .map_err(explain_rejection)?;
            let page = Page::<ApiNotification>::from_response(response)
                .await
                .context("reading notifications")?;

            notifications.extend(
                page.items
                    .into_iter()
                    .map(|api| self.notification_from_api(api)),
            );
            page_uri = page.next;
        }

        Ok(notifications.into_iter().collect())
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
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
