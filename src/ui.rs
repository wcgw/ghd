//! The terminal UI: for now, the Inbox View alone.

use std::pin::pin;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use chrono::{DateTime, TimeDelta, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize as _};
use ratatui::text::Line;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::github::GitHub;
use crate::inbox::{Inbox, Notification, SubjectKind};

/// Widest the repository column gets, in terminal cells; longer names are cut off.
const MAX_REPOSITORY_WIDTH: u16 = 32;

/// Longest that quitting waits for changes still on their way to GitHub.
const QUIT_GRACE: Duration = Duration::from_secs(10);

/// Runs the UI on `terminal` until the user quits, fetching the Inbox from `github` meanwhile and sending it the
/// changes the user makes.
///
/// # Errors
///
/// Fails if the terminal can't be drawn on or read from, or if changes made just before quitting were refused or
/// didn't reach GitHub in time. Failing to fetch the Inbox, or a refused change while the UI runs, isn't an error
/// here: the screen shows it.
pub async fn run(terminal: &mut DefaultTerminal, github: &GitHub) -> anyhow::Result<()> {
    let mut app = App::default();
    let mut events = EventStream::new();
    // Polled alongside terminal events, so the UI keeps drawing and can be quit while pages arrive.
    let mut fetch = pin!(github.inbox());
    // Changes already on screen, on their way to GitHub.
    let mut in_flight = FuturesUnordered::new();

    loop {
        terminal
            .draw(|frame| app.render(frame))
            .context("drawing the terminal")?;

        tokio::select! {
            inbox = &mut fetch, if matches!(app.inbox_view, InboxView::Loading) => {
                app.inbox_view = InboxView::fetched(inbox);
            }
            Some(reply) = in_flight.next(), if !in_flight.is_empty() => app.settle(reply),
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => match command_for(key) {
                    Some(Command::Quit) => break,
                    Some(command) => {
                        if let Some(change) = app.execute(command) {
                            in_flight.push(send(github, change));
                        }
                    }
                    None => {}
                },
                // Anything else, a resize included, just needs the redraw at the top of the loop.
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("reading terminal events"),
                None => break,
            },
        }
    }

    // Dropping the changes still in flight could lose ones the screen already showed as made.
    if in_flight.is_empty() {
        return Ok(());
    }
    let unconfirmed = in_flight.len();
    app.status = Some(StatusMessage::Info(format!(
        "Sending {unconfirmed} change(s) to GitHub…"
    )));
    terminal
        .draw(|frame| app.render(frame))
        .context("drawing the terminal")?;
    let replies = tokio::time::timeout(QUIT_GRACE, in_flight.collect::<Vec<_>>())
        .await
        .map_err(|_| {
            anyhow!("GitHub didn't confirm {unconfirmed} change(s) made just before quitting; they may not have been made")
        })?;
    let refused: Vec<String> = replies
        .into_iter()
        .filter_map(|reply| {
            let error = reply.accepted.err()?;
            Some(format!("couldn't {}: {error:#}", reply.change))
        })
        .collect();
    if !refused.is_empty() {
        bail!(
            "GitHub refused changes made just before quitting:\n{}",
            refused.join("\n")
        );
    }
    Ok(())
}

/// What a key press can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Quit,
    SelectNext,
    SelectPrevious,
    /// Opens the selected Subject in the browser, which also marks it read.
    Open,
    MarkRead,
    MarkDone,
}

/// The command a key press stands for. Marking read and Done use GitHub's own notification shortcuts.
fn command_for(key: KeyEvent) -> Option<Command> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Command::Quit),
        KeyCode::Char('q') => Some(Command::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(Command::SelectNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Command::SelectPrevious),
        KeyCode::Char('o') | KeyCode::Enter => Some(Command::Open),
        KeyCode::Char('I') => Some(Command::MarkRead),
        KeyCode::Char('e') => Some(Command::MarkDone),
        _ => None,
    }
}

/// A change to a Notification that is already on screen and on its way to GitHub. It holds the Notification as it
/// was before the change, so the change can be undone if GitHub refuses it.
#[derive(Debug)]
enum Change {
    MarkRead(Notification),
    MarkDone(Notification),
}

/// What the change does, e.g. `mark Done "Fix the build"`.
impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MarkRead(n) => write!(f, "mark read \"{}\"", n.subject_title),
            Self::MarkDone(n) => write!(f, "mark Done \"{}\"", n.subject_title),
        }
    }
}

/// GitHub's answer to a [`Change`].
struct Reply {
    change: Change,
    accepted: anyhow::Result<()>,
}

/// Sends `change` to GitHub.
///
/// # Cancel Safety
///
/// Not cancel safe: dropped while waiting for GitHub, the change may or may not have been made.
async fn send(github: &GitHub, change: Change) -> Reply {
    let accepted = match &change {
        Change::MarkRead(n) => github.mark_read(n.thread).await,
        Change::MarkDone(n) => github.mark_done(n.thread).await,
    };
    Reply { change, accepted }
}

/// A one-line message at the bottom of the screen.
#[derive(Debug)]
enum StatusMessage {
    Info(String),
    Error(String),
}

/// Everything on screen.
#[derive(Default)]
struct App {
    inbox_view: InboxView,
    /// What the latest action did, or why it failed; `None` until there's something to say.
    status: Option<StatusMessage>,
}

impl App {
    /// Carries out `command` on screen. Returns the change to send to GitHub, if the command makes one.
    fn execute(&mut self, command: Command) -> Option<Change> {
        match command {
            // `run` quits before asking.
            Command::Quit => None,
            Command::SelectNext => {
                self.inbox_view.select_next();
                None
            }
            Command::SelectPrevious => {
                self.inbox_view.select_previous();
                None
            }
            Command::Open => match self
                .inbox_view
                .selected()
                .map(|n| open::that_detached(&n.subject_url))?
            {
                Ok(()) => self.inbox_view.mark_selected_read().map(Change::MarkRead),
                Err(error) => {
                    self.status = Some(StatusMessage::Error(format!(
                        "Couldn't open a browser: {error}"
                    )));
                    None
                }
            },
            Command::MarkRead => self.inbox_view.mark_selected_read().map(Change::MarkRead),
            Command::MarkDone => {
                let notification = self.inbox_view.take_selected()?;
                self.status = Some(StatusMessage::Info(format!(
                    "Marked Done: {}",
                    notification.subject_title
                )));
                Some(Change::MarkDone(notification))
            }
        }
    }

    /// Takes in GitHub's answer to an earlier change: a refused change is undone on screen and reported.
    fn settle(&mut self, reply: Reply) {
        let Err(error) = reply.accepted else {
            return;
        };
        self.status = Some(StatusMessage::Error(format!(
            "GitHub refused to {}, undone: {error:#}",
            reply.change
        )));
        self.inbox_view.undo(reply.change);
    }

    fn render(&mut self, frame: &mut Frame) {
        let [view_area, status_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        self.inbox_view.render(frame, view_area);
        let status = match &self.status {
            None => return,
            Some(StatusMessage::Info(text)) => Line::from(text.as_str()),
            Some(StatusMessage::Error(text)) => Line::from(text.as_str()).red(),
        };
        frame.render_widget(status, status_area);
    }
}

/// The built-in View showing the whole Inbox, and what it has to show so far.
#[derive(Default)]
enum InboxView {
    #[default]
    Loading,
    Loaded {
        inbox: Inbox,
        selection: TableState,
    },
    Failed(anyhow::Error),
}

impl InboxView {
    /// The View once fetching the Inbox has finished, the first Notification selected.
    fn fetched(inbox: anyhow::Result<Inbox>) -> Self {
        match inbox {
            Ok(inbox) => {
                let first = (!inbox.is_empty()).then_some(0);
                Self::Loaded {
                    inbox,
                    selection: TableState::new().with_selected(first),
                }
            }
            Err(error) => Self::Failed(error),
        }
    }

    /// Moves the selection down one row; past the last row, the next draw puts it back.
    fn select_next(&mut self) {
        if let Self::Loaded { selection, .. } = self {
            selection.select_next();
        }
    }

    /// Moves the selection up one row; past the first row, the next draw puts it back.
    fn select_previous(&mut self) {
        if let Self::Loaded { selection, .. } = self {
            selection.select_previous();
        }
    }

    /// The selected Notification; `None` while there's no Inbox to select from, or it's empty.
    fn selected(&self) -> Option<&Notification> {
        let Self::Loaded { inbox, selection } = self else {
            return None;
        };
        inbox.get(selection.selected()?)
    }

    /// Clears Unread on the selected Notification. Returns it as it was before; `None` if it was already read.
    fn mark_selected_read(&mut self) -> Option<Notification> {
        let Self::Loaded { inbox, selection } = self else {
            return None;
        };
        // Cloned to keep it as it was, should the change need undoing.
        let notification = inbox.get(selection.selected()?)?.clone();
        inbox.mark_read(notification.thread).then_some(notification)
    }

    /// Takes the selected Notification out of the Inbox and returns it; the selection moves to the one below.
    fn take_selected(&mut self) -> Option<Notification> {
        let Self::Loaded { inbox, selection } = self else {
            return None;
        };
        let thread = inbox.get(selection.selected()?)?.thread;
        inbox.remove(thread)
    }

    /// Undoes `change` on screen, after GitHub refused it.
    fn undo(&mut self, change: Change) {
        let Self::Loaded { inbox, .. } = self else {
            return;
        };
        match change {
            Change::MarkRead(n) => inbox.mark_unread(n.thread),
            Change::MarkDone(n) => inbox.insert(n),
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let message = match self {
            Self::Loading => Paragraph::new("Fetching notifications…"),
            Self::Failed(error) => {
                Paragraph::new(format!("Couldn't fetch notifications: {error:#}"))
                    .red()
                    .wrap(Wrap { trim: false })
            }
            Self::Loaded { inbox, .. } if inbox.is_empty() => {
                Paragraph::new("Nothing in your Inbox.")
            }
            Self::Loaded { inbox, selection } => {
                let block = Block::bordered().title(format!(" Inbox ({}) ", inbox.unread_count()));
                let table = inbox_table(inbox, Utc::now()).block(block);
                frame.render_stateful_widget(table, area, selection);
                return;
            }
        };
        frame.render_widget(message.block(Block::bordered().title(" Inbox ")), area);
    }
}

/// One row per Notification: unread marker, Subject kind, repository, title, Reason, and age relative to `now`.
fn inbox_table(inbox: &Inbox, now: DateTime<Utc>) -> Table<'_> {
    let rows = inbox.iter().map(|n| {
        let style = if n.unread {
            Style::new().bold()
        } else {
            Style::new().dim()
        };
        Row::new([
            Cell::from(if n.unread { "●" } else { " " }),
            Cell::from(subject_kind_label(&n.subject_kind)),
            Cell::from(n.repository.as_str()),
            Cell::from(n.subject_title.as_str()),
            Cell::from(n.reason.to_string()),
            Cell::from(age(n.updated_at, now)),
        ])
        .style(style)
    });

    let repository_width = inbox
        .iter()
        .map(|n| n.repository.chars().count())
        .max()
        .unwrap_or(0);
    let reason_width = inbox
        .iter()
        .map(|n| n.reason.to_string().len())
        .max()
        .unwrap_or(0);
    let widths = [
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(
            u16::try_from(repository_width)
                .map_or(MAX_REPOSITORY_WIDTH, |w| w.min(MAX_REPOSITORY_WIDTH)),
        ),
        Constraint::Fill(1),
        Constraint::Length(u16::try_from(reason_width).unwrap_or(u16::MAX)),
        Constraint::Length(3),
    ];

    Table::new(rows, widths).row_highlight_style(Style::new().reversed())
}

/// A label of at most three cells for the Subject column.
fn subject_kind_label(kind: &SubjectKind) -> &'static str {
    match kind {
        SubjectKind::PullRequest => "PR",
        SubjectKind::Issue => "ISS",
        SubjectKind::Release => "REL",
        SubjectKind::Discussion => "DIS",
        SubjectKind::Commit => "CMT",
        SubjectKind::CheckSuite => "CI",
        SubjectKind::Other(_) => "?",
    }
}

/// How long before `now` the moment `then` was, in at most three characters: `now`, `5m`, `3h`, `2d`, `6w`, `2y`.
/// Each unit is truncated, not rounded. A `then` after `now` (clock skew) counts as `now`.
fn age(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(then);
    if elapsed < TimeDelta::minutes(1) {
        "now".to_owned()
    } else if elapsed < TimeDelta::hours(1) {
        format!("{}m", elapsed.num_minutes())
    } else if elapsed < TimeDelta::days(1) {
        format!("{}h", elapsed.num_hours())
    } else if elapsed < TimeDelta::weeks(1) {
        format!("{}d", elapsed.num_days())
    } else if elapsed < TimeDelta::days(365) {
        format!("{}w", elapsed.num_weeks())
    } else {
        format!("{}y", elapsed.num_days() / 365)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::inbox::{Reason, ThreadId};

    #[test]
    fn age_truncates_to_the_largest_whole_unit() {
        let now: DateTime<Utc> = "2026-10-21T12:00:00Z".parse().expect("valid timestamp");
        let cases = [
            (TimeDelta::seconds(-30), "now"),
            (TimeDelta::zero(), "now"),
            (TimeDelta::seconds(59), "now"),
            (TimeDelta::minutes(1), "1m"),
            (TimeDelta::seconds(3599), "59m"),
            (TimeDelta::hours(1), "1h"),
            (TimeDelta::minutes(24 * 60 - 1), "23h"),
            (TimeDelta::days(1), "1d"),
            (TimeDelta::hours(7 * 24 - 1), "6d"),
            (TimeDelta::weeks(1), "1w"),
            (TimeDelta::days(364), "52w"),
            (TimeDelta::days(365), "1y"),
            (TimeDelta::days(3 * 365 + 10), "3y"),
        ];
        for (elapsed, expected) in cases {
            assert_eq!(age(now - elapsed, now), expected, "{elapsed} ago");
        }
    }

    /// Width of the screen [`screen`] draws on.
    const SCREEN_WIDTH: u16 = 72;

    /// The app showing a loaded Inbox: "Add polling" (thread 1, unread, PR, just now), then "Old news" (thread 2,
    /// read, issue, two days ago). The first is selected.
    fn app() -> App {
        let inbox = [
            Notification {
                thread: ThreadId(1),
                subject_url: "https://github.com/wcgw/ghd/pull/1".to_owned(),
                repository: "wcgw/ghd".to_owned(),
                subject_kind: SubjectKind::PullRequest,
                subject_title: "Add polling".to_owned(),
                reason: Reason::ReviewRequested,
                unread: true,
                updated_at: Utc::now(),
            },
            Notification {
                thread: ThreadId(2),
                subject_url: "https://github.com/wcgw/other/issues/2".to_owned(),
                repository: "wcgw/other".to_owned(),
                subject_kind: SubjectKind::Issue,
                subject_title: "Old news".to_owned(),
                reason: Reason::Subscribed,
                unread: false,
                updated_at: Utc::now() - TimeDelta::days(2),
            },
        ]
        .into_iter()
        .collect();
        App {
            inbox_view: InboxView::fetched(Ok(inbox)),
            status: None,
        }
    }

    /// The lines `app` draws on a 5-line screen.
    fn screen(app: &mut App) -> Vec<String> {
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, 5)).expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame))
            .expect("test backend never fails");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(SCREEN_WIDTH.into())
            .map(|line| line.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    fn selected_title(app: &App) -> Option<&str> {
        app.inbox_view.selected().map(|n| n.subject_title.as_str())
    }

    fn refused() -> anyhow::Result<()> {
        Err(anyhow!("Not Found"))
    }

    #[test]
    fn loaded_inbox_shows_unread_count_and_rows() {
        let lines = screen(&mut app());

        assert!(lines[0].contains("Inbox (1)"), "{lines:#?}");
        assert!(
            lines[1].contains("● PR  wcgw/ghd   Add polling"),
            "{lines:#?}"
        );
        assert!(lines[1].contains("review-requested now"), "{lines:#?}");
        assert!(lines[2].contains("ISS wcgw/other Old news"), "{lines:#?}");
        assert!(lines[2].contains("subscribed       2d"), "{lines:#?}");
        assert_eq!(lines[4].trim(), "", "no status yet");
    }

    #[test]
    fn keys_follow_github_shortcuts() {
        let press = |code, modifiers| command_for(KeyEvent::new(code, modifiers));
        let none = KeyModifiers::NONE;

        assert_eq!(press(KeyCode::Char('e'), none), Some(Command::MarkDone));
        assert_eq!(
            press(KeyCode::Char('I'), KeyModifiers::SHIFT),
            Some(Command::MarkRead)
        );
        assert_eq!(press(KeyCode::Char('i'), none), None);
        assert_eq!(press(KeyCode::Char('o'), none), Some(Command::Open));
        assert_eq!(press(KeyCode::Enter, none), Some(Command::Open));
        assert_eq!(press(KeyCode::Char('j'), none), Some(Command::SelectNext));
        assert_eq!(press(KeyCode::Up, none), Some(Command::SelectPrevious));
        assert_eq!(press(KeyCode::Char('q'), none), Some(Command::Quit));
        assert_eq!(
            press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Command::Quit)
        );
        assert_eq!(press(KeyCode::Char('c'), none), None);
    }

    #[test]
    fn done_leaves_at_once_and_comes_back_if_refused() {
        let mut app = app();

        let change = app
            .execute(Command::MarkDone)
            .expect("a Notification is selected");

        assert!(matches!(&change, Change::MarkDone(n) if n.thread == ThreadId(1)));
        assert_eq!(
            selected_title(&app),
            Some("Old news"),
            "selection moves to the next one"
        );
        let lines = screen(&mut app);
        assert!(
            !lines[..4].iter().any(|line| line.contains("Add polling")),
            "{lines:#?}"
        );
        assert!(
            lines[4].starts_with("Marked Done: Add polling"),
            "{lines:#?}"
        );

        app.settle(Reply {
            change,
            accepted: refused(),
        });

        let lines = screen(&mut app);
        assert!(lines[1].contains("Add polling"), "{lines:#?}");
        assert!(
            lines[4].starts_with(r#"GitHub refused to mark Done "Add polling", undone: Not Found"#),
            "{lines:#?}"
        );
    }

    #[test]
    fn accepted_done_stays_done() {
        let mut app = app();
        let change = app
            .execute(Command::MarkDone)
            .expect("a Notification is selected");

        app.settle(Reply {
            change,
            accepted: Ok(()),
        });

        let lines = screen(&mut app);
        assert!(
            !lines[..4].iter().any(|line| line.contains("Add polling")),
            "{lines:#?}"
        );
        assert!(
            lines[4].starts_with("Marked Done: Add polling"),
            "{lines:#?}"
        );
    }

    #[test]
    fn read_is_sent_only_for_unread_and_undone_if_refused() {
        let mut app = app();

        let change = app
            .execute(Command::MarkRead)
            .expect("the selection is unread");
        assert!(matches!(&change, Change::MarkRead(n) if n.thread == ThreadId(1)));
        assert!(screen(&mut app)[0].contains("Inbox (0)"));
        assert!(app.execute(Command::MarkRead).is_none(), "already read");

        app.settle(Reply {
            change,
            accepted: refused(),
        });

        let lines = screen(&mut app);
        assert!(lines[0].contains("Inbox (1)"), "{lines:#?}");
        assert!(
            lines[4].starts_with(r#"GitHub refused to mark read "Add polling""#),
            "{lines:#?}"
        );
    }

    #[test]
    fn nothing_to_act_on_while_loading() {
        let mut app = App::default();

        assert!(app.execute(Command::MarkDone).is_none());
        assert!(app.execute(Command::MarkRead).is_none());
        assert!(app.execute(Command::Open).is_none());
    }
}
