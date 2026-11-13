//! The Files screen: a pull request's Changed files in the list's window, the one at its cursor in the window below,
//! whole or as its diff.
//!
//! Each pull request's list and each file, as it shows, gets a buffer of its own, so that coming back to one finds it
//! where it was left; a file shown for the first time shows from its top.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use unicode_width::UnicodeWidthStr;
use wcgw_kit::BufferId;
use wcgw_kit::doc::{BlockKey, Content, Cursor, Document, Line, Span};
use wcgw_kit::shell::{Placement, Size, WindowId};

use super::{Effect, Effects, Ghd, Io, Kit, PREVIEW_MIN_HEIGHT, Target, counted, in_window, list};
use crate::assessment;
use crate::fetched::Fetched;
use crate::files::{ChangedFile, ChangedFiles, FileStatus, FileText, LineChange};
use crate::github::FilesRequest;
use crate::hl::{self, Group};
use crate::inbox::{Listing, Notification, ThreadId};
use crate::markdown::{self, Look};
use crate::syntax::{self, Classes, Highlighting};

/// How many lines past those shown are highlighted ahead, so that scrolling a little finds them done.
const HIGHLIGHT_AHEAD: usize = 100;

/// How many cells wide the gutter of a file shown whole is: its sign, and a space.
const GUTTER_WIDTH: u16 = 2;

/// How a Changed file shows: whole, or as its diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Showing {
    Whole,
    Diff,
}

/// The Files screen, open or not, and what was fetched and drawn for it this run, which outlasts it being closed and
/// opened again.
#[derive(Debug, Default)]
pub(super) struct Files {
    screen: Option<Screen>,
    /// Each pull request's list of files.
    lists: HashMap<ThreadId, BufferId>,
    /// Each file shown, as it shows, by its pull request and contents path.
    drawn: HashMap<(ThreadId, String, Showing), FileDrawing>,
    /// The buffer saying why there's no file to show.
    placeholder: Option<BufferId>,
    fetched: ChangedFiles,
    /// A list's fetch is under way; there's never more than one.
    list_fetching: bool,
    /// A whole file's fetch is under way; there's never more than one.
    text_fetching: bool,
    /// How many buffers were made, to name the next.
    made: usize,
}

/// The Files screen while it's open: which pull request it is of, and where it shows.
#[derive(Debug)]
struct Screen {
    /// The pull request's Notification, as it was when the screen opened.
    notification: Notification,
    request: FilesRequest,
    /// The list's buffer.
    list: BufferId,
    /// The window below, where files show.
    window: WindowId,
    /// The window was the Preview's, to give back.
    lent: bool,
    /// How the files that can show either way show; a new one always shows whole, and a removed one as its diff.
    showing: Showing,
    /// What the list was drawn for: whether its files had arrived, and how many.
    list_stamp: Option<(u8, usize)>,
}

/// A file's buffer, its highlighting so far, and what it was drawn for.
#[derive(Debug)]
struct FileDrawing {
    buffer: BufferId,
    /// `None` for a file in a language ghd doesn't know, or Markdown, which is highlighted as it's drawn.
    highlighting: Option<Highlighting>,
    /// The width, the Heatmap's version, whether its text arrived, and how many lines were highlighted.
    stamp: Option<(u16, u64, u8, usize)>,
}

impl Files {
    /// Whether the screen is open.
    pub(super) fn is_open(&self) -> bool {
        self.screen.is_some()
    }

    /// The window below, where files show, while the screen is open.
    pub(super) fn window(&self) -> Option<WindowId> {
        self.screen.as_ref().map(|screen| screen.window)
    }

    /// The user closed a window the screen needs: it's closed.
    pub(super) fn closed(&mut self) {
        self.screen = None;
    }

    /// Forgets the open screen's Changed files, so that they're fetched again; returns its thread.
    pub(super) fn forget(&mut self) -> Option<ThreadId> {
        let thread = self.screen.as_ref()?.notification.thread;
        self.fetched.forget(thread);
        self.drawn.retain(|(of, ..), _| *of != thread);
        if let Some(screen) = &mut self.screen {
            screen.list_stamp = None;
        }
        Some(thread)
    }

    /// Takes in a pull request's Changed files, or why they couldn't be fetched.
    pub(super) fn took_list(
        &mut self,
        thread: ThreadId,
        as_of: DateTime<Utc>,
        files: Result<Vec<ChangedFile>, String>,
    ) {
        self.list_fetching = false;
        self.fetched
            .fetched_list(thread, as_of, files.map_err(|error| anyhow::anyhow!(error)));
    }

    /// Takes in a whole file, or why it couldn't be fetched.
    pub(super) fn took_text(
        &mut self,
        thread: ThreadId,
        contents_path: &str,
        text: Result<FileText, String>,
    ) {
        self.text_fetching = false;
        self.fetched.fetched_text(
            thread,
            contents_path,
            text.map_err(|error| anyhow::anyhow!(error)),
        );
    }

    /// Forgets the Changed files of threads neither of `listing`'s lists holds.
    pub(super) fn retain_in(&mut self, listing: &Listing) {
        self.fetched.retain_in(listing);
    }

    /// The open screen's Changed files, once fetched.
    fn files(&self) -> Option<&[ChangedFile]> {
        let screen = self.screen.as_ref()?;
        let n = &screen.notification;
        match self.fetched.list(n.thread, n.updated_at)? {
            Fetched::Done(list) => Some(list),
            Fetched::Fetching | Fetched::Failed(_) => None,
        }
    }

    /// Whether `file` shows whole rather than as its diff: a new one, or any but a removed one once `:whole` asks
    /// for files whole.
    fn showing(&self, file: &ChangedFile) -> Showing {
        match file.status {
            FileStatus::Added => Showing::Whole,
            FileStatus::Removed => Showing::Diff,
            FileStatus::Modified | FileStatus::Renamed | FileStatus::Copied => self
                .screen
                .as_ref()
                .map_or(Showing::Diff, |screen| screen.showing),
        }
    }

    /// The whole text of `file` of `thread`: from its patch when it's new, else as fetched.
    fn text<'a>(
        &'a self,
        thread: ThreadId,
        file: &ChangedFile,
    ) -> Option<Fetched<(std::borrow::Cow<'a, str>, bool)>> {
        if let Some(text) = file.added_text() {
            return Some(Fetched::Done((std::borrow::Cow::Owned(text), false)));
        }
        Some(match self.fetched.text(thread, &file.contents_path)? {
            Fetched::Fetching => Fetched::Fetching,
            Fetched::Failed(why) => Fetched::Failed(why.clone()),
            Fetched::Done(FileText::Binary) => Fetched::Failed(String::new()),
            Fetched::Done(FileText::Text { text, truncated }) => {
                Fetched::Done((std::borrow::Cow::Borrowed(text.as_str()), *truncated))
            }
        })
    }
}

impl Ghd {
    /// `:files` (`gf`): shows the Changed files of the pull request at the list's cursor, or goes back to the list.
    pub(super) fn toggle_files(&mut self, kit: &mut Kit) {
        if self.files.is_open() {
            self.leave_files(kit);
            return;
        }
        let Some(notification) = self
            .at_cursor(kit)
            .and_then(|thread| self.notification(thread))
            .cloned()
        else {
            return;
        };
        let Some(request) = FilesRequest::for_notification(&notification) else {
            kit.info("Only pull requests have files");
            return;
        };
        if kit.size().1 < PREVIEW_MIN_HEIGHT {
            kit.info(format!(
                "Files need a terminal at least {PREVIEW_MIN_HEIGHT} lines tall"
            ));
            return;
        }
        let thread = notification.thread;
        let list = if let Some(&list) = self.files.lists.get(&thread) {
            list
        } else {
            let number = notification
                .subject_url
                .rsplit('/')
                .next()
                .unwrap_or_default();
            let list = kit.add_buffer(format!("files #{number}"), Document::new(), false);
            self.files.lists.insert(thread, list);
            list
        };
        let placeholder = self.files_placeholder(kit);
        let (window, lent) = if let Some(window) = self.preview.lend() {
            (window, true)
        } else {
            let window = in_window(kit, self.list_window, |kit| {
                kit.open_window(Placement::Below, Size::Percent(75), placeholder)
            })
            .unwrap_or_else(|| kit.current_window());
            (window, false)
        };
        kit.focus_window(self.list_window);
        kit.show_buffer(list);
        self.files.screen = Some(Screen {
            notification,
            request,
            list,
            window,
            lent,
            showing: Showing::Diff,
            list_stamp: None,
        });
    }

    /// Closes the Files screen: the list shows the View again, and the window below the Preview, if it was the
    /// Preview's, or goes.
    pub(super) fn leave_files(&mut self, kit: &mut Kit) {
        let Some(screen) = self.files.screen.take() else {
            return;
        };
        let view = self.views[self.current].buffer;
        if kit.window_buffer(self.list_window) != Some(view) {
            kit.show_buffer_in(self.list_window, view);
        }
        kit.set_window_size(self.list_window, Size::Shared);
        if kit.window_ids().contains(&screen.window) {
            if screen.lent {
                kit.set_window_size(screen.window, Size::Percent(75));
                self.preview.take_back(screen.window);
            } else {
                kit.close_window_id(screen.window);
            }
        }
        kit.focus_window(self.list_window);
    }

    /// `:whole` (`<leader>w`): shows the files whole, or as their diffs again.
    pub(super) fn toggle_whole(&mut self) -> Result<Effects, String> {
        let screen = self
            .files
            .screen
            .as_mut()
            .ok_or_else(|| "Files show whole or as diffs on the Files screen".to_owned())?;
        screen.showing = match screen.showing {
            Showing::Whole => Showing::Diff,
            Showing::Diff => Showing::Whole,
        };
        Ok(Vec::new())
    }

    /// The Changed file at the cursor of the Files screen's list, by its place in the list.
    fn selected_file(&self, kit: &mut Kit) -> Option<usize> {
        let screen = self.files.screen.as_ref()?;
        let thread = screen.notification.thread;
        in_window(kit, self.list_window, |kit| {
            match kit.targets_under_cursor()[1] {
                Some(Target::File(of, index)) if *of == thread => Some(*index),
                _ => None,
            }
        })
        .flatten()
    }

    /// Where GitHub shows the Changed file at the cursor: in the pull request's "Files changed" tab, at the file; or
    /// the tab itself while the files aren't fetched; and the pull request's thread, which opening it marks read.
    pub(super) fn selected_file_url(&self, kit: &mut Kit) -> Option<(String, ThreadId)> {
        let screen = self.files.screen.as_ref()?;
        let page = &screen.notification.subject_url;
        let file = self
            .selected_file(kit)
            .and_then(|index| self.files.files()?.get(index));
        let url = match file {
            Some(file) => format!("{page}/files#{}", file.anchor()),
            None => format!("{page}/files"),
        };
        Some((url, screen.notification.thread))
    }

    /// The buffer saying why there's no file to show.
    fn files_placeholder(&mut self, kit: &mut Kit) -> BufferId {
        if let Some(placeholder) = self.files.placeholder {
            return placeholder;
        }
        let placeholder = kit.add_buffer("file", Document::new(), false);
        self.files.placeholder = Some(placeholder);
        placeholder
    }

    /// Brings the Files screen up to date: the list as fetched, the file at its cursor below, fetching what they
    /// need.
    pub(super) fn sync_files(&mut self, kit: &mut Kit) -> Effects {
        let Some(screen) = &self.files.screen else {
            return Vec::new();
        };
        if kit.window_buffer(self.list_window) != Some(screen.list) {
            // The list's window shows something else now: the user went elsewhere.
            self.leave_files(kit);
            return Vec::new();
        }
        let mut effects = self.files_to_fetch();
        self.draw_list(kit);
        self.size_list(kit);
        let Some(index) = self.selected_file(kit) else {
            let says = match self.files_list_state() {
                Some(Fetched::Failed(_)) | None => "",
                Some(Fetched::Fetching) => "Fetching files…",
                Some(Fetched::Done(())) => "No files.",
            };
            self.show_file_placeholder(kit, says);
            return effects;
        };
        let Some(file) = self
            .files
            .files()
            .and_then(|files| files.get(index))
            .cloned()
        else {
            return effects;
        };
        effects.extend(self.text_to_fetch(&file));
        self.show_file(kit, &file);
        effects
    }

    /// Where the open screen's list stands: fetching, fetched, or failed.
    fn files_list_state(&self) -> Option<Fetched<()>> {
        let screen = self.files.screen.as_ref()?;
        let n = &screen.notification;
        Some(match self.files.fetched.list(n.thread, n.updated_at)? {
            Fetched::Fetching => Fetched::Fetching,
            Fetched::Done(_) => Fetched::Done(()),
            Fetched::Failed(why) => Fetched::Failed(why.clone()),
        })
    }

    /// The Changed files to fetch now, noted as being fetched: the screen's, unless they're fetched already.
    fn files_to_fetch(&mut self) -> Effects {
        let Some(screen) = &self.files.screen else {
            return Vec::new();
        };
        let (thread, as_of) = (screen.notification.thread, screen.notification.updated_at);
        if self.files.list_fetching || self.files.fetched.list(thread, as_of).is_some() {
            return Vec::new();
        }
        let request = screen.request.clone();
        self.files.fetched.fetching_list(thread, as_of);
        self.files.list_fetching = true;
        vec![Effect::App(Io::FetchFiles {
            thread,
            as_of,
            request,
        })]
    }

    /// The whole file to fetch now, noted as being fetched: `file`, when it's shown whole and there's no patch to
    /// show it from.
    fn text_to_fetch(&mut self, file: &ChangedFile) -> Effects {
        let Some(screen) = &self.files.screen else {
            return Vec::new();
        };
        let thread = screen.notification.thread;
        if self.files.showing(file) != Showing::Whole
            || file.added_text().is_some()
            || self.files.text_fetching
            || self
                .files
                .fetched
                .text(thread, &file.contents_path)
                .is_some()
        {
            return Vec::new();
        }
        self.files
            .fetched
            .fetching_text(thread, &file.contents_path);
        self.files.text_fetching = true;
        vec![Effect::App(Io::FetchText {
            thread,
            contents_path: file.contents_path.clone(),
        })]
    }

    /// Draws the list again when its files arrived: the pull request in its winbar, a row per file.
    fn draw_list(&mut self, kit: &mut Kit) {
        let Some(screen) = &self.files.screen else {
            return;
        };
        let n = &screen.notification;
        let fetched = self.files.fetched.list(n.thread, n.updated_at);
        let stamp = match fetched {
            None | Some(Fetched::Fetching) => (0, 0),
            Some(Fetched::Failed(_)) => (1, 0),
            Some(Fetched::Done(list)) => (2, list.len()),
        };
        if screen.list_stamp == Some(stamp) {
            return;
        }
        let number = n.subject_url.rsplit('/').next().unwrap_or_default();
        let mut winbar = vec![
            (format!(" #{number} "), hl::AGE),
            (n.subject_title.clone(), hl::PREVIEW_TITLE),
        ];
        let blocks = match fetched {
            None | Some(Fetched::Fetching) => list::message("Fetching files…", hl::MESSAGE),
            Some(Fetched::Failed(why)) => {
                list::message(&format!("Couldn't fetch the files: {why}"), hl::ERROR)
            }
            Some(Fetched::Done(files)) if files.is_empty() => {
                list::message("No files.", hl::MESSAGE)
            }
            Some(Fetched::Done(files)) => {
                let added: u64 = files.iter().map(|file| file.additions).sum();
                let removed: u64 = files.iter().map(|file| file.deletions).sum();
                winbar.extend([
                    (
                        format!(" · {} · ", counted(files.len() as u64, "file")),
                        Group::WIN_BAR,
                    ),
                    (format!("+{added}"), Group::ADDED),
                    (" ".to_owned(), Group::WIN_BAR),
                    (format!("\u{2212}{removed} "), Group::REMOVED),
                ]);
                file_rows(n.thread, files, kit.size().0)
            }
        };
        let list = screen.list;
        let shown = kit.buffer_mut(list);
        shown.set_winbar(Some(winbar));
        shown.doc_mut().set_blocks(blocks);
        if let Some(screen) = &mut self.files.screen {
            screen.list_stamp = Some(stamp);
        }
    }

    /// Gives the list no more rows than it takes to show every file, up to a quarter of the screen, the file below
    /// taking the rest; a quarter while there are none.
    fn size_list(&self, kit: &mut Kit) {
        let Some(screen) = &self.files.screen else {
            return;
        };
        // Below the tabline, above the statusline and the command line.
        let quarter = kit.size().1.saturating_sub(3).div_ceil(4);
        // Each file's row, the winbar, and the line between the windows.
        let rows = self
            .files
            .files()
            .filter(|files| !files.is_empty())
            .map_or(quarter, |files| {
                u16::try_from(files.len())
                    .unwrap_or(u16::MAX)
                    .saturating_add(2)
                    .min(quarter)
            });
        kit.set_window_size(self.list_window, Size::Cells(rows.max(3)));
        kit.set_window_size(screen.window, Size::Shared);
    }

    /// Shows the placeholder below, saying `says`.
    fn show_file_placeholder(&mut self, kit: &mut Kit, says: &str) {
        let Some(window) = self.files.window() else {
            return;
        };
        let placeholder = self.files_placeholder(kit);
        let shown = kit.buffer_mut(placeholder);
        shown.set_winbar(None);
        shown.doc_mut().set_blocks(list::message(says, hl::MESSAGE));
        if kit.window_buffer(window) != Some(placeholder) {
            kit.show_buffer_in(window, placeholder);
        }
    }

    /// Shows `file` below, from its own buffer, drawn again when the width, the Heatmap, its text or how far it's
    /// highlighted changed.
    fn show_file(&mut self, kit: &mut Kit, file: &ChangedFile) {
        let Some(screen) = &self.files.screen else {
            return;
        };
        let (thread, window) = (screen.notification.thread, screen.window);
        let showing = self.files.showing(file);
        let key = (thread, file.contents_path.clone(), showing);
        if !self.files.drawn.contains_key(&key) {
            self.files.made += 1;
            let buffer =
                kit.add_buffer(format!("file {}", self.files.made), Document::new(), false);
            let highlighting = None;
            self.files.drawn.insert(
                key.clone(),
                FileDrawing {
                    buffer,
                    highlighting,
                    stamp: None,
                },
            );
        }
        let buffer = self.files.drawn[&key].buffer;
        if kit.window_buffer(window) != Some(buffer) {
            in_window(kit, window, |kit| kit.show_buffer_at(buffer, Cursor::new()));
        }
        // As far down as it shows, and the window's height more.
        let height = usize::from(kit.size().1);
        let top = in_window(kit, window, |kit| kit.top_row(buffer))
            .flatten()
            .unwrap_or(0);
        let down_to = top + height + HIGHLIGHT_AHEAD;
        self.highlight(file, &key, showing, down_to);
        let text_state = match self.files.text(thread, file) {
            None | Some(Fetched::Fetching) => 0,
            Some(Fetched::Failed(_)) => 1,
            Some(Fetched::Done(_)) => 2,
        };
        let drawing = &self.files.drawn[&key];
        let highlighted = drawing
            .highlighting
            .as_ref()
            .map_or(0, |highlighting| highlighting.lines().len());
        let stamp = (kit.size().0, self.heat_version, text_state, highlighted);
        if drawing.stamp == Some(stamp) {
            return;
        }
        let lines = self.file_lines(kit, thread, file, showing);
        let shown = kit.buffer_mut(buffer);
        shown.set_winbar(Some(vec![(format!(" {} ", file.path), Group::WIN_BAR)]));
        shown
            .doc_mut()
            .set_blocks([(BlockKey::new("file"), Content::from(lines))]);
        if let Some(drawing) = self.files.drawn.get_mut(&key) {
            drawing.stamp = Some(stamp);
        }
    }

    /// Highlights `file`, shown as `showing`, down to line `down_to`, if ghd knows its language.
    fn highlight(
        &mut self,
        file: &ChangedFile,
        key: &(ThreadId, String, Showing),
        showing: Showing,
        down_to: usize,
    ) {
        if file.is_markdown() && showing == Showing::Whole {
            return;
        }
        let thread = key.0;
        let text = match showing {
            Showing::Diff => file.patch.clone(),
            Showing::Whole => match self.files.text(thread, file) {
                Some(Fetched::Done((text, _))) => Some(text.into_owned()),
                _ => None,
            },
        };
        let Some(drawing) = self.files.drawn.get_mut(key) else {
            return;
        };
        if drawing.highlighting.is_none()
            && let Some(text) = text
        {
            drawing.highlighting = match showing {
                Showing::Whole => Highlighting::of_file(&file.path, &text),
                Showing::Diff => Highlighting::of_diff(&file.path, &text),
            };
        }
        if let Some(highlighting) = &mut drawing.highlighting {
            highlighting.down_to(down_to);
        }
    }

    /// What `file` of `thread` shows as `showing`: where it was renamed or copied from, then the file whole, or its
    /// diff, or why there's neither.
    fn file_lines(
        &self,
        kit: &Kit,
        thread: ThreadId,
        file: &ChangedFile,
        showing: Showing,
    ) -> Vec<Line<Target>> {
        let mut lines = Vec::new();
        match (file.status, &file.previous_path) {
            (FileStatus::Renamed, Some(previous)) => {
                lines.push(said(&format!("Renamed from {previous}"), hl::AGE));
            }
            (FileStatus::Copied, Some(previous)) => {
                lines.push(said(&format!("Copied from {previous}"), hl::AGE));
            }
            _ => {}
        }
        let highlighted = self
            .files
            .drawn
            .get(&(thread, file.contents_path.clone(), showing))
            .and_then(|drawing| drawing.highlighting.as_ref())
            .map(Highlighting::lines);
        let what = match &file.patch {
            _ if file.status == FileStatus::Removed => {
                vec![said(
                    &format!("Deleted: {}", counted(file.deletions, "line")),
                    hl::AGE,
                )]
            }
            _ if showing == Showing::Whole => self.whole_lines(kit, thread, file, highlighted),
            Some(patch) => diff_lines(kit, patch, highlighted),
            // Renamed or copied as it was: that's all there is to say.
            None if file.additions + file.deletions == 0 && !lines.is_empty() => Vec::new(),
            None => vec![said(
                "No diff from GitHub (binary, or too large): gX opens it.",
                hl::MESSAGE,
            )],
        };
        if !lines.is_empty() && !what.is_empty() {
            lines.push(Line::text(""));
        }
        lines.extend(what);
        lines
    }

    /// `file` of `thread`, whole: rendered if it's Markdown, as its Heatmap while that shows, or else highlighted as
    /// far as `highlighted` goes; with a gutter showing what the pull request did to its lines, when its patch says.
    /// Or why it can't be shown yet, or at all.
    fn whole_lines(
        &self,
        kit: &Kit,
        thread: ThreadId,
        file: &ChangedFile,
        highlighted: Option<&[Classes]>,
    ) -> Vec<Line<Target>> {
        let (text, truncated) = match self.files.text(thread, file) {
            None | Some(Fetched::Fetching) => return vec![said("Fetching…", hl::MESSAGE)],
            Some(Fetched::Failed(why)) if why.is_empty() => {
                return vec![said("Binary: gX opens it.", hl::MESSAGE)];
            }
            Some(Fetched::Failed(why)) => {
                return vec![said(&format!("Couldn't fetch the file: {why}"), hl::ERROR)];
            }
            Some(Fetched::Done(done)) => done,
        };
        if text.trim().is_empty() {
            return vec![said("Empty file.", hl::MESSAGE)];
        }
        let changes = file.line_changes();
        // No gutter without a patch to say what changed.
        let gutter = !changes.is_empty();
        let (drawn, sources): (Vec<Line<Target>>, Vec<Option<usize>>) = if file.is_markdown() {
            let theme = kit.theme();
            let width = kit
                .size()
                .0
                .saturating_sub(if gutter { GUTTER_WIDTH } else { 0 });
            let key = assessment::Key {
                thread,
                as_of: self
                    .files
                    .screen
                    .as_ref()
                    .map_or_else(Utc::now, |screen| screen.notification.updated_at),
                part: assessment::Part::File(file.contents_path.clone()),
            };
            let look = Look::of(theme, super::preview::heat(theme));
            let drawing = markdown::draw(&text, &look, width, self.heat_of(&key), |url| {
                Target::Url(url.to_owned())
            });
            (drawing.lines, drawing.sources)
        } else {
            text.lines()
                .enumerate()
                .map(|(index, line)| {
                    let classes = highlighted.and_then(|lines| lines.get(index));
                    (Line::Text(code_spans(line, classes, None)), Some(index))
                })
                .unzip()
        };
        let mut lines = if gutter {
            drawn
                .into_iter()
                .zip(gutter_changes(&changes, &sources))
                .map(|(line, change)| match line {
                    Line::Text(mut spans) => {
                        spans.insert(0, gutter_mark(change));
                        Line::Text(spans)
                    }
                    picture @ Line::Image { .. } => picture,
                })
                .collect()
        } else {
            drawn
        };
        if truncated {
            lines.push(Line::text(""));
            lines.push(said(
                "Only its first 1 MB is shown: gX opens the rest.",
                hl::MESSAGE,
            ));
        }
        lines
    }
}

/// The list's rows: each file's status letter, its path, and its lines added and removed, at the right of `width`.
fn file_rows(
    thread: ThreadId,
    files: &[ChangedFile],
    width: u16,
) -> Vec<(BlockKey, Content<Target>)> {
    let counts: Vec<(String, String)> = files
        .iter()
        .map(|file| {
            (
                format!("+{}", file.additions),
                format!("\u{2212}{}", file.deletions),
            )
        })
        .collect();
    let counts_width = counts
        .iter()
        .map(|(added, removed)| added.width() + 1 + removed.width())
        .max()
        .unwrap_or(0);
    let path_width = usize::from(width).saturating_sub(2 + 1 + counts_width);
    files
        .iter()
        .zip(counts)
        .enumerate()
        .map(|(index, (file, (added, removed)))| {
            let letter = match file.status {
                FileStatus::Added => Group::ADDED,
                FileStatus::Removed => Group::REMOVED,
                FileStatus::Modified | FileStatus::Renamed | FileStatus::Copied => Group::CHANGED,
            };
            let path = match &file.previous_path {
                Some(previous) => format!("{previous} \u{2192} {}", file.path),
                None => file.path.clone(),
            };
            let pad = counts_width.saturating_sub(added.width() + 1 + removed.width());
            let line = vec![
                Span::new(format!("{} ", file.status.letter()), letter),
                Span::new(list::fit(&path, path_width), hl::PATH),
                Span::new(format!(" {}", " ".repeat(pad)), Group::NORMAL),
                Span::new(added, hl::AGE),
                Span::new(" ", Group::NORMAL),
                Span::new(removed, hl::AGE),
            ];
            let content = Content {
                lines: vec![list::clip(line, usize::from(width))],
                folds: Vec::new(),
                target: Some(Target::File(thread, index)),
            };
            (BlockKey::new(format!("{index} {}", file.path)), content)
        })
        .collect()
}

/// A line saying `text` in `group`.
fn said(text: &str, group: Group) -> Line<Target> {
    Line::Text(vec![Span::new(text, group)])
}

/// The lines of `patch`, a file's diff, highlighted as far as `highlighted` goes. Lines added and removed are on the
/// colorscheme's `DiffAdd` and `DiffDelete` background, across the window if they fit in it, their `+` and `-` in
/// `Added` and `Removed`; hunks' `@@` lines are muted.
fn diff_lines(kit: &Kit, patch: &str, highlighted: Option<&[Classes]>) -> Vec<Line<Target>> {
    let theme = kit.theme();
    let width = usize::from(kit.size().0);
    patch
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let (side, background) = match line.as_bytes().first() {
                Some(b'+') => (
                    Group::ADDED,
                    Some(list::background(theme.style(Group::DIFF_ADD))),
                ),
                Some(b'-') => (
                    Group::REMOVED,
                    Some(list::background(theme.style(Group::DIFF_DELETE))),
                ),
                // `@@ -1,3 +1,4 @@` and "\ No newline at end of file".
                Some(b'@' | b'\\') => return said(&untabbed(line), hl::HUNK),
                _ => (Group::NORMAL, None),
            };
            // The `+`, `-` or space that says which side it's on, and its code.
            let (start, code) = if line.is_char_boundary(1) {
                line.split_at(1)
            } else {
                ("", line)
            };
            let classes = highlighted.and_then(|lines| lines.get(index));
            let mut side = Span::new(start, side);
            if let Some(background) = background {
                side = side.with_style(background);
            }
            let mut spans = vec![side];
            spans.extend(code_spans(code, classes, background));
            if let Some(background) = background {
                // A line's background is only under its text: spaces take it across the window. Not for a line that
                // wraps.
                let used: usize = spans.iter().map(|span| span.text.width()).sum();
                if used < width {
                    spans.push(
                        Span::new(" ".repeat(width - used), Group::NORMAL).with_style(background),
                    );
                }
            }
            Line::Text(spans)
        })
        .collect()
}

/// `line` as spans, in its classes' groups if `classes` gives them, its tabs as spaces, on `background` if any.
fn code_spans(
    line: &str,
    classes: Option<&Classes>,
    background: Option<ratatui::style::Style>,
) -> Vec<Span<Target>> {
    let pieces = match classes {
        Some(classes) => syntax::pieces(line, classes),
        None => vec![(line, None)],
    };
    pieces
        .into_iter()
        .map(|(piece, class)| {
            let span = Span::new(
                untabbed(piece),
                class.map_or(Group::NORMAL, crate::syntax::Class::group),
            );
            match background {
                Some(background) => span.with_style(background),
                None => span,
            }
        })
        .collect()
}

/// `line` with its tabs as four spaces, which a terminal cell can't hold.
fn untabbed(line: &str) -> String {
    line.replace('\t', "    ")
}

/// What the gutter shows next to each line drawn, the line of the file each draws given by `sources`: what the pull
/// request did to that line, its `changes`. Lines removed next to a line not drawn, like a blank one between
/// paragraphs, show next to the last line drawn before it, or else the first.
fn gutter_changes(
    changes: &[Option<LineChange>],
    sources: &[Option<usize>],
) -> Vec<Option<LineChange>> {
    let mut shown: Vec<Option<LineChange>> = sources
        .iter()
        .map(|source| source.and_then(|line| changes.get(line).copied().flatten()))
        .collect();
    let drawn: HashSet<usize> = sources.iter().flatten().copied().collect();
    for (line, change) in changes.iter().enumerate() {
        if !matches!(
            change,
            Some(LineChange::RemovedBelow | LineChange::RemovedAbove)
        ) || drawn.contains(&line)
        {
            continue;
        }
        let before = sources
            .iter()
            .rposition(|source| source.is_some_and(|source| source < line));
        let (at, change) = match before {
            Some(at) => (at, LineChange::RemovedBelow),
            None => match sources.iter().position(Option::is_some) {
                Some(at) => (at, LineChange::RemovedAbove),
                None => continue,
            },
        };
        shown[at].get_or_insert(change);
    }
    shown
}

/// What the gutter shows of a line the pull request did `change` to, and the space after it.
fn gutter_mark(change: Option<LineChange>) -> Span<Target> {
    match change {
        Some(LineChange::Added) => Span::new("\u{258e} ", Group::ADDED),
        Some(LineChange::Modified) => Span::new("\u{258e} ", Group::CHANGED),
        Some(LineChange::RemovedBelow) => Span::new("\u{2581} ", Group::REMOVED),
        Some(LineChange::RemovedAbove) => Span::new("\u{2594} ", Group::REMOVED),
        None => Span::new("  ", Group::NORMAL),
    }
}

impl Ghd {
    /// What the Files screen shows that voight would assess: the file at the cursor, if it's Markdown shown whole.
    pub(super) fn file_assessable(&self, kit: &mut Kit) -> Option<(assessment::Key, String)> {
        let screen = self.files.screen.as_ref()?;
        let file = self.files.files()?.get(self.selected_file(kit)?)?;
        if !file.is_markdown() || self.files.showing(file) != Showing::Whole {
            return None;
        }
        let Some(Fetched::Done((text, _))) = self.files.text(screen.notification.thread, file)
        else {
            return None;
        };
        Some((
            assessment::Key {
                thread: screen.notification.thread,
                as_of: screen.notification.updated_at,
                part: assessment::Part::File(file.contents_path.clone()),
            },
            markdown::as_github_shows(&text),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_lines_show_next_to_the_lines_drawn() {
        let changes = [
            None,
            Some(LineChange::RemovedBelow),
            None,
            Some(LineChange::Added),
        ];
        // The line removed below is a blank line between paragraphs, not drawn.
        let sources = [Some(0), None, Some(2), Some(3)];

        assert_eq!(
            gutter_changes(&changes, &sources),
            [
                Some(LineChange::RemovedBelow),
                None,
                None,
                Some(LineChange::Added)
            ]
        );
    }

    #[test]
    fn lines_removed_above_the_first_show_on_the_first_drawn() {
        let changes = [Some(LineChange::RemovedAbove), None];
        let sources = [None, Some(1)];

        assert_eq!(
            gutter_changes(&changes, &sources),
            [None, Some(LineChange::RemovedAbove)]
        );
    }
}
