//! What ghd keeps between runs, one file per Account, so that it can show the Inbox at once on startup while it
//! fetches the latest from GitHub.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::github::Account;
use crate::inbox::{Inbox, Notification, Reason, SubjectKind, ThreadId};
use crate::xdg;

/// The version of the cache file's format; a file of another version is ignored.
const FORMAT_VERSION: u32 = 1;

/// What ghd keeps between runs: the Inbox as it was, and where the user was in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub inbox: Inbox,
    pub saved_at: DateTime<Utc>,
    /// The name of the View that was on screen.
    pub view: String,
    /// The Notification at each View's cursor, by the View's name; a View showing nothing isn't there.
    pub cursors: BTreeMap<String, ThreadId>,
}

/// Where one Account's [`Snapshot`] is kept.
#[derive(Debug)]
pub struct Cache {
    /// `None` when there's nowhere to keep it: neither `XDG_CACHE_HOME` nor `HOME` is set.
    path: Option<PathBuf>,
}

impl Cache {
    /// The cache for `account`: `<host>/<user>.json` in ghd's cache directory (see [`xdg::cache_dir`]), with
    /// `@active` for the user when `account` is `gh`'s active one (no GitHub login has an `@`).
    pub fn for_account(account: &Account) -> Self {
        Self::in_dir(xdg::cache_dir().as_deref(), account)
    }

    fn in_dir(dir: Option<&Path>, account: &Account) -> Self {
        let file = format!("{}.json", account.user().unwrap_or("@active"));
        Self {
            path: dir.map(|dir| dir.join(account.host()).join(file)),
        }
    }

    /// The Snapshot saved last; `None` if there's none, or it can't be read or is from another version of ghd: it's
    /// only a cache.
    pub async fn load(&self) -> Option<Snapshot> {
        let bytes = tokio::fs::read(self.path.as_ref()?).await.ok()?;
        let file: File = serde_json::from_slice(&bytes).ok()?;
        (file.version == FORMAT_VERSION).then(|| file.into_snapshot())
    }

    /// Keeps `snapshot`, in place of the one there was. Only the user can read the file: it has the titles of
    /// private repositories' issues and pull requests.
    ///
    /// # Errors
    ///
    /// Fails if the file can't be written. Without anywhere to keep it, there's nothing to do and no error.
    pub async fn save(&self, snapshot: &Snapshot) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes =
            serde_json::to_vec(&File::from_snapshot(snapshot)).context("encoding the cache")?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        // Written beside it, then renamed over it, so that a crash half-way never leaves half a file.
        let partial = path.with_extension("json.partial");
        write_private(&partial, &bytes)
            .await
            .with_context(|| format!("writing {}", partial.display()))?;
        tokio::fs::rename(&partial, path)
            .await
            .with_context(|| format!("replacing {}", path.display()))
    }
}

/// Writes `bytes` to a new or truncated file at `path` that only the user can read and write.
async fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.flush().await
}

/// The cache file, as written.
#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    saved_at: DateTime<Utc>,
    view: String,
    cursors: BTreeMap<String, u64>,
    inbox: Vec<CachedNotification>,
}

/// A Notification, as written: its Reason and Subject kind by the API's names, so a new ghd still reads them.
#[derive(Serialize, Deserialize)]
struct CachedNotification {
    thread: u64,
    subject_url: String,
    repository: String,
    subject_kind: String,
    subject_title: String,
    reason: String,
    unread: bool,
    updated_at: DateTime<Utc>,
}

impl File {
    fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            version: FORMAT_VERSION,
            saved_at: snapshot.saved_at,
            view: snapshot.view.clone(),
            cursors: snapshot
                .cursors
                .iter()
                .map(|(view, thread)| (view.clone(), thread.0))
                .collect(),
            inbox: snapshot
                .inbox
                .iter()
                .map(|n| CachedNotification {
                    thread: n.thread.0,
                    subject_url: n.subject_url.clone(),
                    repository: n.repository.clone(),
                    subject_kind: n.subject_kind.api_name().to_owned(),
                    subject_title: n.subject_title.clone(),
                    reason: n.reason.api_name().to_owned(),
                    unread: n.unread,
                    updated_at: n.updated_at,
                })
                .collect(),
        }
    }

    fn into_snapshot(self) -> Snapshot {
        Snapshot {
            inbox: self
                .inbox
                .into_iter()
                .map(|n| Notification {
                    thread: ThreadId(n.thread),
                    subject_url: n.subject_url,
                    repository: n.repository,
                    subject_kind: SubjectKind::from_api(&n.subject_kind),
                    subject_title: n.subject_title,
                    reason: Reason::from_api(&n.reason),
                    unread: n.unread,
                    updated_at: n.updated_at,
                })
                .collect(),
            saved_at: self.saved_at,
            view: self.view,
            cursors: self
                .cursors
                .into_iter()
                .map(|(view, thread)| (view, ThreadId(thread)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own under the system's temporary one, removed when dropped.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ghd-{name}-{}", std::process::id()));
            // Left over from an earlier run that didn't clean up, if anything.
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            // Best effort: a leftover temporary directory is harmless.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot() -> Snapshot {
        let notification =
            |thread: u64, kind: SubjectKind, reason: Reason, unread: bool| Notification {
                thread: ThreadId(thread),
                subject_url: format!("https://github.com/wcgw/ghd/pull/{thread}"),
                repository: "wcgw/ghd".to_owned(),
                subject_kind: kind,
                subject_title: format!("Subject {thread}"),
                reason,
                unread,
                updated_at: format!("2026-10-21T1{thread}:00:00Z")
                    .parse()
                    .expect("valid RFC 3339"),
            };
        Snapshot {
            inbox: [
                notification(1, SubjectKind::PullRequest, Reason::ReviewRequested, true),
                notification(
                    2,
                    SubjectKind::Other("RepositoryAdvisory".to_owned()),
                    Reason::Other("brand_new_reason".to_owned()),
                    false,
                ),
            ]
            .into_iter()
            .collect(),
            saved_at: "2026-10-21T18:00:00Z".parse().expect("valid RFC 3339"),
            view: "Reviews".to_owned(),
            cursors: BTreeMap::from([
                ("Inbox".to_owned(), ThreadId(2)),
                ("Reviews".to_owned(), ThreadId(1)),
            ]),
        }
    }

    #[test]
    fn each_account_has_its_own_file() {
        let dir = Path::new("/cache/ghd");
        let path = |host, user| {
            Cache::in_dir(Some(dir), &Account::new(host, user).expect("valid account"))
                .path
                .expect("a cache directory")
        };

        assert_eq!(path(None, None), dir.join("github.com/@active.json"));
        assert_eq!(
            path(None, Some("alexsnaps")),
            dir.join("github.com/alexsnaps.json")
        );
        assert_eq!(
            path(Some("GitHub.Example.com"), None),
            dir.join("github.example.com/@active.json")
        );
        assert!(Cache::in_dir(None, &Account::default()).path.is_none());
    }

    #[tokio::test]
    async fn snapshot_comes_back_as_saved() {
        let dir = ScratchDir::new("cache-round-trip");
        let cache = Cache::in_dir(Some(&dir.0), &Account::default());

        assert_eq!(cache.load().await, None, "nothing saved yet");
        cache
            .save(&snapshot())
            .await
            .expect("the directory is writable");
        assert_eq!(cache.load().await, Some(snapshot()));

        let mut later = snapshot();
        later.view = "Inbox".to_owned();
        cache.save(&later).await.expect("the directory is writable");
        assert_eq!(cache.load().await, Some(later), "replaced");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = ScratchDir::new("cache-mode");
        let cache = Cache::in_dir(Some(&dir.0), &Account::default());
        cache
            .save(&snapshot())
            .await
            .expect("the directory is writable");

        let path = cache.path.as_ref().expect("a cache directory");
        let mode = std::fs::metadata(path).expect("saved").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn unreadable_or_other_version_cache_is_ignored() {
        let dir = ScratchDir::new("cache-ignored");
        let cache = Cache::in_dir(Some(&dir.0), &Account::default());
        let path = cache.path.clone().expect("a cache directory");
        std::fs::create_dir_all(path.parent().expect("in a directory"))
            .expect("the directory is writable");

        std::fs::write(&path, "{ not json").expect("the directory is writable");
        assert_eq!(cache.load().await, None);

        let mut other_version =
            serde_json::to_value(File::from_snapshot(&snapshot())).expect("encodable");
        other_version["version"] = (FORMAT_VERSION + 1).into();
        std::fs::write(&path, other_version.to_string()).expect("the directory is writable");
        assert_eq!(cache.load().await, None);
    }
}
