//! Pull requests' Changed files, from the REST API.

use anyhow::Context as _;
use http::Uri;
use http::header::{ACCEPT, HeaderMap, HeaderValue};
use octocrab::{FromResponse as _, Page};
use serde::Deserialize;

use super::{GitHub, below_repos};
use crate::files::{ChangedFile, FileStatus, FileText};
use crate::inbox::Notification;

/// The first page of a pull request's files, below its REST path: as many per page as the API allows.
const FIRST_PAGE: &str = "/files?per_page=100";

/// The media type for a file's contents as they are, rather than base64 in JSON.
const RAW: HeaderValue = HeaderValue::from_static("application/vnd.github.raw+json");

/// Which pull request's Changed files to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesRequest {
    /// The pull request's path in the REST API.
    pull_path: String,
}

impl FilesRequest {
    /// The Changed files of `notification`'s Subject; `None` unless it's a pull request.
    pub fn for_notification(notification: &Notification) -> Option<Self> {
        let path = notification.api_paths.subject.as_deref()?;
        matches!(below_repos(path).as_slice(), [_, _, "pulls", _]).then(|| Self {
            pull_path: path.to_owned(),
        })
    }
}

impl GitHub {
    /// The Changed files `request` asks for, in GitHub's order, fetched page by page: as many as GitHub lists,
    /// 3000 at most. Only reads.
    ///
    /// # Errors
    ///
    /// Fails if any page can't be fetched.
    ///
    /// # Cancel Safety
    ///
    /// Cancel safe: it only reads.
    pub async fn changed_files(&self, request: &FilesRequest) -> anyhow::Result<Vec<ChangedFile>> {
        let mut files = Vec::new();
        let mut next = Some(format!("{}{FIRST_PAGE}", request.pull_path));
        while let Some(path) = next {
            let response = self
                .client
                ._get(path.as_str())
                .await
                .map_err(|error| self.explain(error))?;
            let response = octocrab::map_github_error(response)
                .await
                .map_err(|error| self.explain(error))?;
            let page = Page::<ApiDiffEntry>::from_response(response)
                .await
                .context("reading a pull request's files")?;
            files.extend(page.items.into_iter().map(ApiDiffEntry::into_changed_file));
            // A path, not the whole URL GitHub links to: octocrab only sends the token without a host.
            next = page.next.as_ref().map(path_and_query);
        }
        Ok(files)
    }

    /// The whole text of the file at `contents_path` (a [`ChangedFile::contents_path`]), as it is. Only reads.
    ///
    /// # Errors
    ///
    /// Fails if GitHub can't be reached, or doesn't give the file.
    ///
    /// # Cancel Safety
    ///
    /// Cancel safe: it only reads.
    pub async fn file_text(&self, contents_path: &str) -> anyhow::Result<FileText> {
        let response = self
            .client
            ._get_with_headers(contents_path, Some(HeaderMap::from_iter([(ACCEPT, RAW)])))
            .await
            .map_err(|error| self.explain(error))?;
        let response = octocrab::map_github_error(response)
            .await
            .map_err(|error| self.explain(error))?;
        match self.client.body_to_string(response).await {
            Ok(text) => Ok(FileText::of(text)),
            Err(octocrab::Error::InvalidUtf8 { .. }) => Ok(FileText::Binary),
            Err(error) => Err(self.explain(error)),
        }
    }
}

/// `uri`'s path, with its query.
fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query()
        .map_or_else(|| uri.path().to_owned(), |path| path.as_str().to_owned())
}

/// A pull request's file, as the REST API describes it.
#[derive(Deserialize)]
struct ApiDiffEntry {
    filename: String,
    status: String,
    previous_filename: Option<String>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    patch: Option<String>,
    contents_url: String,
}

impl ApiDiffEntry {
    fn into_changed_file(self) -> ChangedFile {
        let contents_path = self
            .contents_url
            .parse::<Uri>()
            .map_or_else(|_| self.contents_url.clone(), |uri| path_and_query(&uri));
        ChangedFile {
            path: self.filename,
            status: FileStatus::from_api(&self.status),
            previous_path: self.previous_filename,
            additions: self.additions,
            deletions: self.deletions,
            patch: self.patch,
            contents_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::tests::github_for;
    use super::*;
    use crate::inbox::{ApiPaths, Reason, SubjectKind, ThreadId};

    fn notification(subject: &str) -> Notification {
        Notification {
            thread: ThreadId(8),
            subject_url: "https://github.com/wcgw/ghd/pull/8".to_owned(),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::PullRequest,
            subject_title: "Add files".to_owned(),
            reason: Reason::ReviewRequested,
            unread: true,
            updated_at: Utc::now(),
            api_paths: ApiPaths {
                subject: Some(subject.to_owned()),
                latest_comment: None,
            },
        }
    }

    fn entry(server: &MockServer, filename: &str, status: &str) -> Value {
        json!({
            "filename": filename,
            "status": status,
            "additions": 3,
            "deletions": 1,
            "changes": 4,
            "patch": "@@ -1 +1 @@\n-a\n+b",
            "contents_url": format!("{}/repos/wcgw/ghd/contents/{filename}?ref=abc123", server.uri()),
            "blob_url": "https://github.com/wcgw/ghd/blob/abc123/x",
            "raw_url": "https://github.com/wcgw/ghd/raw/abc123/x",
            "sha": "def",
        })
    }

    #[test]
    fn only_pull_requests_have_files() {
        assert!(FilesRequest::for_notification(&notification("/repos/wcgw/ghd/pulls/8")).is_some());
        assert!(
            FilesRequest::for_notification(&notification("/repos/wcgw/ghd/issues/8")).is_none()
        );
    }

    #[tokio::test]
    async fn files_come_page_by_page_with_the_token() {
        let server = MockServer::start().await;
        let next = format!(
            "<{}/repos/wcgw/ghd/pulls/8/files?per_page=100&page=2>; rel=\"next\"",
            server.uri()
        );
        let mut renamed = entry(&server, "src/new.rs", "renamed");
        renamed["previous_filename"] = json!("src/old.rs");
        Mock::given(method("GET"))
            .and(path("/repos/wcgw/ghd/pulls/8/files"))
            .and(query_param("per_page", "100"))
            .and(query_param_is_missing("page"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", next.as_str())
                    .set_body_json(json!([renamed])),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/wcgw/ghd/pulls/8/files"))
            .and(query_param("page", "2"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([entry(
                &server,
                "docs/new.md",
                "added"
            )])))
            .expect(1)
            .mount(&server)
            .await;
        let request = FilesRequest::for_notification(&notification("/repos/wcgw/ghd/pulls/8"))
            .expect("a pull request");

        let files = github_for(&server)
            .await
            .changed_files(&request)
            .await
            .expect("both pages are served");

        assert_eq!(
            files
                .iter()
                .map(|file| (file.path.as_str(), file.status))
                .collect::<Vec<_>>(),
            [
                ("src/new.rs", FileStatus::Renamed),
                ("docs/new.md", FileStatus::Added)
            ]
        );
        assert_eq!(files[0].previous_path.as_deref(), Some("src/old.rs"));
        assert_eq!(
            files[1].contents_path, "/repos/wcgw/ghd/contents/docs/new.md?ref=abc123",
            "a path, sent with the token"
        );
    }

    #[tokio::test]
    async fn whole_files_come_raw() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/wcgw/ghd/contents/docs/new.md"))
            .and(query_param("ref", "abc123"))
            .and(header("accept", "application/vnd.github.raw+json"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# New\n\nText\n"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/wcgw/ghd/contents/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0x89, b'P', 0xff, 0xfe]))
            .mount(&server)
            .await;
        let github = github_for(&server).await;

        let text = github
            .file_text("/repos/wcgw/ghd/contents/docs/new.md?ref=abc123")
            .await
            .expect("served");
        let image = github
            .file_text("/repos/wcgw/ghd/contents/logo.png?ref=abc123")
            .await
            .expect("served");

        assert_eq!(
            text,
            FileText::Text {
                text: "# New\n\nText\n".to_owned(),
                truncated: false,
            }
        );
        assert_eq!(image, FileText::Binary);
    }
}
