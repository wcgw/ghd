//! The Files screen: a pull request's Changed files in place of the list, the selected one below.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};

use super::pane_search::PaneSearch;
use super::{
    Assessable, Command, PREVIEW_MIN_HEIGHT, Scrolled, StatusMessage, Step, Wheel, counted,
    move_within,
};
use crate::assessment::{self, Window};
use crate::fetched::Fetched;
use crate::files::{ChangedFile, ChangedFiles, FileStatus, FileText, LineChange};
use crate::github::{FilesRequest, GitHub};
use crate::inbox::{Listing, Notification, ThreadId};
use crate::markdown;
use crate::syntax::{self, Classes, Highlighting};
use crate::theme::Theme;

/// The Files screen, and the Changed files fetched for it this run, which outlast it being closed and reopened.
#[derive(Debug, Default)]
pub(super) struct Files {
    /// The screen, while it's open.
    screen: Option<Screen>,
    fetched: ChangedFiles,
}

/// The Files screen while it's open: which pull request it is of, and where it's at.
#[derive(Debug)]
struct Screen {
    /// The pull request's Notification, as it was when the screen opened.
    notification: Notification,
    request: FilesRequest,
    /// The selected file's row.
    cursor: TableState,
    /// How many rows the list had room for at the last draw.
    rows: u16,
    /// How the files that can show either way show; a new one always shows whole, and a removed one as its diff.
    showing: Showing,
    /// The selected file's pane.
    scroll: Scrolled,
    /// What's looked for in the selected file, and in the others once they're selected.
    search: PaneSearch,
    /// Each file's highlighting so far, whole or as its diff, by its contents path; `None` for one in a language ghd
    /// doesn't know. Drawing highlights a file as far down as it shows, which only drawing knows, and otherwise only
    /// reads the screen.
    highlighting: RefCell<HashMap<(Showing, String), Option<Highlighting>>>,
}

impl Screen {
    /// The pull request's Changed files in `fetched`; `None` until they're fetched, or if they couldn't be.
    fn files<'a>(&self, fetched: &'a ChangedFiles) -> Option<&'a [ChangedFile]> {
        match fetched.list(self.notification.thread, self.notification.updated_at)? {
            Fetched::Done(list) => Some(list),
            Fetched::Fetching | Fetched::Failed(_) => None,
        }
    }
}

/// How a Changed file shows: whole, or as its diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Showing {
    Whole,
    Diff,
}

/// What a key press on the Files screen leaves for the app around it to do.
#[derive(Debug)]
pub(super) enum Pressed {
    /// The screen dealt with it.
    Nothing,
    Quit,
    /// Something to say in the status line.
    Said(StatusMessage),
    /// `r`: the Changed files are forgotten, and so are the thread's Assessments of them.
    Forgot(ThreadId),
    /// `o` opened the selected file in the browser, which marks the pull request's Notification read.
    Opened(ThreadId),
}

impl Files {
    /// Whether the screen is open.
    pub(super) fn is_open(&self) -> bool {
        self.screen.is_some()
    }

    /// Opens the screen for `notification`, on a screen `screen_height` lines tall; says why not if it isn't a pull
    /// request, or the terminal is too short.
    pub(super) fn open(
        &mut self,
        notification: &Notification,
        screen_height: u16,
    ) -> Option<StatusMessage> {
        let Some(request) = FilesRequest::for_notification(notification) else {
            return Some(StatusMessage::Info(
                "Only pull requests have files".to_owned(),
            ));
        };
        if screen_height < PREVIEW_MIN_HEIGHT {
            return Some(StatusMessage::Info(format!(
                "Files need a terminal at least {PREVIEW_MIN_HEIGHT} lines tall"
            )));
        }
        self.screen = Some(Screen {
            notification: notification.clone(),
            request,
            cursor: TableState::new().with_selected(Some(0)),
            rows: 0,
            showing: Showing::Diff,
            scroll: Scrolled::default(),
            search: PaneSearch::default(),
            highlighting: RefCell::default(),
        });
        None
    }

    /// Scrolls the selected file by a turn of the mouse wheel.
    pub(super) fn wheel(&mut self, turn: Wheel) {
        if let Some(screen) = &mut self.screen {
            screen.scroll.wheel(turn);
        }
    }

    /// What's looked for in the selected file, as typed so far, while the user types it; `None` while they don't.
    pub(super) fn typed(&self) -> Option<&str> {
        self.screen.as_ref()?.search.typed()
    }

    /// Edits what's looked for in the selected file with `key`, as [`PaneSearch::type_key`] does.
    pub(super) fn type_key(&mut self, key: KeyEvent) {
        if let Some(screen) = &mut self.screen {
            screen.search.type_key(key, &mut screen.scroll);
        }
    }

    /// Carries out `command` on the screen, where only moving, scrolling, searching the selected file, `w`, `o`, `r`,
    /// Esc or `f`, `?` and quitting do anything: the rest act on Notifications. Returns what's left for the app around
    /// it to do.
    pub(super) fn press(&mut self, command: Command) -> Pressed {
        let Some(screen) = &mut self.screen else {
            return Pressed::Nothing;
        };
        let files = screen.files(&self.fetched).map_or(0, <[ChangedFile]>::len);
        let file = screen.cursor.selected();
        let half_page = (screen.rows / 2).max(1);
        match command {
            Command::Quit => return Pressed::Quit,
            Command::Down => move_within(&mut screen.cursor, files, TableState::select_next),
            Command::Up => move_within(&mut screen.cursor, files, TableState::select_previous),
            Command::First => move_within(&mut screen.cursor, files, TableState::select_first),
            Command::Last => move_within(&mut screen.cursor, files, TableState::select_last),
            Command::HalfPageDown => {
                move_within(&mut screen.cursor, files, |cursor| {
                    cursor.scroll_down_by(half_page);
                });
            }
            Command::HalfPageUp => {
                move_within(&mut screen.cursor, files, |cursor| {
                    cursor.scroll_up_by(half_page);
                });
            }
            Command::ScrollPreviewDown => {
                screen.scroll.half_down();
                return Pressed::Nothing;
            }
            Command::ScrollPreviewUp => {
                screen.scroll.half_up();
                return Pressed::Nothing;
            }
            Command::Search => {
                screen.search.start_typing(&screen.scroll);
                return Pressed::Nothing;
            }
            Command::NextMatch => {
                screen.search.step(Step::Next);
                return Pressed::Nothing;
            }
            Command::PreviousMatch => {
                screen.search.step(Step::Previous);
                return Pressed::Nothing;
            }
            Command::ToggleWholeFiles => {
                screen.showing = match screen.showing {
                    Showing::Whole => Showing::Diff,
                    Showing::Diff => Showing::Whole,
                };
            }
            Command::Refresh => {
                let thread = screen.notification.thread;
                self.fetched.forget(thread);
                return Pressed::Forgot(thread);
            }
            Command::Open => return self.open_selected(),
            Command::Clear => {
                // Esc stops looking for anything in the file before it goes back.
                if !screen.search.clear() {
                    self.screen = None;
                }
                return Pressed::Nothing;
            }
            Command::OpenFiles => {
                self.screen = None;
                return Pressed::Nothing;
            }
            // What acts on Notifications; `?`, `H` and `:` never reach here, the app around it showing the help, the
            // Heatmap and the command line itself.
            Command::Help
            | Command::ToggleHeatmap
            | Command::OpenCommandLine
            | Command::ToggleSelected
            | Command::SelectAllShown
            | Command::NextView
            | Command::PreviousView
            | Command::ShowView(_)
            | Command::MarkRead
            | Command::MarkDone
            | Command::ToggleDone
            | Command::ToggleUnreadOnly
            | Command::TogglePreview => return Pressed::Nothing,
        }
        // Another file, or another way to show it: from its top, what's looked for looked for in it. Moving no further,
        // at either end, leaves the file as it is.
        if command == Command::ToggleWholeFiles || screen.cursor.selected() != file {
            screen.scroll.by = 0;
            screen.search.lose_place();
        }
        Pressed::Nothing
    }

    /// Opens the selected Changed file on GitHub, which marks the pull request's Notification read, as opening the
    /// pull request does.
    fn open_selected(&mut self) -> Pressed {
        let Some(url) = self.selected_url() else {
            return Pressed::Nothing;
        };
        if let Err(error) = open::that_detached(&url) {
            return Pressed::Said(StatusMessage::Error(format!(
                "Couldn't open a browser: {error}"
            )));
        }
        match &self.screen {
            Some(screen) => Pressed::Opened(screen.notification.thread),
            None => Pressed::Nothing,
        }
    }

    /// The Changed file selected; `None` while the screen is closed, or its files aren't fetched yet.
    fn selected(&self) -> Option<&ChangedFile> {
        let screen = self.screen.as_ref()?;
        screen.files(&self.fetched)?.get(screen.cursor.selected()?)
    }

    /// The lines the list takes to show every Changed file, its border included; `None` while it shows a message in
    /// their place: the screen closed, the files not fetched yet, or none.
    pub(super) fn lines_listing_all(&self) -> Option<u16> {
        let screen = self.screen.as_ref()?;
        let list = screen.files(&self.fetched)?;
        if list.is_empty() {
            return None;
        }
        // And a line above and below for the border.
        Some(
            u16::try_from(list.len())
                .unwrap_or(u16::MAX)
                .saturating_add(2),
        )
    }

    /// Whether `file` shows whole rather than as its diff: a new one, or any but a removed one once `w` asks for files
    /// whole.
    fn shows_whole(&self, file: &ChangedFile) -> bool {
        match file.status {
            FileStatus::Added => true,
            FileStatus::Removed => false,
            FileStatus::Modified | FileStatus::Renamed | FileStatus::Copied => self
                .screen
                .as_ref()
                .is_some_and(|screen| screen.showing == Showing::Whole),
        }
    }

    /// Where GitHub shows the selected Changed file: in the pull request's "Files changed" tab, at the file; or the
    /// tab itself while the files aren't fetched.
    pub(super) fn selected_url(&self) -> Option<String> {
        let page = &self.screen.as_ref()?.notification.subject_url;
        Some(match self.selected() {
            Some(file) => format!("{page}/files#{}", file.anchor()),
            None => format!("{page}/files"),
        })
    }

    /// The Changed files to fetch now, noted as being fetched: the screen's, unless they're fetched already.
    pub(super) fn list_to_fetch(&mut self) -> Option<Wanted> {
        let screen = self.screen.as_ref()?;
        let (thread, as_of) = (screen.notification.thread, screen.notification.updated_at);
        if self.fetched.list(thread, as_of).is_some() {
            return None;
        }
        let request = screen.request.clone();
        self.fetched.fetching_list(thread, as_of);
        Some(Wanted {
            thread,
            as_of,
            request,
        })
    }

    /// Takes in a pull request's Changed files, or why they couldn't be fetched.
    pub(super) fn took_list(&mut self, fetched: WasFetched) {
        self.fetched
            .fetched_list(fetched.thread, fetched.as_of, fetched.files);
    }

    /// The whole file to fetch now, noted as being fetched: the selected one, when it's shown whole and there's no
    /// patch to show it from.
    pub(super) fn text_to_fetch(&mut self) -> Option<WantedText> {
        let file = self.selected()?;
        if !self.shows_whole(file) || file.added_text().is_some() {
            return None;
        }
        let thread = self.screen.as_ref()?.notification.thread;
        let contents_path = file.contents_path.clone();
        if self.fetched.text(thread, &contents_path).is_some() {
            return None;
        }
        self.fetched.fetching_text(thread, &contents_path);
        Some(WantedText {
            thread,
            contents_path,
        })
    }

    /// Takes in a whole file, or why it couldn't be fetched.
    pub(super) fn took_text(&mut self, fetched: TextFetched) {
        self.fetched
            .fetched_text(fetched.thread, &fetched.contents_path, fetched.text);
    }

    /// Forgets the Changed files of threads neither of `listing`'s lists holds.
    pub(super) fn retain_in(&mut self, listing: &Listing) {
        self.fetched.retain_in(listing);
    }

    /// How many lines down the selected file's pane is scrolled; 0 while the screen is closed.
    #[cfg(test)]
    pub(super) fn scrolled_by(&self) -> u16 {
        self.screen.as_ref().map_or(0, |screen| screen.scroll.by)
    }

    /// What's on the screen for voight to assess: the selected file, if it's Markdown shown whole. `None` when
    /// there's nothing to assess, and while the screen is closed.
    pub(super) fn assessable(&self) -> Option<Assessable<'_>> {
        let screen = self.screen.as_ref()?;
        let file = self.selected()?;
        if !file.is_markdown() || !self.shows_whole(file) {
            return None;
        }
        let text = match file.added_text() {
            Some(text) => Cow::Owned(text),
            None => match self
                .fetched
                .text(screen.notification.thread, &file.contents_path)?
            {
                Fetched::Done(FileText::Text { text, .. }) => Cow::Borrowed(text.as_str()),
                _ => return None,
            },
        };
        Some(Assessable {
            key: assessment::Key {
                thread: screen.notification.thread,
                as_of: screen.notification.updated_at,
                part: assessment::Part::File(file.contents_path.clone()),
            },
            text,
            markdown: true,
        })
    }

    /// Draws the list in `area`: the pull request in its border, and its Changed files.
    pub(super) fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(screen) = &mut self.screen else {
            return;
        };
        // Inside the border.
        screen.rows = area.height.saturating_sub(2);
        let notification = &screen.notification;
        let fetched = self
            .fetched
            .list(notification.thread, notification.updated_at);
        let number = notification
            .subject_url
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let mut title = vec![
            Span::raw(" "),
            Span::styled(format!("#{number} "), theme.age),
            Span::styled(notification.subject_title.clone(), theme.title.bold()),
        ];
        if let Some(Fetched::Done(list)) = fetched {
            let added: u64 = list.iter().map(|file| file.additions).sum();
            let removed: u64 = list.iter().map(|file| file.deletions).sum();
            title.extend([
                Span::raw(format!(" · {} · ", counted(list.len() as u64, "file"))),
                Span::styled(format!("+{added}"), theme.diff_added),
                Span::raw(" "),
                Span::styled(format!("\u{2212}{removed}"), theme.diff_removed),
            ]);
        }
        title.push(Span::raw(" "));
        let block = Block::bordered()
            .border_style(theme.border)
            .title(Line::from(title))
            .title_bottom(
                Line::styled(
                    " Esc back · w whole files or diffs · / search · ? keys ",
                    theme.hint,
                )
                .right_aligned(),
            );
        let list = match fetched {
            None | Some(Fetched::Fetching) => {
                frame.render_widget(
                    Paragraph::new("Fetching files…")
                        .style(theme.message)
                        .block(block),
                    area,
                );
                return;
            }
            Some(Fetched::Failed(why)) => {
                frame.render_widget(
                    Paragraph::new(format!("Couldn't fetch the files: {why}"))
                        .style(theme.error)
                        .wrap(Wrap { trim: false })
                        .block(block),
                    area,
                );
                return;
            }
            Some(Fetched::Done(list)) if list.is_empty() => {
                frame.render_widget(
                    Paragraph::new("No files.")
                        .style(theme.message)
                        .block(block),
                    area,
                );
                return;
            }
            Some(Fetched::Done(list)) => list,
        };
        let counts: Vec<String> = list
            .iter()
            .map(|file| format!("+{} \u{2212}{}", file.additions, file.deletions))
            .collect();
        let counts_width = counts.iter().map(|c| c.chars().count()).max().unwrap_or(0);
        let rows = list.iter().zip(counts).map(|(file, counts)| {
            let letter_style = match file.status {
                FileStatus::Added => theme.diff_added,
                FileStatus::Removed => theme.diff_removed,
                FileStatus::Modified | FileStatus::Renamed | FileStatus::Copied => {
                    theme.diff_changed
                }
            };
            let path = match &file.previous_path {
                Some(previous) => format!("{previous} \u{2192} {}", file.path),
                None => file.path.clone(),
            };
            Row::new([
                Cell::from(file.status.letter().to_string()).style(letter_style),
                Cell::from(path).style(theme.title),
                Cell::from(Line::from(counts).right_aligned()).style(theme.age),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(u16::try_from(counts_width).unwrap_or(u16::MAX)),
            ],
        )
        .row_highlight_style(theme.cursor)
        .block(block);
        frame.render_stateful_widget(table, area, &mut screen.cursor);
    }

    /// Draws the selected Changed file in `area`, as far down as it's scrolled, or as a jump to where what's looked for
    /// is found takes it, as the Heatmap of `heat`, the windows of its Assessment, if there are any; with what's looked
    /// for in the bottom border, and what voight says of it if `label` says anything.
    pub(super) fn render_selected(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        label: Option<Span<'static>>,
        heat: &[Window],
    ) {
        // Inside the border.
        let width = area.width.saturating_sub(2);
        // As far down as it's scrolled, and the pane's height more: each line takes at least a line of the pane.
        let down_to = |scrolled_by: u16| usize::from(scrolled_by.saturating_add(area.height));
        let scrolled_by = self.screen.as_ref().map_or(0, |screen| screen.scroll.by);
        let mut lines = self.selected_lines(theme, width, heat, down_to(scrolled_by));
        let jumped_to = self.screen.as_mut().map(|screen| {
            screen.search.jump(&lines, area, &mut screen.scroll);
            screen.scroll.by
        });
        if let Some(jumped_to) = jumped_to.filter(|&by| by > scrolled_by) {
            // Further down than it's highlighted.
            lines = self.selected_lines(theme, width, heat, down_to(jumped_to));
        }
        let path = self
            .selected()
            .map(|file| format!(" {} ", file.path))
            .unwrap_or_default();
        let mut block = Block::bordered()
            .border_style(theme.border)
            .title(Line::styled(path, theme.tab));
        if let Some(search) = self
            .screen
            .as_ref()
            .and_then(|screen| screen.search.highlight(&mut lines, theme))
        {
            block = block.title_bottom(search);
        }
        if let Some(label) = label {
            block = block.title_bottom(Line::from(label).right_aligned());
        }
        let text = Paragraph::new(lines).wrap(Wrap { trim: false });
        if let Some(screen) = &mut self.screen {
            screen
                .scroll
                .render(frame, area, text, block, theme.scrollbar);
        }
    }

    /// What the file pane shows of the selected file, as [`Self::lines_of`] does; nothing if there's none.
    fn selected_lines(
        &self,
        theme: &Theme,
        width: u16,
        heat: &[Window],
        down_to: usize,
    ) -> Vec<Line<'static>> {
        self.selected().map_or_else(Vec::new, |file| {
            self.lines_of(file, theme, width, heat, down_to)
        })
    }

    /// What the file pane shows of `file`, `width` cells wide: where it was renamed or copied from, then the file
    /// whole, as the Heatmap of `heat` and highlighted down to line `down_to` at least, or its diff, or why there's
    /// neither.
    fn lines_of(
        &self,
        file: &ChangedFile,
        theme: &Theme,
        width: u16,
        heat: &[Window],
        down_to: usize,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        match (file.status, &file.previous_path) {
            (FileStatus::Renamed, Some(previous)) => lines.push(Line::styled(
                format!("Renamed from {previous}"),
                theme.markdown_muted,
            )),
            (FileStatus::Copied, Some(previous)) => lines.push(Line::styled(
                format!("Copied from {previous}"),
                theme.markdown_muted,
            )),
            _ => {}
        }
        let what = match &file.patch {
            _ if file.status == FileStatus::Removed => vec![Line::styled(
                format!("Deleted: {}", counted(file.deletions, "line")),
                theme.markdown_muted,
            )],
            _ if self.shows_whole(file) => self.whole_lines_of(file, theme, width, heat, down_to),
            Some(patch) => self.diff_lines(file, patch, theme, width, down_to),
            // Renamed or copied as it was: that's all there is to say.
            None if file.additions + file.deletions == 0 && !lines.is_empty() => Vec::new(),
            None => vec![Line::styled(
                "No diff from GitHub (binary, or too large): o opens it.",
                theme.message,
            )],
        };
        if !lines.is_empty() && !what.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(what);
        lines
    }

    /// `file`, whole, `width` cells wide: rendered if it's Markdown, as the Heatmap of `heat`, or else highlighted down
    /// to line `down_to` at least; with a gutter showing what the pull request did to its lines, when its patch says.
    /// Or why it can't be shown yet, or at all.
    fn whole_lines_of(
        &self,
        file: &ChangedFile,
        theme: &Theme,
        width: u16,
        heat: &[Window],
        down_to: usize,
    ) -> Vec<Line<'static>> {
        let (text, truncated) = if let Some(text) = file.added_text() {
            (Cow::Owned(text), false)
        } else {
            let thread = self
                .screen
                .as_ref()
                .map(|screen| screen.notification.thread);
            match thread.and_then(|thread| self.fetched.text(thread, &file.contents_path)) {
                None | Some(Fetched::Fetching) => {
                    return vec![Line::styled("Fetching…", theme.message)];
                }
                Some(Fetched::Failed(why)) => {
                    return vec![Line::styled(
                        format!("Couldn't fetch the file: {why}"),
                        theme.error,
                    )];
                }
                Some(Fetched::Done(FileText::Binary)) => {
                    return vec![Line::styled("Binary: o opens it.", theme.message)];
                }
                Some(Fetched::Done(FileText::Text { text, truncated })) => {
                    (Cow::Borrowed(text.as_str()), *truncated)
                }
            }
        };
        if text.trim().is_empty() {
            return vec![Line::styled("Empty file.", theme.message)];
        }
        let changes = file.line_changes();
        // No gutter without a patch to say what changed.
        let gutter = !changes.is_empty();
        let drawn = if file.is_markdown() {
            let width = if gutter {
                width.saturating_sub(GUTTER_WIDTH)
            } else {
                width
            };
            markdown::lines_with_sources(&text, theme, width, heat)
        } else {
            self.code_lines(file, &text, theme, down_to)
                .into_iter()
                .enumerate()
                .map(|(index, line)| (line, Some(index)))
                .collect()
        };
        let mut lines = if gutter {
            with_gutter(drawn, &changes, theme)
        } else {
            drawn.into_iter().map(|(line, _)| line).collect()
        };
        if truncated {
            lines.push(Line::default());
            lines.push(Line::styled(
                "Only its first 1 MB is shown: o opens the rest.",
                theme.message,
            ));
        }
        lines
    }

    /// The lines of `text`, `file` whole, highlighted down to line `down_to` at least if ghd knows its language.
    fn code_lines(
        &self,
        file: &ChangedFile,
        text: &str,
        theme: &Theme,
        down_to: usize,
    ) -> Vec<Line<'static>> {
        let highlighted = self.highlighted(file, Showing::Whole, down_to, || {
            Highlighting::of_file(&file.path, text)
        });
        text.lines()
            .enumerate()
            .map(|(index, line)| {
                let classes = highlighted.as_deref().and_then(|lines| lines.get(index));
                Line::from(code_spans(line, classes, theme))
            })
            .collect()
    }

    /// The lines of `patch`, `file`'s diff, highlighted down to line `down_to` at least if ghd knows its language.
    /// Lines added and removed are on their own background, across the pane's `width` if they fit in it, their `+`
    /// and `-` in their own color; hunks' `@@` lines are muted.
    fn diff_lines(
        &self,
        file: &ChangedFile,
        patch: &str,
        theme: &Theme,
        width: u16,
        down_to: usize,
    ) -> Vec<Line<'static>> {
        let highlighted = self.highlighted(file, Showing::Diff, down_to, || {
            Highlighting::of_diff(&file.path, patch)
        });
        patch
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let (side, background) = match line.as_bytes().first() {
                    Some(b'+') => (theme.diff_added, Some(theme.diff_added_line)),
                    Some(b'-') => (theme.diff_removed, Some(theme.diff_removed_line)),
                    // `@@ -1,3 +1,4 @@` and "\ No newline at end of file".
                    Some(b'@' | b'\\') => {
                        return Line::styled(untabbed(line), theme.markdown_muted);
                    }
                    _ => (Style::new(), None),
                };
                // The `+`, `-` or space that says which side it's on, and its code.
                let (start, code) = if line.is_char_boundary(1) {
                    line.split_at(1)
                } else {
                    ("", line)
                };
                let classes = highlighted.as_deref().and_then(|lines| lines.get(index));
                let mut spans = vec![Span::styled(start.to_owned(), side)];
                spans.extend(code_spans(code, classes, theme));
                let mut line = Line::from(spans);
                if let Some(background) = background {
                    // A line's style is only under its text: spaces take it across the pane. Not for a line that
                    // wraps, as where it wraps isn't known here.
                    let room = usize::from(width).saturating_sub(line.width());
                    if room > 0 {
                        line.push_span(" ".repeat(room));
                    }
                    line = line.style(background);
                }
                line
            })
            .collect()
    }

    /// The classes of the pieces of the lines of `file` as it's `showing`, highlighted down to line `down_to` at
    /// least, by the highlighting `make` makes the first time; `None` if ghd doesn't know its language.
    fn highlighted(
        &self,
        file: &ChangedFile,
        showing: Showing,
        down_to: usize,
        make: impl FnOnce() -> Option<Highlighting>,
    ) -> Option<Ref<'_, [Classes]>> {
        let screen = self.screen.as_ref()?;
        let key = (showing, file.contents_path.clone());
        if let Some(highlighting) = screen
            .highlighting
            .borrow_mut()
            .entry(key.clone())
            .or_insert_with(make)
        {
            highlighting.down_to(down_to);
        }
        Ref::filter_map(screen.highlighting.borrow(), |kept| {
            Some(kept.get(&key)?.as_ref()?.lines())
        })
        .ok()
    }

    /// How many lines of the selected file, as it shows, are highlighted so far; 0 while the screen is closed.
    #[cfg(test)]
    pub(super) fn lines_highlighted(&self) -> usize {
        let (Some(screen), Some(file)) = (&self.screen, self.selected()) else {
            return 0;
        };
        let showing = if self.shows_whole(file) {
            Showing::Whole
        } else {
            Showing::Diff
        };
        screen
            .highlighting
            .borrow()
            .get(&(showing, file.contents_path.clone()))
            .and_then(Option::as_ref)
            .map_or(0, |highlighting| highlighting.lines().len())
    }
}

/// A pull request's Changed files to fetch, for the activity its Notification has.
pub(super) struct Wanted {
    pub(super) thread: ThreadId,
    pub(super) as_of: DateTime<Utc>,
    request: FilesRequest,
}

/// The outcome of fetching a pull request's Changed files.
pub(super) struct WasFetched {
    pub(super) thread: ThreadId,
    pub(super) as_of: DateTime<Utc>,
    pub(super) files: anyhow::Result<Vec<ChangedFile>>,
}

/// Fetches the Changed files `wanted` from `github`.
///
/// # Cancel Safety
///
/// Cancel safe, as [`GitHub::changed_files`] is.
pub(super) async fn fetch(github: &GitHub, wanted: Wanted) -> WasFetched {
    WasFetched {
        files: github.changed_files(&wanted.request).await,
        thread: wanted.thread,
        as_of: wanted.as_of,
    }
}

/// A Changed file's whole text to fetch.
pub(super) struct WantedText {
    pub(super) thread: ThreadId,
    pub(super) contents_path: String,
}

/// The outcome of fetching a Changed file's whole text.
pub(super) struct TextFetched {
    pub(super) thread: ThreadId,
    pub(super) contents_path: String,
    pub(super) text: anyhow::Result<FileText>,
}

/// Fetches the whole text `wanted` from `github`.
///
/// # Cancel Safety
///
/// Cancel safe, as [`GitHub::file_text`] is.
pub(super) async fn fetch_text(github: &GitHub, wanted: WantedText) -> TextFetched {
    TextFetched {
        text: github.file_text(&wanted.contents_path).await,
        thread: wanted.thread,
        contents_path: wanted.contents_path,
    }
}

/// `line` with its tabs as four spaces, which a terminal cell can't hold.
fn untabbed(line: &str) -> String {
    line.replace('\t', "    ")
}

/// How many cells wide the gutter of a file shown whole is: its sign, and a space.
const GUTTER_WIDTH: u16 = 2;

/// The lines `drawn`, each with a gutter in front: see [`gutter_changes`]. Each comes with the index of the line of the
/// file it draws, if it draws one, and `changes` are those of the file's lines.
fn with_gutter(
    drawn: Vec<(Line<'static>, Option<usize>)>,
    changes: &[Option<LineChange>],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let sources: Vec<Option<usize>> = drawn.iter().map(|(_, source)| *source).collect();
    drawn
        .into_iter()
        .zip(gutter_changes(changes, &sources))
        .map(|((mut line, _), change)| {
            line.spans.insert(0, gutter(change, theme));
            line
        })
        .collect()
}

/// What the gutter shows next to each line drawn, the line of the file each draws given by `sources`: what the pull
/// request did to that line, its `changes`. Lines removed next to a line not drawn, like a blank one between
/// paragraphs, show next to the last line drawn before it, or else the first.
pub(super) fn gutter_changes(
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
fn gutter(change: Option<LineChange>, theme: &Theme) -> Span<'static> {
    match change {
        Some(LineChange::Added) => Span::styled("\u{258e} ", theme.diff_added),
        Some(LineChange::Modified) => Span::styled("\u{258e} ", theme.diff_changed),
        Some(LineChange::RemovedBelow) => Span::styled("\u{2581} ", theme.diff_removed),
        Some(LineChange::RemovedAbove) => Span::styled("\u{2594} ", theme.diff_removed),
        None => Span::raw("  "),
    }
}

/// `line` as spans, highlighted by `classes` if there are any, its tabs as spaces.
fn code_spans(line: &str, classes: Option<&Classes>, theme: &Theme) -> Vec<Span<'static>> {
    let spans = match classes {
        Some(classes) => syntax::spans(line, classes, Style::new(), theme),
        None => vec![Span::raw(line.to_owned())],
    };
    spans
        .into_iter()
        .map(|span| Span::styled(untabbed(&span.content), span.style))
        .collect()
}
