//! The user's GitHub notifications, through the REST API.

use std::env::{self, VarError};
use std::fmt;

use anyhow::{Context as _, bail};
use http::Uri;
use octocrab::models::activity::Notification as ApiNotification;
use octocrab::secrecy::SecretString;
use octocrab::{FromResponse as _, GitHubError, Octocrab, Page};

use crate::inbox::{Inbox, Notification, Reason, SubjectKind};

/// The REST API root for github.com.
const GITHUB_API: &str = "https://api.github.com";

/// The Inbox's first page: read and unread Notifications, as many per page as the API allows.
const INBOX_FIRST_PAGE: &str = "/notifications?all=true&per_page=50";

/// A GitHub API client acting as the user's Account.
pub struct GitHub {
    client: Octocrab,
}

impl GitHub {
    /// A client for github.com using a token borrowed from the environment: `GH_TOKEN`, then `GITHUB_TOKEN`, then
    /// `gh auth token` (the `gh` CLI's active account). Makes no request to GitHub.
    ///
    /// # Errors
    ///
    /// Fails when none of those yields a token.
    pub async fn authenticate() -> anyhow::Result<Self> {
        Self::with_token(borrow_token().await?, GITHUB_API)
    }

    /// A client sending `token` to the REST API rooted at `api_root`.
    fn with_token(token: SecretString, api_root: &str) -> anyhow::Result<Self> {
        let client = Octocrab::builder()
            .personal_token(token)
            .base_uri(api_root)
            .with_context(|| format!("invalid GitHub API address {api_root}"))?
            .build()
            .context("building the GitHub client")?;
        Ok(Self { client })
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

            notifications.extend(page.items.into_iter().map(notification_from_api));
            page_uri = page.next;
        }

        Ok(notifications.into_iter().collect())
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

/// Maps the API's representation of a Notification onto ghd's.
fn notification_from_api(api: ApiNotification) -> Notification {
    Notification {
        repository: api.repository.full_name.unwrap_or(api.repository.name),
        subject_kind: SubjectKind::from_api(&api.subject.r#type),
        subject_title: api.subject.title,
        reason: Reason::from_api(&api.reason),
        unread: api.unread,
        updated_at: api.updated_at,
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
        GitHub::with_token("test-token".to_owned().into(), &server.uri())
            .expect("mock server address is valid")
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
