//! Changed files: what a pull request adds, changes, renames, copies or removes (see `CONTEXT.md`), fetched apart
//! from the Inbox.

use std::collections::HashMap;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};

use crate::fetched::Fetched;
use crate::inbox::{Listing, ThreadId};

/// The most of a file's text ghd shows: GitHub can send up to 100 MB of it.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// A file a pull request adds, changes, renames, copies or removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
    /// Where a renamed or copied file was; `None` for any other.
    pub previous_path: Option<String>,
    /// Lines added.
    pub additions: u64,
    /// Lines removed.
    pub deletions: u64,
    /// The unified diff, as GitHub gives it; `None` when it gives none: for a binary file, or one too large.
    pub patch: Option<String>,
    /// Where GitHub's REST API has the file as the pull request leaves it, as a path from the host, with its
    /// `?ref=`.
    pub contents_path: String,
}

/// What a pull request does to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    /// Changed in place: its contents, or only its mode.
    Modified,
    Renamed,
    Copied,
    Removed,
}

impl FileStatus {
    /// The status as the REST API names it; a status ghd doesn't know reads as modified.
    pub fn from_api(name: &str) -> Self {
        match name {
            "added" => Self::Added,
            "renamed" => Self::Renamed,
            "copied" => Self::Copied,
            "removed" => Self::Removed,
            // "modified", "changed" (its mode only), "unchanged", and any GitHub adds later.
            _ => Self::Modified,
        }
    }

    /// The letter the file list shows it by.
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::Removed => 'D',
        }
    }
}

impl ChangedFile {
    /// Whether it's Markdown, by its extension: `.md` or `.markdown`, ignoring case.
    pub fn is_markdown(&self) -> bool {
        let path = self.path.to_ascii_lowercase();
        path.ends_with(".md") || path.ends_with(".markdown")
    }

    /// The text of an added file, from its patch, where every line is added; `None` for any other file, or one
    /// GitHub gives no patch for.
    pub fn added_text(&self) -> Option<String> {
        if self.status != FileStatus::Added {
            return None;
        }
        let patch = self.patch.as_deref()?;
        let mut text = String::with_capacity(patch.len());
        let mut ends_with_new_line = true;
        for line in patch.lines() {
            if let Some(added) = line.strip_prefix('+') {
                text.push_str(added);
                text.push('\n');
            } else if line.starts_with('\\') {
                // "\ No newline at end of file", after the last line.
                ends_with_new_line = false;
            }
            // `@@` hunk headers.
        }
        if !ends_with_new_line {
            text.pop();
        }
        Some(text)
    }

    /// Its anchor in GitHub's "Files changed" tab: `diff-`, then the SHA-256 of its path in hex.
    pub fn anchor(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.path.as_bytes());
        let mut anchor = String::with_capacity(5 + 64);
        anchor.push_str("diff-");
        for byte in digest.as_ref() {
            // Writing to a String can't fail.
            let _ = write!(anchor, "{byte:02x}");
        }
        anchor
    }
}

/// A file's text, as a pull request leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileText {
    Text {
        /// At most [`MAX_TEXT_BYTES`] of it.
        text: String,
        /// There's more than `text`.
        truncated: bool,
    },
    /// Not text: it isn't UTF-8, or has a NUL in it.
    Binary,
}

impl FileText {
    /// What `text`, as fetched, shows as.
    pub fn of(text: String) -> Self {
        if text.contains('\0') {
            return Self::Binary;
        }
        if text.len() <= MAX_TEXT_BYTES {
            return Self::Text {
                text,
                truncated: false,
            };
        }
        let mut end = MAX_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self::Text {
            text: text[..end].to_owned(),
            truncated: true,
        }
    }
}

/// The Changed files fetched this run: each pull request's list, for the activity its Notification had then, and
/// the whole files fetched for them.
#[derive(Debug, Default)]
pub struct ChangedFiles {
    lists: HashMap<ThreadId, (DateTime<Utc>, Fetched<Vec<ChangedFile>>)>,
    /// By thread and the file's contents path, which has the commit it's at.
    texts: HashMap<(ThreadId, String), Fetched<FileText>>,
}

impl ChangedFiles {
    /// Where the list of `thread`'s Changed files stands, with the activity it has `as_of`; `None` if it's never
    /// been fetched, or only for older activity.
    pub fn list(
        &self,
        thread: ThreadId,
        as_of: DateTime<Utc>,
    ) -> Option<&Fetched<Vec<ChangedFile>>> {
        self.lists
            .get(&thread)
            .filter(|(fetched_as_of, _)| *fetched_as_of == as_of)
            .map(|(_, list)| list)
    }

    /// Notes that the list of `thread`'s Changed files, with the activity it has `as_of`, is being fetched.
    pub fn fetching_list(&mut self, thread: ThreadId, as_of: DateTime<Utc>) {
        self.lists.insert(thread, (as_of, Fetched::Fetching));
    }

    /// Takes in what fetching the list of `thread`'s Changed files, with the activity it had `as_of`, came to.
    pub fn fetched_list(
        &mut self,
        thread: ThreadId,
        as_of: DateTime<Utc>,
        list: anyhow::Result<Vec<ChangedFile>>,
    ) {
        self.lists
            .insert(thread, (as_of, Fetched::from_result(list)));
    }

    /// Where the whole text of `thread`'s file at `contents_path` stands; `None` if it's never been fetched.
    pub fn text(&self, thread: ThreadId, contents_path: &str) -> Option<&Fetched<FileText>> {
        self.texts.get(&(thread, contents_path.to_owned()))
    }

    /// Notes that the whole text of `thread`'s file at `contents_path` is being fetched.
    pub fn fetching_text(&mut self, thread: ThreadId, contents_path: &str) {
        self.texts
            .insert((thread, contents_path.to_owned()), Fetched::Fetching);
    }

    /// Takes in what fetching the whole text of `thread`'s file at `contents_path` came to.
    pub fn fetched_text(
        &mut self,
        thread: ThreadId,
        contents_path: &str,
        text: anyhow::Result<FileText>,
    ) {
        self.texts.insert(
            (thread, contents_path.to_owned()),
            Fetched::from_result(text),
        );
    }

    /// Forgets `thread`'s Changed files and their texts, so that they're fetched again.
    pub fn forget(&mut self, thread: ThreadId) {
        self.lists.remove(&thread);
        self.texts.retain(|(of, _), _| *of != thread);
    }

    /// Forgets the Changed files of threads neither of `listing`'s lists holds.
    pub fn retain_in(&mut self, listing: &Listing) {
        self.lists.retain(|&thread, _| listing.contains(thread));
        self.texts
            .retain(|(thread, _), _| listing.contains(*thread));
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use chrono::TimeDelta;

    use super::*;

    fn file(path: &str, status: FileStatus, patch: Option<&str>) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            status,
            previous_path: None,
            additions: 2,
            deletions: 0,
            patch: patch.map(str::to_owned),
            contents_path: format!("/repos/wcgw/ghd/contents/{path}?ref=abc"),
        }
    }

    fn then() -> DateTime<Utc> {
        "2026-10-22T12:00:00Z".parse().expect("valid RFC 3339")
    }

    #[test]
    fn statuses_come_from_the_api() {
        for (name, status, letter) in [
            ("added", FileStatus::Added, 'A'),
            ("modified", FileStatus::Modified, 'M'),
            ("changed", FileStatus::Modified, 'M'),
            ("renamed", FileStatus::Renamed, 'R'),
            ("copied", FileStatus::Copied, 'C'),
            ("removed", FileStatus::Removed, 'D'),
            ("brand_new", FileStatus::Modified, 'M'),
        ] {
            assert_eq!(FileStatus::from_api(name), status, "{name}");
            assert_eq!(status.letter(), letter);
        }
    }

    #[test]
    fn markdown_is_known_by_its_extension() {
        assert!(file("docs/README.md", FileStatus::Added, None).is_markdown());
        assert!(file("NOTES.MARKDOWN", FileStatus::Added, None).is_markdown());
        assert!(!file("src/md.rs", FileStatus::Added, None).is_markdown());
    }

    #[test]
    fn added_file_comes_whole_from_its_patch() {
        let added = file(
            "a.md",
            FileStatus::Added,
            Some("@@ -0,0 +1,3 @@\n+# Title\n+\n+Text"),
        );
        assert_eq!(added.added_text().as_deref(), Some("# Title\n\nText\n"));

        let no_new_line = file(
            "a.md",
            FileStatus::Added,
            Some("@@ -0,0 +1 @@\n+Text\n\\ No newline at end of file"),
        );
        assert_eq!(no_new_line.added_text().as_deref(), Some("Text"));

        assert_eq!(file("a.md", FileStatus::Added, None).added_text(), None);
        assert_eq!(
            file("a.md", FileStatus::Modified, Some("@@ -1 +1 @@\n-a\n+b")).added_text(),
            None,
            "not added"
        );
    }

    #[test]
    fn anchor_is_the_sha256_of_the_path() {
        // printf %s 'README.md' | sha256sum
        assert_eq!(
            file("README.md", FileStatus::Added, None).anchor(),
            "diff-b335630551682c19a781afebcf4d07bf978fb1f8ac04c6bf87428ed5106870f5"
        );
    }

    #[test]
    fn binary_and_long_texts_are_told_apart() {
        assert_eq!(
            FileText::of("plain".to_owned()),
            FileText::Text {
                text: "plain".to_owned(),
                truncated: false,
            }
        );
        assert_eq!(FileText::of("a\0b".to_owned()), FileText::Binary);
        let long = "é".repeat(MAX_TEXT_BYTES);
        let FileText::Text { text, truncated } = FileText::of(long) else {
            panic!("text");
        };
        assert!(truncated);
        assert!(text.len() <= MAX_TEXT_BYTES && text.len() > MAX_TEXT_BYTES - 2);
    }

    #[test]
    fn lists_hold_until_newer_activity_and_texts_go_with_them() {
        let mut files = ChangedFiles::default();
        let thread = ThreadId(7);
        files.fetching_list(thread, then());
        assert_eq!(files.list(thread, then()), Some(&Fetched::Fetching));
        files.fetched_list(
            thread,
            then(),
            Ok(vec![file("a.md", FileStatus::Added, None)]),
        );
        files.fetched_text(thread, "/c/a.md?ref=abc", Err(anyhow!("timed out")));
        assert!(matches!(files.list(thread, then()), Some(Fetched::Done(list)) if list.len() == 1));
        assert_eq!(
            files.list(thread, then() + TimeDelta::minutes(5)),
            None,
            "a new push, say"
        );

        files.forget(thread);

        assert_eq!(files.list(thread, then()), None);
        assert_eq!(files.text(thread, "/c/a.md?ref=abc"), None);
    }

    #[test]
    fn files_leave_with_their_notifications() {
        let mut files = ChangedFiles::default();
        files.fetched_list(ThreadId(7), then(), Ok(Vec::new()));
        files.fetched_text(ThreadId(7), "/c/a.md?ref=abc", Ok(FileText::Binary));

        files.retain_in(&Listing::default());

        assert_eq!(files.list(ThreadId(7), then()), None);
        assert_eq!(files.text(ThreadId(7), "/c/a.md?ref=abc"), None);
    }
}
