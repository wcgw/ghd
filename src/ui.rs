//! The terminal UI: the Views as tabs, the one on screen listing its Notifications.

mod files;
mod pane_search;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use std::{fmt, io, mem, panic};

use anyhow::{Context as _, bail};
use chrono::{DateTime, TimeDelta, Utc};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures_util::future::OptionFuture;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Table, TableState, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use tokio::time::Instant;

use crate::assessment::{self, Assessed, Assessment, Assessments, Assessor, Readiness, Window};
use crate::cache::{Cache, Snapshot};
use crate::change::{Change, RecentChanges, Reply, send};
use crate::config::View;
use crate::done::DoneMarks;
use crate::fetched;
use crate::github::{GitHub, PreviewRequest};
use crate::heatmap;
use crate::inbox::{Inbox, Listing, Notification, SubjectKind, ThreadId};
use crate::markdown;
use crate::preview::{
    Body, Ci, Mark, Note, Place, Preview, Previews, PullRequestSummary, ReviewDecision, Shown,
};
use crate::search::{self, Search};
use crate::state::{Lookup, States, SubjectState};
use crate::theme::Theme;
use crate::ui::files::{Files, Pressed};

/// Widest the repository column gets, in terminal cells; longer names are cut off.
const MAX_REPOSITORY_WIDTH: u16 = 32;

/// Longest that quitting waits for changes still on their way to GitHub.
const QUIT_GRACE: Duration = Duration::from_secs(10);

/// How long the cursor stays on a Notification before its Preview is fetched, so that moving through the list
/// fetches nothing.
const PREVIEW_REST: Duration = Duration::from_millis(200);

/// The fewest lines the screen needs for the Preview pane: the list's row at the cursor, and a few lines of the
/// Preview.
const PREVIEW_MIN_HEIGHT: u16 = 12;

/// How many lines a turn of the mouse wheel scrolls a pane by, as desktops commonly do.
const WHEEL_LINES: u16 = 3;

/// Has the terminal report its mouse wheel to ghd, rather than turn it into arrow keys, until [`release_mouse`] or a
/// panic. Its buttons too, which ghd leaves be, but not the pointer's every move, which would have the screen drawn
/// again at each.
///
/// # Errors
///
/// Fails if the terminal can't be written to.
pub fn capture_mouse() -> io::Result<()> {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        release_mouse();
        hook(info);
    }));
    crossterm::execute!(io::stdout(), ReportMouse(true))
}

/// Has the terminal stop reporting its mouse, as before [`capture_mouse`].
pub fn release_mouse() {
    // A terminal that can't be written to is past caring.
    let _ = crossterm::execute!(io::stdout(), ReportMouse(false));
}

/// Turns the terminal's reporting of mouse buttons, the wheel's included, on or off: X11's (1000), with coordinates
/// as SGR's (1006).
struct ReportMouse(bool);

impl crossterm::Command for ReportMouse {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str(if self.0 {
            "\x1b[?1000h\x1b[?1006h"
        } else {
            "\x1b[?1006l\x1b[?1000l"
        })
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        // The console has no mode for buttons alone.
        if self.0 {
            crossterm::event::EnableMouseCapture.execute_winapi()
        } else {
            crossterm::event::DisableMouseCapture.execute_winapi()
        }
    }
}

/// Runs the UI on `terminal` until the user quits, showing `views` as tabs in that order, in `theme`, naming Subject
/// kinds with `kind_labels`. Meanwhile it
/// keeps the Inbox up to date from `github`, polling as often as GitHub allows, and sends it the changes the user
/// makes. Until the first fetch arrives, it shows what `cache` kept from last time; on quitting, it keeps the Inbox
/// there for next time. With `assessor`, voight assesses the text each Preview shows, once its model is loaded.
///
/// # Errors
///
/// Fails if the terminal can't be drawn on or read from, if changes made just before quitting were refused or
/// didn't reach GitHub in time, or if the Inbox can't be kept in `cache`. Failing to fetch the Inbox, or a refused
/// change while the UI runs, isn't an error here: the screen shows it, and the next poll tries again.
///
/// # Panics
///
/// If `views` is empty: the configuration always has the built-in Inbox View.
pub async fn run(
    terminal: &mut DefaultTerminal,
    github: &GitHub,
    views: Vec<View>,
    theme: Theme,
    kind_labels: KindLabels,
    cache: &Cache,
    mut assessor: Option<Assessor>,
) -> anyhow::Result<()> {
    let mut app = App::new(views, theme, kind_labels);
    if assessor.is_some() {
        app.assessments = Some(Assessments::default());
    }
    if let Some(snapshot) = cache.load().await {
        app.restore(snapshot, Utc::now());
    }
    let mut events = EventStream::new();
    // The fetch under way, if any; never more than one. Polled alongside terminal events, so the UI keeps drawing
    // and can be quit while pages arrive.
    let mut fetch = Some(Box::pin(fetch_inbox(github, Fetch::Whole)));
    // When to poll the Inbox next, once no fetch is under way.
    let mut next_poll = Instant::now();
    // Changes already on screen, on their way to GitHub.
    let mut in_flight = FuturesUnordered::new();
    // The lookup of Subjects' States under way, if any; never more than one.
    let mut lookup = None;
    // The fetch of a Preview under way, if any; never more than one. One the cursor has left still arrives, and is
    // kept for when it comes back.
    let mut preview_fetch = None;
    // The fetches of the Files screen's Changed files and of a whole file under way, if any; one of each at most.
    let mut files_fetch = None;
    let mut text_fetch = None;

    loop {
        if lookup.is_none() {
            let wanted = app.states_to_look_up();
            if !wanted.is_empty() {
                lookup = Some(Box::pin(look_up_states(github, wanted)));
            }
        }
        let now = Instant::now();
        app.note_cursor(now);
        if preview_fetch.is_none()
            && let Some(wanted) = app.preview_to_fetch(now)
        {
            preview_fetch = Some(Box::pin(fetch_preview(github, wanted)));
        }
        // When to wake up to fetch the Preview at the cursor, once it's stayed there.
        let preview_due = app.preview_due().map(|(due, _)| due);
        if files_fetch.is_none()
            && let Some(wanted) = app.files.list_to_fetch()
        {
            files_fetch = Some(Box::pin(files::fetch(github, wanted)));
        }
        if text_fetch.is_none()
            && let Some(wanted) = app.files.text_to_fetch()
        {
            text_fetch = Some(Box::pin(files::fetch_text(github, wanted)));
        }
        if let Some(assessor) = &assessor
            && let Some((key, text)) = app.assessment_to_start()
        {
            assessor.assess(key, text);
        }
        let fetching = fetch.is_some() || lookup.is_some();
        terminal
            .draw(|frame| app.render(frame, fetching))
            .context("drawing the terminal")?;

        tokio::select! {
            Some(looked_up) = OptionFuture::from(lookup.as_mut()), if lookup.is_some() => {
                lookup = None;
                app.states_looked_up(looked_up);
            }
            Some(fetched) = OptionFuture::from(fetch.as_mut()), if fetch.is_some() => {
                fetch = None;
                app.fetched(fetched);
                next_poll = Instant::now() + github.poll_interval();
            }
            () = tokio::time::sleep_until(next_poll), if fetch.is_none() => {
                fetch = Some(Box::pin(fetch_inbox(github, Fetch::IfChanged)));
            }
            Some(fetched) = OptionFuture::from(preview_fetch.as_mut()), if preview_fetch.is_some() => {
                preview_fetch = None;
                app.preview_fetched(fetched);
            }
            // Nothing to do but go round again, to fetch it.
            () = tokio::time::sleep_until(preview_due.unwrap_or(now)),
                if preview_fetch.is_none() && preview_due.is_some() => {}
            Some(fetched) = OptionFuture::from(files_fetch.as_mut()), if files_fetch.is_some() => {
                files_fetch = None;
                app.files.took_list(fetched);
            }
            Some(fetched) = OptionFuture::from(text_fetch.as_mut()), if text_fetch.is_some() => {
                text_fetch = None;
                app.files.took_text(fetched);
            }
            Some(said) = OptionFuture::from(assessor.as_mut().map(Assessor::next)), if assessor.is_some() => {
                match said {
                    Some(event) => app.voight_said(event),
                    // Its thread stopped, having said why.
                    None => assessor = None,
                }
            }
            Some(reply) = in_flight.next(), if !in_flight.is_empty() => app.settle(reply),
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => match app.press(key) {
                    Some(Outcome::Quit) => break,
                    // A refresh asked for while a fetch is under way gets that fetch.
                    Some(Outcome::Refresh) => {
                        if fetch.is_none() {
                            fetch = Some(Box::pin(fetch_inbox(github, Fetch::Whole)));
                        }
                    }
                    Some(Outcome::Send(changes)) => {
                        in_flight.extend(changes.into_iter().map(|change| send(github, change)));
                    }
                    None => {}
                },
                Some(Ok(Event::Mouse(mouse))) => app.mouse(mouse),
                // Anything else, a resize included, just needs the redraw at the top of the loop.
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("reading terminal events"),
                None => break,
            },
        }
    }

    let mut problems = Vec::new();

    // Dropping the changes still in flight could lose ones the screen already showed as made.
    if !in_flight.is_empty() {
        let unconfirmed = in_flight.len();
        app.status = Some(StatusMessage::Info(format!(
            "Sending {unconfirmed} change(s) to GitHub…"
        )));
        terminal
            .draw(|frame| app.render(frame, false))
            .context("drawing the terminal")?;
        match tokio::time::timeout(QUIT_GRACE, in_flight.collect::<Vec<_>>()).await {
            Ok(replies) => {
                for reply in replies {
                    if let Err(error) = &reply.accepted {
                        problems.push(format!("GitHub refused to {}: {error:#}", reply.change));
                    }
                    // Undoes a refused change, so that the Inbox kept for next time doesn't have it.
                    app.settle(reply);
                }
            }
            Err(_) => problems.push(format!(
                "GitHub didn't confirm {unconfirmed} change(s) made just before quitting; they may not have been made"
            )),
        }
    }

    if let Some(snapshot) = app.into_snapshot(Utc::now())
        && let Err(error) = cache.save(&snapshot).await
    {
        problems.push(format!("couldn't keep the Inbox for next time: {error:#}"));
    }
    if !problems.is_empty() {
        bail!("{}", problems.join("\n"));
    }
    Ok(())
}

/// How much of the Inbox to fetch.
#[derive(Debug, Clone, Copy)]
enum Fetch {
    Whole,
    /// The whole Inbox if it changed since the last fetch, nothing otherwise.
    IfChanged,
}

/// The outcome of fetching the Inbox.
struct Fetched {
    /// When the fetch began: changes GitHub confirmed later may be missing from `listing`.
    began: Instant,
    /// What GitHub lists; `None` if it hadn't changed.
    listing: anyhow::Result<Option<Listing>>,
}

/// Fetches as much of the Inbox from `github` as `fetch` says.
///
/// # Cancel Safety
///
/// Cancel safe, as [`GitHub::listing`] is.
async fn fetch_inbox(github: &GitHub, fetch: Fetch) -> Fetched {
    let began = Instant::now();
    let listing = match fetch {
        Fetch::Whole => github.listing().await.map(Some),
        Fetch::IfChanged => github.listing_if_changed().await,
    };
    Fetched { began, listing }
}

/// What `run` has to do about a key press, beyond what [`App::press`] did on screen.
#[derive(Debug)]
enum Outcome {
    Quit,
    /// Fetch the whole Inbox now, rather than at the next poll.
    Refresh,
    /// Send these changes to GitHub; the screen shows them made already. Never empty.
    Send(Vec<Change>),
}

/// The outcome of looking up Subjects' States.
struct LookedUp {
    asked: Vec<Lookup>,
    found: anyhow::Result<HashMap<ThreadId, SubjectState>>,
}

/// Looks up on `github` the States of the Subjects `asked` about.
///
/// # Cancel Safety
///
/// Cancel safe, as [`GitHub::subject_states`] is.
async fn look_up_states(github: &GitHub, asked: Vec<Lookup>) -> LookedUp {
    let found = {
        let subjects: Vec<(ThreadId, &str)> = asked
            .iter()
            .map(|lookup| (lookup.thread, lookup.subject_url.as_str()))
            .collect();
        github.subject_states(&subjects).await
    };
    LookedUp { asked, found }
}

/// A Preview to fetch, for the activity its Notification has.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WantedPreview {
    thread: ThreadId,
    as_of: DateTime<Utc>,
    request: PreviewRequest,
}

/// The outcome of fetching a Preview, for the thread and activity it was asked for.
struct PreviewFetched {
    thread: ThreadId,
    as_of: DateTime<Utc>,
    preview: anyhow::Result<Preview>,
}

/// Fetches the Preview `wanted` from `github`.
///
/// # Cancel Safety
///
/// Cancel safe, as [`GitHub::preview`] is.
async fn fetch_preview(github: &GitHub, wanted: WantedPreview) -> PreviewFetched {
    PreviewFetched {
        preview: github.preview(&wanted.request).await,
        thread: wanted.thread,
        as_of: wanted.as_of,
    }
}

/// What a key press can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Quit,
    /// Opens the command line, to type a command into as in Vim: `q` to quit.
    OpenCommandLine,
    /// Fetches the whole Inbox now, rather than at the next poll.
    Refresh,
    Down,
    Up,
    /// Moves the cursor to the first row of the list.
    First,
    /// Moves the cursor to the last row of the list.
    Last,
    /// Moves the cursor down by half the rows the list shows.
    HalfPageDown,
    /// Moves the cursor up by half the rows the list shows.
    HalfPageUp,
    /// Selects the Notification at the cursor, or unselects it, then moves the cursor down.
    ToggleSelected,
    /// Selects every Notification the View on screen shows, or unselects them if they're all selected.
    SelectAllShown,
    NextView,
    PreviousView,
    /// Shows the View at this index in tab order, counting from 0; nothing happens past the last View.
    ShowView(usize),
    /// Opens the prompt to type a Search into; on the Files screen, what to look for in the selected file.
    Search,
    /// On the Files screen, jumps to the next place what's looked for is found in the selected file.
    NextMatch,
    /// On the Files screen, jumps back to the previous place what's looked for is found in the selected file.
    PreviousMatch,
    /// Clears the Selection, or the Search if nothing is selected; on the Files screen, stops looking for anything in
    /// the selected file, or else closes the screen.
    Clear,
    /// Opens the Subjects in the browser, which also marks them read.
    Open,
    MarkRead,
    /// Marks Done; in the Done list, brings back to ghd's Inbox.
    MarkDone,
    /// Shows the Done list in every View in place of the Inbox, or the Inbox again.
    ToggleDone,
    /// Shows only the unread Notifications in the View on screen, or all of them again.
    ToggleUnreadOnly,
    /// Opens the Preview of the Notification at the cursor below the list, or closes it.
    TogglePreview,
    /// Scrolls the Preview down by half its height, and once its end shows, steps to the next comment; on the Files
    /// screen, scrolls the selected file.
    ScrollPreviewDown,
    /// Scrolls the Preview up by half its height, and once its top shows, steps to the previous comment, or the body
    /// before the first; on the Files screen, scrolls the selected file.
    ScrollPreviewUp,
    /// Shows the Changed files of the pull request at the cursor in place of the list, or closes them.
    OpenFiles,
    /// On the Files screen, shows the files whole rather than as their diffs, or as their diffs again.
    ToggleWholeFiles,
    /// Shows the Heatmap of the text voight assessed, on the Preview or the Files screen, or stops.
    ToggleHeatmap,
    Help,
}

/// The help's lines for commands, in order, each with the commands its keys stand for. Adding a command means adding
/// it to a line: the help tests check that every key the keymap knows is on one, and does what its line says.
const COMMAND_HELP: [(&[Command], KeyHelp); 17] = [
    (
        &[Command::Down, Command::Up, Command::First, Command::Last],
        KeyHelp {
            keys: "j  k  ↓  ↑  g  G",
            does: "move to the next, previous, first or last Notification",
        },
    ),
    (
        &[Command::HalfPageDown, Command::HalfPageUp],
        KeyHelp {
            keys: "Ctrl-D  Ctrl-U",
            does: "move half a page down or up",
        },
    ),
    (
        &[Command::ToggleSelected, Command::SelectAllShown],
        KeyHelp {
            keys: "Space  A",
            does: "select or not, and move down; all shown, or none",
        },
    ),
    (
        &[Command::NextView, Command::PreviousView],
        KeyHelp {
            keys: "Tab  Shift-Tab",
            does: "show the next or previous View",
        },
    ),
    (
        &[Command::ShowView(0)],
        KeyHelp {
            keys: "1–9",
            does: "show a View by its place in the tabs",
        },
    ),
    (
        &[Command::Open],
        KeyHelp {
            keys: "o  Enter",
            does: "open in the browser, which marks read",
        },
    ),
    (
        &[Command::MarkRead],
        KeyHelp {
            keys: "I",
            does: "mark read",
        },
    ),
    (
        &[Command::MarkDone],
        KeyHelp {
            keys: "e",
            does: "mark Done; in the Done list, back to the Inbox",
        },
    ),
    (
        &[Command::ToggleDone],
        KeyHelp {
            keys: "D",
            does: "show the Done list, or the Inbox again",
        },
    ),
    (
        &[Command::ToggleUnreadOnly],
        KeyHelp {
            keys: "u",
            does: "show unread only, or all again",
        },
    ),
    (
        &[Command::TogglePreview, Command::ToggleHeatmap],
        KeyHelp {
            keys: "p  H",
            does: "show or hide the Preview; voight's Heatmap",
        },
    ),
    (
        &[Command::ScrollPreviewDown, Command::ScrollPreviewUp],
        KeyHelp {
            keys: "J  K",
            does: "scroll the Preview, and step through its comments",
        },
    ),
    (
        &[Command::OpenFiles, Command::ToggleWholeFiles],
        KeyHelp {
            keys: "f  w",
            does: "a pull request's files; files whole, or their diffs",
        },
    ),
    (
        &[Command::Search, Command::NextMatch, Command::PreviousMatch],
        KeyHelp {
            keys: "/  n  N",
            does: "search the View or a file; next or previous match",
        },
    ),
    (
        &[Command::Clear],
        KeyHelp {
            keys: "Esc",
            does: "clear the selection, or else the search",
        },
    ),
    (
        &[Command::Refresh],
        KeyHelp {
            keys: "r",
            does: "fetch the Inbox now",
        },
    ),
    // On one line, so that the help fits a 24-line terminal.
    (
        &[Command::Help, Command::OpenCommandLine, Command::Quit],
        KeyHelp {
            keys: "?  :q  Ctrl-C",
            does: "show these keys; quit",
        },
    ),
];

/// What the help says about acting on the Selection, after the commands.
const SELECTION_HELP: &str = "With a selection, o, I and e act on all of it.";

/// The keys that work differently in the search prompt, as the help lists them after the commands.
const SEARCH_PROMPT_HELP: [KeyHelp; 3] = [
    KeyHelp {
        keys: "Tab  Shift-Tab",
        does: "complete a field's name, or the next or previous one",
    },
    KeyHelp {
        keys: "Enter",
        does: "keep the search",
    },
    KeyHelp {
        keys: "Esc",
        does: "put back the search there was before",
    },
];

/// A line in the help.
struct KeyHelp {
    /// The keys, separated by two spaces.
    keys: &'static str,
    does: &'static str,
}

/// The command a key press stands for. Marking read and Done use GitHub's own notification shortcuts.
fn command_for(key: KeyEvent) -> Option<Command> {
    match key.code {
        _ if is_interrupt(key) => Some(Command::Quit),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Command::HalfPageDown)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Command::HalfPageUp)
        }
        KeyCode::Char(':') => Some(Command::OpenCommandLine),
        KeyCode::Char('r') => Some(Command::Refresh),
        KeyCode::Char('j') | KeyCode::Down => Some(Command::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Command::Up),
        KeyCode::Char('g') => Some(Command::First),
        KeyCode::Char('G') => Some(Command::Last),
        KeyCode::Char(' ') => Some(Command::ToggleSelected),
        KeyCode::Char('A') => Some(Command::SelectAllShown),
        KeyCode::Tab => Some(Command::NextView),
        KeyCode::BackTab => Some(Command::PreviousView),
        KeyCode::Char(digit @ '1'..='9') => Some(Command::ShowView((digit as u8 - b'1').into())),
        KeyCode::Char('o') | KeyCode::Enter => Some(Command::Open),
        KeyCode::Char('I') => Some(Command::MarkRead),
        KeyCode::Char('e') => Some(Command::MarkDone),
        KeyCode::Char('D') => Some(Command::ToggleDone),
        KeyCode::Char('u') => Some(Command::ToggleUnreadOnly),
        KeyCode::Char('p') => Some(Command::TogglePreview),
        KeyCode::Char('J') => Some(Command::ScrollPreviewDown),
        KeyCode::Char('K') => Some(Command::ScrollPreviewUp),
        KeyCode::Char('f') => Some(Command::OpenFiles),
        KeyCode::Char('w') => Some(Command::ToggleWholeFiles),
        KeyCode::Char('H') => Some(Command::ToggleHeatmap),
        KeyCode::Char('/') => Some(Command::Search),
        KeyCode::Char('n') => Some(Command::NextMatch),
        KeyCode::Char('N') => Some(Command::PreviousMatch),
        KeyCode::Esc => Some(Command::Clear),
        KeyCode::Char('?') => Some(Command::Help),
        _ => None,
    }
}

/// Whether `key` is Ctrl-C, which quits even while the user types into a prompt.
fn is_interrupt(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Moves `cursor` with `moving`, no further than the last of `rows` rows (the first, if there are none). The table
/// would only bring a cursor past the end back when it's next drawn: until then, the row at the cursor would be none.
fn move_within(cursor: &mut TableState, rows: usize, moving: impl FnOnce(&mut TableState)) {
    moving(cursor);
    if let Some(row) = cursor.selected() {
        cursor.select(Some(row.min(rows.saturating_sub(1))));
    }
}

/// A one-line message at the bottom of the screen.
#[derive(Debug)]
enum StatusMessage {
    Info(String),
    Error(String),
    /// Why the latest fetch failed; it goes away when one succeeds.
    FetchFailed(String),
    /// That the Inbox on screen is the one kept from last time; it goes away when a fetched one arrives.
    Cached(String),
}

/// What the app has of the Inbox so far.
enum InboxState {
    Loading,
    /// The Inbox, and the Done list.
    Loaded(Listing),
    Failed(anyhow::Error),
}

/// A View on screen, with its cursor.
struct ShownView {
    view: View,
    /// The cursor's row among the Notifications the View shows.
    cursor: TableState,
    /// The View shows only its unread Notifications, while the Inbox is on screen.
    unread_only: bool,
}

/// Everything on screen.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the help, the Done list, States wanted and the Heatmap are independent of each other: they are not a \
              state to model as an enum"
)]
struct App {
    inbox: InboxState,
    /// The Views in tab order; never empty, the built-in Inbox View first.
    views: Vec<ShownView>,
    /// Index into `views` of the View on screen.
    current: usize,
    /// What the latest action did, or why it failed; `None` until there's something to say.
    status: Option<StatusMessage>,
    /// Changes the user made that a fetched Inbox may not show yet.
    recent: RecentChanges,
    /// The Search as the user typed it; empty when there's none.
    search_input: String,
    /// The Search `search_input` stands for. It narrows whichever View is on screen.
    search: Search,
    /// The prompt in the status line, while the user types a Search.
    search_prompt: Option<SearchPrompt>,
    /// What's typed on the command line, after `:`, while it's open.
    command_line: Option<String>,
    /// The Selection, by thread: only ever threads the Inbox holds, whichever View shows them.
    selection: HashSet<ThreadId>,
    /// The help is shown over the View.
    help_shown: bool,
    /// How many rows the list had room for at the last draw.
    rows_on_screen: u16,
    theme: Theme,
    kind_labels: KindLabels,
    /// The Subjects' States known so far.
    states: States,
    /// The user's word on which threads are Done, which GitHub's API can't say.
    marks: DoneMarks,
    /// The Done list is on screen, in place of the Inbox, in every View.
    showing_done: bool,
    /// An Inbox arrived since Subjects' States were last looked up: the ones it changed want looking up.
    states_wanted: bool,
    /// The Preview pane below the list, while it's open.
    preview: Option<PreviewPane>,
    /// The Previews fetched so far.
    previews: Previews,
    /// How many lines the screen had at the last draw.
    screen_height: u16,
    /// voight's Assessments of Previews, and whether it's ready to make more; `None` while voight is off.
    assessments: Option<Assessments>,
    /// The text voight assessed on screen shows as its Heatmap, on whichever Preview or file it is.
    heatmap: bool,
    /// The Files screen, open or not, and the Changed files fetched for it.
    files: Files,
}

/// Text on screen for voight to assess, and what it's of.
struct Assessable<'a> {
    key: assessment::Key,
    text: Cow<'a, str>,
    /// `text` is Markdown, to assess as the Preview draws it.
    markdown: bool,
}

/// The Preview pane, and where it's at.
#[derive(Debug, Default)]
struct PreviewPane {
    /// The thread at the cursor, and since when: its Preview is fetched once the cursor has stayed on it for
    /// [`PREVIEW_REST`].
    at: Option<(ThreadId, Instant)>,
    /// Where J and K stepped the Preview at the cursor to, and for its Notification's activity then; `None` while it's
    /// where it opens.
    stepped: Option<(DateTime<Utc>, Place)>,
    scroll: Scrolled,
}

/// How far down a pane of text is scrolled, and what fit in it at the last draw.
#[derive(Debug, Default)]
struct Scrolled {
    /// How many lines down.
    by: u16,
    /// How many lines the pane had room for.
    rows: u16,
    /// How many lines the text took, wrapped to the pane's width.
    lines: u16,
}

/// Which way the mouse wheel turned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wheel {
    Down,
    Up,
}

impl Scrolled {
    /// Scrolls down by half the pane, no further than to the text's last line at the bottom of the pane.
    fn half_down(&mut self) {
        self.down((self.rows / 2).max(1));
    }

    /// Scrolls up by half the pane, no further than the top.
    fn half_up(&mut self) {
        self.up((self.rows / 2).max(1));
    }

    /// Scrolls by a turn of the mouse wheel: [`WHEEL_LINES`], no further than the text's last line at the bottom of
    /// the pane, nor than its top.
    fn wheel(&mut self, turn: Wheel) {
        match turn {
            Wheel::Down => self.down(WHEEL_LINES),
            Wheel::Up => self.up(WHEEL_LINES),
        }
    }

    /// Scrolls down by `lines`, no further than to the text's last line at the bottom of the pane.
    fn down(&mut self, lines: u16) {
        self.by = self.by.saturating_add(lines).min(self.furthest());
    }

    /// Scrolls up by `lines`, no further than the top.
    fn up(&mut self, lines: u16) {
        self.by = self.by.saturating_sub(lines);
    }

    /// The furthest the text can be scrolled: until its last line is at the bottom of the pane.
    fn furthest(&self) -> u16 {
        self.lines.saturating_sub(self.rows)
    }

    /// Draws `text` in `block` on `area` of `frame`, as far down as it's scrolled, noting what fits; and when it
    /// doesn't all fit, a scrollbar on the right border, its thumb in `thumb` over the lines that show.
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        text: Paragraph,
        block: Block,
        thumb: Style,
    ) {
        // Inside the border.
        self.rows = area.height.saturating_sub(2);
        self.lines =
            u16::try_from(text.line_count(area.width.saturating_sub(2))).unwrap_or(u16::MAX);
        // Shorter text, or a taller pane, than at the last scroll.
        self.by = self.by.min(self.furthest());
        frame.render_widget(text.scroll((self.by, 0)).block(block), area);
        if self.lines > self.rows {
            // A position for each line the text can be scrolled down by: with the track as long as the pane's rows,
            // the thumb then spans the lines that show, out of all of them.
            let mut state =
                ScrollbarState::new(usize::from(self.furthest()) + 1).position(self.by.into());
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                // The border shows where the thumb isn't.
                .track_symbol(None)
                .thumb_symbol("┃")
                .thumb_style(thumb);
            // Between the border's corners.
            let track = area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            });
            frame.render_stateful_widget(scrollbar, track, &mut state);
        }
    }
}

/// The prompt the user types a Search into.
struct SearchPrompt {
    /// The Search input when the prompt opened, put back if the user cancels.
    input_before: String,
    /// The ways to complete the word being typed, while the user goes through them with Tab and Shift-Tab.
    completion: Option<Completion>,
}

/// The ways to complete the word being typed, and the one in the input now.
struct Completion {
    /// Each a whole input, as [`search::completions`] gives them; never empty.
    inputs: Vec<String>,
    /// Index into `inputs`.
    at: usize,
}

/// Which way to go through a list: Tab and Shift-Tab through the completions, n and N through where what's looked for
/// in a file is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Next,
    Previous,
}

impl App {
    /// The app waiting for the Inbox, with `views` as tabs in that order and the first of them on screen, in
    /// `theme`, naming Subject kinds with `kind_labels`.
    ///
    /// # Panics
    ///
    /// If `views` is empty.
    fn new(views: Vec<View>, theme: Theme, kind_labels: KindLabels) -> Self {
        assert!(
            !views.is_empty(),
            "the configuration always has the Inbox View"
        );
        let views = views
            .into_iter()
            .map(|view| ShownView {
                view,
                cursor: TableState::new().with_selected(Some(0)),
                unread_only: false,
            })
            .collect();
        Self {
            inbox: InboxState::Loading,
            views,
            current: 0,
            status: None,
            recent: RecentChanges::default(),
            search_input: String::new(),
            search: Search::default(),
            search_prompt: None,
            command_line: None,
            selection: HashSet::new(),
            help_shown: false,
            rows_on_screen: 0,
            theme,
            kind_labels,
            states: States::default(),
            states_wanted: false,
            marks: DoneMarks::default(),
            showing_done: false,
            preview: None,
            previews: Previews::default(),
            screen_height: 0,
            assessments: None,
            heatmap: false,
            files: Files::default(),
        }
    }

    /// Takes in the outcome of fetching the Inbox. A fetched Inbox replaces the one on screen, with the user's recent
    /// changes made again. A failed fetch leaves the Inbox on screen as it was, and says why.
    fn fetched(&mut self, fetched: Fetched) {
        match fetched.listing {
            // Unchanged since the last fetch.
            Ok(None) => {}
            Ok(Some(mut listing)) => {
                self.marks.apply(&mut listing);
                self.recent.reapply(&mut listing.inbox, fetched.began);
                self.replace_listing(listing);
                if matches!(
                    self.status,
                    Some(StatusMessage::FetchFailed(_) | StatusMessage::Cached(_))
                ) {
                    self.status = None;
                }
            }
            Err(error) => match self.inbox {
                InboxState::Loaded(_) => {
                    self.status = Some(StatusMessage::FetchFailed(format!(
                        "Couldn't refresh the Inbox, it may be out of date: {error:#}"
                    )));
                }
                InboxState::Loading | InboxState::Failed(_) => {
                    self.inbox = InboxState::Failed(error);
                }
            },
        }
    }

    /// Shows `listing` instead of the Inbox and Done list on screen. What it doesn't hold any more leaves the
    /// Selection and the States, and what it changed wants its State looked up.
    fn replace_listing(&mut self, listing: Listing) {
        self.selection.retain(|&thread| listing.contains(thread));
        self.states.retain_in(&listing);
        self.previews.retain_in(&listing);
        self.files.retain_in(&listing);
        if let Some(assessments) = &mut self.assessments {
            assessments.retain_in(&listing);
        }
        self.states_wanted = true;
        self.keeping_cursors(|app| app.inbox = InboxState::Loaded(listing));
    }

    /// Makes `input` the Search.
    fn set_search(&mut self, input: String) {
        self.keeping_cursors(|app| {
            app.search = Search::parse(&input);
            app.search_input = input;
        });
    }

    /// Whether the user is typing a Search, so keys go to the prompt.
    fn is_typing_search(&self) -> bool {
        self.search_prompt.is_some()
    }

    /// Edits the Search in the prompt with `key`, the View on screen narrowing as it changes. A character adds to
    /// it and Backspace takes the last one off; Tab and Shift-Tab complete the word being typed, going through the
    /// ways to complete it when pressed again; Enter closes the prompt, keeping the Search, and Esc closes it,
    /// putting back the Search there was before.
    fn type_search(&mut self, key: KeyEvent) {
        if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && let Some(prompt) = &mut self.search_prompt
        {
            prompt.completion = None;
        }
        match key.code {
            KeyCode::Tab => self.complete(Step::Next),
            KeyCode::BackTab => self.complete(Step::Previous),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut input = mem::take(&mut self.search_input);
                input.push(character);
                self.set_search(input);
            }
            KeyCode::Backspace => {
                let mut input = mem::take(&mut self.search_input);
                input.pop();
                self.set_search(input);
            }
            KeyCode::Enter => self.search_prompt = None,
            KeyCode::Esc => {
                if let Some(prompt) = self.search_prompt.take() {
                    self.set_search(prompt.input_before);
                }
            }
            _ => {}
        }
    }

    /// Completes the word being typed with the first (`Step::Next`) or last way to, or goes on to the next or
    /// previous way if the user is going through them already. Does nothing if the word can't be completed.
    fn complete(&mut self, step: Step) {
        let Some(prompt) = &mut self.search_prompt else {
            return;
        };
        let completion = if let Some(completion) = &mut prompt.completion {
            let count = completion.inputs.len();
            completion.at = match step {
                Step::Next => (completion.at + 1) % count,
                Step::Previous => (completion.at + count - 1) % count,
            };
            completion
        } else {
            let inputs = search::completions(&self.search_input);
            let at = match step {
                Step::Next => 0,
                Step::Previous => inputs.len().saturating_sub(1),
            };
            if inputs.is_empty() {
                return;
            }
            prompt.completion.insert(Completion { inputs, at })
        };
        let input = completion.inputs[completion.at].clone();
        self.set_search(input);
    }

    /// Edits the command line with `key`, as in Vim: a character adds to it, and Backspace takes the last one off or,
    /// with nothing typed, closes it; Enter closes it and carries out the command typed, and Esc closes it. Returns
    /// what's left for `run` to do.
    fn type_command(&mut self, key: KeyEvent) -> Option<Outcome> {
        let typed = self.command_line.as_mut()?;
        match key.code {
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                typed.push(character);
            }
            KeyCode::Backspace => {
                if typed.pop().is_none() {
                    self.command_line = None;
                }
            }
            KeyCode::Enter => {
                let typed = mem::take(typed);
                self.command_line = None;
                return self.carry_out(&typed);
            }
            KeyCode::Esc => self.command_line = None,
            _ => {}
        }
        None
    }

    /// Carries out the command `typed` on the command line: Vim's for quitting, the only one so far; says so if it's
    /// none. Returns what's left for `run` to do.
    fn carry_out(&mut self, typed: &str) -> Option<Outcome> {
        match typed.trim() {
            "" => None,
            // ghd has nothing to write, nor other windows to close: they all just quit.
            "q" | "quit" | "q!" | "quit!" | "qa" | "qall" | "qa!" | "qall!" => Some(Outcome::Quit),
            typed => {
                self.status = Some(StatusMessage::Error(format!("Not a command: {typed}")));
                None
            }
        }
    }

    /// The Subjects whose State to look up now: the ones the latest Inbox changed, once after it arrives. Empty
    /// otherwise, so that a lookup that failed waits for the next Inbox.
    fn states_to_look_up(&mut self) -> Vec<Lookup> {
        if !mem::take(&mut self.states_wanted) {
            return Vec::new();
        }
        let InboxState::Loaded(listing) = &self.inbox else {
            return Vec::new();
        };
        // The Done list's too, for when it's on screen.
        let mut stale = self.states.stale(&listing.inbox);
        stale.extend(self.states.stale(&listing.done));
        stale
    }

    /// Takes in Subjects' States looked up; a failed lookup is reported until an Inbox is fetched again.
    fn states_looked_up(&mut self, looked_up: LookedUp) {
        match looked_up.found {
            // A View that asks for a State may show more or fewer rows now.
            Ok(found) => self.keeping_cursors(|app| app.states.found(&looked_up.asked, &found)),
            Err(error) => {
                self.status = Some(StatusMessage::FetchFailed(format!(
                    "Couldn't look up the state of pull requests and issues: {error:#}"
                )));
            }
        }
    }

    /// Notes where the cursor is `now`, while the Preview is open. When it's moved to another Notification, the
    /// Preview shows that one where it opens, from its top, once fetched; one that failed to be fetched is tried
    /// again.
    fn note_cursor(&mut self, now: Instant) {
        let thread = self.at_cursor().map(|n| n.thread);
        let Some(pane) = &mut self.preview else {
            return;
        };
        if pane.at.map(|(at, _)| at) == thread {
            return;
        }
        pane.at = thread.map(|thread| (thread, now));
        pane.stepped = None;
        pane.scroll.by = 0;
        if let Some(thread) = thread {
            self.previews.forget_failed(thread);
        }
    }

    /// The Preview there is to fetch, and when: once the cursor has stayed on its Notification for
    /// [`PREVIEW_REST`]. `None` while the Preview is closed, or the one at the cursor is fetched, being fetched, or
    /// can't be.
    fn preview_due(&self) -> Option<(Instant, WantedPreview)> {
        if self.files.is_open() {
            return None;
        }
        let (thread, since) = self.preview.as_ref()?.at?;
        let notification = self.at_cursor().filter(|n| n.thread == thread)?;
        if self.previews.get(notification).is_some() {
            return None;
        }
        let request = PreviewRequest::for_notification(notification)?;
        Some((
            since + PREVIEW_REST,
            WantedPreview {
                thread,
                as_of: notification.updated_at,
                request,
            },
        ))
    }

    /// The Preview to fetch `now`, if one is due, noted as being fetched.
    fn preview_to_fetch(&mut self, now: Instant) -> Option<WantedPreview> {
        let (due, wanted) = self.preview_due()?;
        if due > now {
            return None;
        }
        self.previews.fetching(wanted.thread, wanted.as_of);
        Some(wanted)
    }

    /// Takes in a fetched Preview, or why it couldn't be fetched.
    fn preview_fetched(&mut self, fetched: PreviewFetched) {
        self.previews
            .fetched(fetched.thread, fetched.as_of, fetched.preview);
    }

    /// The Preview on screen, fetched, and the place it's at, with what that's of; `None` while it's closed, hidden
    /// by the Files screen, not fetched yet, or has nothing to show.
    fn preview_on_screen(&self) -> Option<(assessment::Key, &Preview, Place)> {
        if self.files.is_open() {
            return None;
        }
        let pane = self.preview.as_ref()?;
        let notification = self.at_cursor()?;
        let fetched::Fetched::Done(preview) = self.previews.get(notification)? else {
            return None;
        };
        let place = pane
            .stepped
            .filter(|(as_of, _)| *as_of == notification.updated_at)
            .map_or(preview.opens_on, |(_, place)| place);
        let key = assessment::Key {
            thread: notification.thread,
            as_of: notification.updated_at,
            part: assessment::Part::Preview(place),
        };
        Some((key, preview, place))
    }

    /// Scrolls the Preview down by half its height; once its end shows, steps to the next comment, from its top.
    fn preview_down(&mut self) {
        let next = self
            .preview_on_screen()
            .and_then(|(key, preview, place)| Some((key.as_of, preview.after(place)?)));
        let Some(pane) = &mut self.preview else {
            return;
        };
        if pane.scroll.by < pane.scroll.furthest() {
            pane.scroll.half_down();
        } else if let Some(next) = next {
            pane.stepped = Some(next);
            pane.scroll.by = 0;
        }
    }

    /// Scrolls the Preview up by half its height; once its top shows, steps to the previous comment, or the body
    /// before the first, from its top.
    fn preview_up(&mut self) {
        let previous = self
            .preview_on_screen()
            .and_then(|(key, _, place)| Some((key.as_of, place.before()?)));
        let Some(pane) = &mut self.preview else {
            return;
        };
        if pane.scroll.by > 0 {
            pane.scroll.half_up();
        } else if let Some(previous) = previous {
            pane.stepped = Some(previous);
        }
    }

    /// What's on screen for voight to assess: on the Files screen, the selected file, if it's Markdown shown whole;
    /// otherwise what the Preview shows. `None` when there's nothing to assess.
    fn assessable(&self) -> Option<Assessable<'_>> {
        if self.files.is_open() {
            return self.files.assessable();
        }
        let (key, preview, place) = self.preview_on_screen()?;
        let (text, markdown) = match preview.at(place) {
            Shown::Comment(comment) => (comment.body.as_str(), true),
            Shown::Body(Body::Markdown(body)) => (body.as_str(), true),
            Shown::Body(Body::Plain(body)) => (body.as_str(), false),
        };
        Some(Assessable {
            key,
            text: Cow::Borrowed(text),
            markdown,
        })
    }

    /// The text to assess now, and what it's of, noted as being assessed: what's [assessable](Self::assessable), once
    /// voight is ready and assessing nothing else. `None` otherwise, and when voight is off.
    fn assessment_to_start(&mut self) -> Option<(assessment::Key, String)> {
        let assessments = self.assessments.as_ref()?;
        if !assessments.can_assess() {
            return None;
        }
        let assessable = self.assessable()?;
        if assessments.get(&assessable.key).is_some() {
            return None;
        }
        // Markdown as ghd draws it: no HTML comments left by templates, say, which the author didn't write.
        let text = if assessable.markdown {
            markdown::as_github_shows(&assessable.text)
        } else {
            assessable.text.into_owned()
        };
        let key = assessable.key;
        self.assessments.as_mut()?.assessing(key.clone());
        Some((key, text))
    }

    /// Takes in what voight says: that its model is loaded, or why it couldn't be, or an Assessment.
    fn voight_said(&mut self, event: assessment::Event) {
        let Some(assessments) = &mut self.assessments else {
            return;
        };
        match event {
            assessment::Event::Loaded { fallback } => {
                assessments.loaded(true);
                if let Some(fallback) = fallback {
                    self.status = Some(StatusMessage::Info(fallback));
                }
            }
            assessment::Event::Failed(why) => {
                assessments.loaded(false);
                self.status = Some(StatusMessage::Error(format!(
                    "voight couldn't load its model: {why}"
                )));
            }
            assessment::Event::Assessed { key, assessment } => {
                assessments.assessed(key, assessment);
            }
        }
    }

    /// What the border of the Preview, or of the file on the Files screen, says of voight: that it's loading,
    /// assessing, or its Assessment, in the Heatmap's color for it while that shows; `None` when there's nothing to
    /// say, as for a text with too little prose.
    fn assessment_label(&self) -> Option<Span<'static>> {
        let assessments = self.assessments.as_ref()?;
        let assessable = self.assessable();
        let said = |label: &str| Some(Span::styled(label.to_owned(), self.theme.voight));
        match assessments.readiness() {
            // On the Files screen, only over a file it'll assess.
            Readiness::Loading if !self.files.is_open() || assessable.is_some() => {
                return said(" voight loading… ");
            }
            Readiness::Loading | Readiness::Failed => return None,
            Readiness::Ready => {}
        }
        match assessments.get(&assessable?.key) {
            None | Some(Assessed::Assessing) => said(" voight … "),
            Some(Assessed::Done(Some(assessment))) => {
                let style = if self.heatmap {
                    self.theme.voight.patch(self.theme.heat(assessment.share))
                } else {
                    self.theme.voight
                };
                let label = format!(" voight {:.0}% ", assessment.share * 100.0);
                Some(Span::styled(label, style))
            }
            Some(Assessed::Done(None)) => None,
            Some(Assessed::Failed(_)) => said(" voight failed "),
        }
    }

    /// The Assessment of the text on screen, once made; `None` while there's none, or voight is off.
    fn assessment_on_screen(&self) -> Option<&Assessment> {
        let key = self.assessable()?.key;
        match self.assessments.as_ref()?.get(&key)? {
            Assessed::Done(Some(assessment)) => Some(assessment),
            Assessed::Done(None) | Assessed::Assessing | Assessed::Failed(_) => None,
        }
    }

    /// The windows the text on screen is shaded by, while the Heatmap shows: its Assessment's, once made; none
    /// otherwise.
    fn heat(&self) -> &[Window] {
        if !self.heatmap {
            return &[];
        }
        self.assessment_on_screen()
            .map_or(&[], |assessment| assessment.windows.as_slice())
    }

    /// Shows the Heatmap, or stops; says why when it can't show now.
    fn toggle_heatmap(&mut self) {
        if self.assessments.is_none() {
            self.status = Some(StatusMessage::Info("No Heatmap: voight is off".to_owned()));
            return;
        }
        self.heatmap = !self.heatmap;
        if self.heatmap && self.assessment_on_screen().is_none() {
            self.status = Some(StatusMessage::Info(
                "The Heatmap shows on text voight has assessed".to_owned(),
            ));
        }
    }

    /// Shows `snapshot`'s Inbox, as it was `now`, until a fetched one arrives, with the View that was on screen and
    /// each View's cursor as they were, where they still can be.
    fn restore(&mut self, snapshot: Snapshot, now: DateTime<Utc>) {
        self.inbox = InboxState::Loaded(snapshot.listing);
        self.states = snapshot.states;
        self.marks = snapshot.marks;
        if let Some(index) = self
            .views
            .iter()
            .position(|shown_view| shown_view.view.name == snapshot.view)
        {
            self.current = index;
        }
        let cursors = self
            .views
            .iter()
            .map(|shown_view| snapshot.cursors.get(&shown_view.view.name).copied())
            .collect();
        self.move_cursors(cursors);
        let when = match age(snapshot.saved_at, now).as_str() {
            "now" => "just now".to_owned(),
            age => format!("{age} ago"),
        };
        self.status = Some(StatusMessage::Cached(format!(
            "Showing the Inbox from {when} until the latest arrives"
        )));
    }

    /// What to keep for next time, as of `now`: the Inbox and the Done list, the View on screen and each View's
    /// cursor; `None` until there's an Inbox.
    fn into_snapshot(self, now: DateTime<Utc>) -> Option<Snapshot> {
        let on_screen = self.on_screen();
        let cursors = self
            .views
            .iter()
            .filter_map(|shown_view| {
                let notification = on_screen.at_cursor(shown_view)?;
                Some((shown_view.view.name.clone(), notification.thread))
            })
            .collect();
        let view = self.views[self.current].view.name.clone();
        let InboxState::Loaded(listing) = self.inbox else {
            return None;
        };
        Some(Snapshot {
            listing,
            saved_at: now,
            states: self.states,
            marks: self.marks,
            view,
            cursors,
        })
    }

    /// Runs `change`, keeping each View's cursor on the same Notification if the View still shows it.
    fn keeping_cursors(&mut self, change: impl FnOnce(&mut Self)) {
        let on_screen = self.on_screen();
        let cursors = self
            .views
            .iter()
            .map(|shown_view| on_screen.at_cursor(shown_view).map(|n| n.thread))
            .collect();
        change(self);
        self.move_cursors(cursors);
    }

    /// Moves the cursor of the View on screen with `moving`, no further than the last row it shows.
    fn move_cursor(&mut self, moving: impl FnOnce(&mut TableState)) {
        let rows = self.on_screen().shown(&self.views[self.current]).count();
        move_within(&mut self.views[self.current].cursor, rows, moving);
    }

    /// Selects in each View the Notification of the thread at the View's index in `threads`, if the View shows it.
    fn move_cursors(&mut self, threads: Vec<Option<ThreadId>>) {
        // Not `self.on_screen()`: it would borrow the Views too.
        let on_screen = OnScreen::new(&self.inbox, self.showing_done, &self.search, &self.states);
        for (shown_view, thread) in self.views.iter_mut().zip(threads) {
            let row = thread
                .and_then(|thread| on_screen.shown(shown_view).position(|n| n.thread == thread));
            if let Some(row) = row {
                shown_view.cursor.select(Some(row));
            }
        }
    }

    /// Takes in a key press: while the help is shown, any key but Ctrl-C closes it; while the user types a Search,
    /// what to look for in a file or a command, every key but Ctrl-C goes to the prompt; otherwise the key's command
    /// is carried out on screen. Returns what's left for `run` to do.
    fn press(&mut self, key: KeyEvent) -> Option<Outcome> {
        if !is_interrupt(key) {
            if self.help_shown {
                self.help_shown = false;
                return None;
            }
            if self.is_typing_search() {
                self.type_search(key);
                return None;
            }
            if self.files.typed().is_some() {
                self.files.type_key(key);
                return None;
            }
            if self.command_line.is_some() {
                return self.type_command(key);
            }
        }
        let command = command_for(key)?;
        if self.files.is_open() {
            // The help goes over the Files screen, as over anything else; the Heatmap shades a file as it does a
            // Preview; and the command line is the same on any screen.
            if let Command::Help | Command::ToggleHeatmap | Command::OpenCommandLine = command {
                self.execute(command);
                return None;
            }
            let pressed = self.files.press(command);
            return self.pressed_in_files(pressed);
        }
        match command {
            Command::Quit => Some(Outcome::Quit),
            Command::Refresh => {
                // The Preview on screen too: what it shows can change without notifying, like labels.
                if let Some(thread) = self.at_cursor().map(|n| n.thread)
                    && let Some(pane) = &mut self.preview
                {
                    pane.stepped = None;
                    self.previews.forget(thread);
                    if let Some(assessments) = &mut self.assessments {
                        assessments.forget(thread);
                    }
                }
                Some(Outcome::Refresh)
            }
            command => {
                let changes = self.execute(command);
                (!changes.is_empty()).then_some(Outcome::Send(changes))
            }
        }
    }

    /// Takes what the mouse did: only a turn of its wheel does anything.
    fn mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollDown => self.wheel(Wheel::Down),
            MouseEventKind::ScrollUp => self.wheel(Wheel::Up),
            _ => {}
        }
    }

    /// Takes a turn of the mouse wheel: it scrolls what's read while it shows, the file on the Files screen or else
    /// the Preview, wherever the pointer is; otherwise it moves the cursor, as arrow keys do. Over the help, it does
    /// nothing.
    fn wheel(&mut self, turn: Wheel) {
        if self.help_shown {
            return;
        }
        let shows_more = self.screen_height >= PREVIEW_MIN_HEIGHT;
        let moving = match turn {
            Wheel::Down => Command::Down,
            Wheel::Up => Command::Up,
        };
        if self.files.is_open() {
            if shows_more {
                self.files.wheel(turn);
            } else {
                // Moving only ever leaves the screen to deal with it.
                self.files.press(moving);
            }
        } else if let Some(pane) = self.preview.as_mut().filter(|_| shows_more) {
            pane.scroll.wheel(turn);
        } else {
            self.execute(moving);
        }
    }

    /// Opens the Files screen for the pull request at the cursor; says why not if it isn't one, or the terminal is
    /// too short.
    fn open_files(&mut self) {
        // Cloned: the Notification is borrowed from the Inbox, which the screen outlives.
        let Some(notification) = self.at_cursor().cloned() else {
            return;
        };
        if let Some(why_not) = self.files.open(&notification, self.screen_height) {
            self.status = Some(why_not);
        }
    }

    /// Does what a key press on the Files screen left over, and returns what's left for `run` to do.
    fn pressed_in_files(&mut self, pressed: Pressed) -> Option<Outcome> {
        match pressed {
            Pressed::Nothing => None,
            Pressed::Quit => Some(Outcome::Quit),
            Pressed::Said(status) => {
                self.status = Some(status);
                None
            }
            Pressed::Forgot(thread) => {
                if let Some(assessments) = &mut self.assessments {
                    assessments.forget(thread);
                }
                None
            }
            Pressed::Opened(thread) => {
                let changes = self.mark_read(vec![thread]);
                (!changes.is_empty()).then_some(Outcome::Send(changes))
            }
        }
    }

    /// Carries out `command` on screen. Returns the changes to send to GitHub, if the command makes any.
    #[expect(
        clippy::match_same_arms,
        reason = "the arms that do nothing here do nothing for different reasons, which each one says"
    )]
    fn execute(&mut self, command: Command) -> Vec<Change> {
        match command {
            // Nothing to do on screen: `press` hands these to `run`.
            Command::Quit | Command::Refresh => {}
            Command::OpenCommandLine => self.command_line = Some(String::new()),
            Command::Down => self.move_cursor(TableState::select_next),
            Command::Up => self.move_cursor(TableState::select_previous),
            Command::First => self.move_cursor(TableState::select_first),
            Command::Last => self.move_cursor(TableState::select_last),
            Command::HalfPageDown => {
                let half_page = (self.rows_on_screen / 2).max(1);
                self.move_cursor(|cursor| cursor.scroll_down_by(half_page));
            }
            Command::HalfPageUp => {
                let half_page = (self.rows_on_screen / 2).max(1);
                self.move_cursor(|cursor| cursor.scroll_up_by(half_page));
            }
            Command::ToggleSelected => {
                if let Some(thread) = self.at_cursor().map(|n| n.thread) {
                    if !self.selection.remove(&thread) {
                        self.selection.insert(thread);
                    }
                    self.move_cursor(TableState::select_next);
                }
            }
            Command::SelectAllShown => {
                let shown: Vec<ThreadId> = self
                    .on_screen()
                    .shown(&self.views[self.current])
                    .map(|n| n.thread)
                    .collect();
                if !shown.is_empty() && shown.iter().all(|thread| self.selection.contains(thread)) {
                    for thread in &shown {
                        self.selection.remove(thread);
                    }
                } else {
                    self.selection.extend(shown);
                }
            }
            Command::NextView => self.current = (self.current + 1) % self.views.len(),
            Command::PreviousView => {
                self.current = (self.current + self.views.len() - 1) % self.views.len();
            }
            Command::ShowView(index) => {
                if index < self.views.len() {
                    self.current = index;
                }
            }
            // Only a file is looked in, on the Files screen.
            Command::NextMatch | Command::PreviousMatch => {}
            Command::Search => {
                self.search_prompt = Some(SearchPrompt {
                    input_before: self.search_input.clone(),
                    completion: None,
                });
            }
            Command::Clear => {
                if self.selection.is_empty() {
                    self.set_search(String::new());
                } else {
                    self.selection.clear();
                }
            }
            Command::Help => self.help_shown = true,
            Command::ToggleHeatmap => self.toggle_heatmap(),
            Command::ToggleDone => {
                // The Selection is of the list that was on screen.
                self.selection.clear();
                self.keeping_cursors(|app| app.showing_done = !app.showing_done);
            }
            Command::ToggleUnreadOnly if self.showing_done => {
                self.status = Some(StatusMessage::Info(
                    "Unread only is for the Inbox, not the Done list".to_owned(),
                ));
            }
            Command::ToggleUnreadOnly => self.keeping_cursors(|app| {
                let shown_view = &mut app.views[app.current];
                shown_view.unread_only = !shown_view.unread_only;
            }),
            Command::TogglePreview if self.preview.is_some() => self.preview = None,
            Command::TogglePreview if self.screen_height < PREVIEW_MIN_HEIGHT => {
                self.status = Some(StatusMessage::Info(format!(
                    "The Preview needs a terminal at least {PREVIEW_MIN_HEIGHT} lines tall"
                )));
            }
            Command::TogglePreview => self.preview = Some(PreviewPane::default()),
            Command::OpenFiles => self.open_files(),
            // Only on the Files screen, which `press` hands its keys to.
            Command::ToggleWholeFiles => {}
            Command::ScrollPreviewDown => self.preview_down(),
            Command::ScrollPreviewUp => self.preview_up(),
            Command::Open => return self.open(),
            Command::MarkRead => {
                let targets = self.take_targets();
                let changes = self.mark_read(targets);
                if changes.len() > 1 {
                    self.status = Some(StatusMessage::Info(format!(
                        "Marked {} read",
                        changes.len()
                    )));
                }
                return changes;
            }
            Command::MarkDone => return self.mark_done(),
        }
        Vec::new()
    }

    /// What an action acts on, taking it: the Selection, in the order of the list on screen, which empties it; or else the
    /// Notification at the cursor. Empty when there's nothing to act on.
    fn take_targets(&mut self) -> Vec<ThreadId> {
        if self.selection.is_empty() {
            return self.at_cursor().map(|n| n.thread).into_iter().collect();
        }
        let selection = mem::take(&mut self.selection);
        self.on_screen()
            .list
            .into_iter()
            .flat_map(Inbox::iter)
            .filter(|n| selection.contains(&n.thread))
            .map(|n| n.thread)
            .collect()
    }

    /// Opens what an action acts on in the browser, and marks read what was opened. Stops at the first Subject the
    /// browser can't be started for, and says so.
    fn open(&mut self) -> Vec<Change> {
        let targets = self.take_targets();
        let mut opened = Vec::new();
        for thread in targets {
            let Some(url) = self
                .on_screen()
                .list
                .and_then(|list| list.get(thread))
                .map(|n| n.subject_url.clone())
            else {
                continue;
            };
            if let Err(error) = open::that_detached(&url) {
                self.status = Some(StatusMessage::Error(format!(
                    "Couldn't open a browser: {error}"
                )));
                break;
            }
            opened.push(thread);
        }
        self.mark_read(opened)
    }

    /// Clears Unread on the Notifications of `threads`. Returns a change for each that was unread, holding it as
    /// it was before.
    fn mark_read(&mut self, threads: Vec<ThreadId>) -> Vec<Change> {
        let Some(Listing { inbox, .. }) = self.listing_mut() else {
            return Vec::new();
        };
        let changes: Vec<Change> = threads
            .into_iter()
            .filter_map(|thread| {
                // Cloned to keep it as it was, should the change need undoing.
                let notification = inbox.get(thread)?.clone();
                inbox
                    .mark_read(thread)
                    .then_some(Change::MarkRead(notification))
            })
            .collect();
        self.made(changes)
    }

    /// Moves what an action acts on from the Inbox to the Done list, in every View. Returns a change for each; the
    /// cursor moves to the Notification below. In the Done list, brings it back instead (see [`App::bring_back`]).
    fn mark_done(&mut self) -> Vec<Change> {
        if self.showing_done {
            self.bring_back();
            return Vec::new();
        }
        let targets = self.take_targets();
        let Some(listing) = self.listing_mut() else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for notification in targets
            .into_iter()
            .filter_map(|thread| listing.inbox.remove(thread))
        {
            // Cloned: the Done list shows it, and the change keeps it as it was, should GitHub refuse.
            listing.done.insert(notification.clone());
            changes.push(Change::MarkDone(notification));
        }
        for change in &changes {
            if let Change::MarkDone(n) = change {
                self.marks.mark_done(n);
            }
        }
        match changes.as_slice() {
            [] => {}
            [Change::MarkDone(notification)] => {
                self.status = Some(StatusMessage::Info(format!(
                    "Marked Done: {}",
                    notification.subject_title
                )));
            }
            changes => {
                self.status = Some(StatusMessage::Info(format!(
                    "Marked {} Done",
                    changes.len()
                )));
            }
        }
        self.made(changes)
    }

    /// Moves what an action acts on from the Done list back to ghd's Inbox, and keeps it there until GitHub lists it
    /// unread or with newer activity. Only in ghd: GitHub's API can't move a thread back to the inbox, so GitHub
    /// keeps it Done.
    fn bring_back(&mut self) {
        let targets = self.take_targets();
        let Some(listing) = self.listing_mut() else {
            return;
        };
        let brought: Vec<Notification> = targets
            .into_iter()
            .filter_map(|thread| listing.done.remove(thread))
            .collect();
        for notification in &brought {
            self.marks.mark_not_done(notification);
        }
        self.status = match brought.as_slice() {
            [] => return,
            [notification] => Some(StatusMessage::Info(format!(
                "Back in ghd's Inbox (GitHub keeps it Done): {}",
                notification.subject_title
            ))),
            brought => Some(StatusMessage::Info(format!(
                "{} back in ghd's Inbox (GitHub keeps them Done)",
                brought.len()
            ))),
        };
        if let Some(listing) = self.listing_mut() {
            for notification in brought {
                listing.inbox.insert(notification);
            }
        }
    }

    /// Notes `changes`, made on screen, as on their way to GitHub, and hands them back to be sent.
    fn made(&mut self, changes: Vec<Change>) -> Vec<Change> {
        for change in &changes {
            self.recent.made(change);
        }
        changes
    }

    /// Takes in GitHub's answer to an earlier change: a refused change is undone on screen and reported.
    fn settle(&mut self, reply: Reply) {
        let Err(error) = reply.accepted else {
            self.recent.confirmed(&reply.change, Instant::now());
            return;
        };
        self.recent.refused(&reply.change);
        self.status = Some(StatusMessage::Error(format!(
            "GitHub refused to {}, undone: {error:#}",
            reply.change
        )));
        if let Change::MarkDone(n) = &reply.change {
            self.marks.forget(n.thread);
        }
        if let Some(listing) = self.listing_mut() {
            if let Change::MarkDone(n) = &reply.change {
                listing.done.remove(n.thread);
            }
            reply.change.undo(&mut listing.inbox);
        }
    }

    /// The Notification at the cursor of the View on screen; `None` until the Inbox arrives, or if the View shows none.
    fn at_cursor(&self) -> Option<&Notification> {
        self.on_screen().at_cursor(&self.views[self.current])
    }

    /// The list on screen, the Inbox or the Done list, and what narrows it.
    fn on_screen(&self) -> OnScreen<'_> {
        OnScreen::new(&self.inbox, self.showing_done, &self.search, &self.states)
    }

    /// The Inbox; `None` until it arrives.
    #[cfg(test)]
    fn inbox(&self) -> Option<&Inbox> {
        match &self.inbox {
            InboxState::Loaded(listing) => Some(&listing.inbox),
            InboxState::Loading | InboxState::Failed(_) => None,
        }
    }

    fn listing_mut(&mut self) -> Option<&mut Listing> {
        match &mut self.inbox {
            InboxState::Loaded(listing) => Some(listing),
            InboxState::Loading | InboxState::Failed(_) => None,
        }
    }

    /// Draws the app on `frame`, saying so when a fetch of the Inbox is under way (`fetching`).
    fn render(&mut self, frame: &mut Frame, fetching: bool) {
        // The theme's text style, background included, under everything else.
        frame.render_widget(Block::new().style(self.theme.text), frame.area());
        self.screen_height = frame.area().height;
        let [view_area, status_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        self.render_status(frame, status_area);
        let tall_enough = self.screen_height >= PREVIEW_MIN_HEIGHT;
        // The Preview or the file below is what's read: a quarter is plenty for the list.
        let [list_area, read_area] =
            Layout::vertical([Constraint::Ratio(1, 4), Constraint::Ratio(3, 4)]).areas(view_area);
        // On a terminal made too short while they're open, the Preview and a file wait for it to grow again.
        if self.files.is_open() {
            // Taken before the Files screen is borrowed, which it would otherwise be twice over.
            let label = self.assessment_label();
            let heat = self.heat().to_vec();
            let theme = self.theme;
            if tall_enough {
                // Less than its quarter if that lists every file, the file below taking the rest. A message in their
                // place keeps it: there's no file to give the rest to.
                let list_height = self
                    .files
                    .lines_listing_all()
                    .map_or(list_area.height, |all| all.min(list_area.height));
                let [list_area, file_area] =
                    Layout::vertical([Constraint::Length(list_height), Constraint::Fill(1)])
                        .areas(view_area);
                self.files.render_list(frame, list_area, &theme);
                self.files
                    .render_selected(frame, file_area, &theme, label, &heat);
            } else {
                self.files.render_list(frame, view_area, &theme);
            }
        } else if self.preview.is_some() && tall_enough {
            self.render_view(frame, list_area, fetching);
            self.render_preview(frame, read_area);
        } else {
            self.render_view(frame, view_area, fetching);
        }
        if self.help_shown {
            render_help(frame, &self.theme);
        }
    }

    /// Draws the View on screen in `area`, its tabs and what's going on in its border.
    fn render_view(&mut self, frame: &mut Frame, area: Rect, fetching: bool) {
        // Inside the border.
        self.rows_on_screen = area.height.saturating_sub(2);
        let theme = &self.theme;
        let mut block = Block::bordered()
            .border_style(theme.border)
            .title(self.tabs())
            .title_bottom(Line::styled(" ? keys ", theme.hint).right_aligned());
        if fetching {
            block = block.title_top(Line::styled(" fetching… ", theme.fetching).right_aligned());
        }
        // Not `self.on_screen()`: it would borrow the View on screen too, whose cursor the table moves.
        let on_screen = OnScreen::new(&self.inbox, self.showing_done, &self.search, &self.states);
        let unread_only = on_screen.unread_only(&self.views[self.current]);
        if !self.selection.is_empty() {
            let count = format!(" {} selected ", self.selection.len());
            block = block.title_bottom(Line::styled(count, theme.selected));
        }
        if unread_only {
            block = block.title_bottom(Line::styled(" ● unread only ", theme.unread_marker));
        }
        if !self.search_input.is_empty() && !self.is_typing_search() {
            block = block.title_bottom(Line::styled(
                format!(" /{} ", self.search_input),
                theme.search,
            ));
        }
        let current = &mut self.views[self.current];
        let rows: Vec<&Notification> = on_screen.shown(current).collect();
        let message = match &self.inbox {
            InboxState::Loading => Paragraph::new("Fetching notifications…").style(theme.message),
            InboxState::Failed(error) => {
                Paragraph::new(format!("Couldn't fetch notifications: {error:#}"))
                    .style(theme.error)
                    .wrap(Wrap { trim: false })
            }
            InboxState::Loaded(_) if on_screen.list.is_none_or(Inbox::is_empty) => {
                Paragraph::new(if self.showing_done {
                    "Nothing in the Done list."
                } else {
                    "Nothing in your Inbox."
                })
                .style(theme.message)
            }
            InboxState::Loaded(_)
                if rows.is_empty()
                    && on_screen
                        .matching(&current.view, &Search::default())
                        .next()
                        .is_none() =>
            {
                Paragraph::new(if self.showing_done {
                    "Nothing in the Done list matches this View."
                } else {
                    "Nothing in your Inbox matches this View."
                })
                .style(theme.message)
            }
            // The View shows some, but the Search or unread only leaves none.
            InboxState::Loaded(_) if rows.is_empty() => {
                Paragraph::new(match (unread_only, self.search_input.is_empty()) {
                    (true, true) => "Nothing unread in this View.",
                    (true, false) => "Nothing unread in this View matches the search.",
                    (false, _) => "Nothing in this View matches the search.",
                })
                .style(theme.message)
            }
            InboxState::Loaded(_) => {
                let look = RowLook {
                    theme,
                    kind_labels: self.kind_labels,
                    selection: &self.selection,
                    states: &self.states,
                };
                let table = notifications_table(&rows, Utc::now(), &look).block(block);
                frame.render_stateful_widget(table, area, &mut current.cursor);
                return;
            }
        };
        frame.render_widget(message.block(block), area);
    }

    /// Draws the Preview of the Notification at the cursor in `area`, as far down as it's scrolled.
    fn render_preview(&mut self, frame: &mut Frame, area: Rect) {
        let theme = self.theme;
        // Inside the border.
        let width = area.width.saturating_sub(2);
        let text = Paragraph::new(self.preview_lines(Utc::now(), width)).wrap(Wrap { trim: false });
        let comments = self
            .preview_on_screen()
            .filter(|(_, preview, _)| !preview.comments.is_empty())
            .map(|(_, preview, place)| {
                let count = preview.comments.len() as u64;
                match place {
                    Place::Body => format!("· {} ", counted(count, "comment")),
                    Place::Comment(index) => format!("· comment {} of {count} ", index + 1),
                }
            });
        let mut block = Block::bordered()
            .border_style(theme.border)
            .title(Line::from(vec![
                Span::styled(" Preview ", theme.tab),
                Span::styled(comments.unwrap_or_default(), theme.age),
            ]));
        if let Some(label) = self.assessment_label() {
            block = block.title_bottom(Line::from(label).right_aligned());
        }
        if let Some(pane) = &mut self.preview {
            pane.scroll
                .render(frame, area, text, block, theme.scrollbar);
        }
    }

    /// What the Preview pane shows of the Notification at the cursor, as of `now`, `width` cells wide: its Preview
    /// once fetched, and until then, or when there's none to fetch, what the Notification itself says.
    fn preview_lines(&self, now: DateTime<Utc>, width: u16) -> Vec<Line<'static>> {
        let theme = &self.theme;
        let Some(notification) = self.at_cursor() else {
            return vec![Line::styled("Nothing to preview.", theme.message)];
        };
        let (says, style) = match (
            PreviewRequest::for_notification(notification),
            self.previews.get(notification),
        ) {
            (None, _) => (
                "No Preview for this kind of Notification yet: o opens it.".to_owned(),
                theme.message,
            ),
            (Some(_), Some(fetched::Fetched::Done(preview))) => {
                let place = self
                    .preview_on_screen()
                    .map_or(preview.opens_on, |(_, _, place)| place);
                return preview_lines(
                    preview,
                    place,
                    &notification.repository,
                    theme,
                    now,
                    width,
                    self.heat(),
                );
            }
            (Some(_), Some(fetched::Fetched::Failed(why))) => {
                (format!("Couldn't fetch the Preview: {why}"), theme.error)
            }
            (Some(_), None | Some(fetched::Fetched::Fetching)) => {
                ("Fetching…".to_owned(), theme.message)
            }
        };
        let mut lines = vec![
            Line::styled(notification.subject_title.clone(), theme.title.bold()),
            separated(
                [
                    Some(Span::styled(
                        notification.repository.clone(),
                        theme.repository,
                    )),
                    self.states
                        .get(notification)
                        .map(|state| state_span(state, theme)),
                    Some(Span::styled(notification.reason.to_string(), theme.reason)),
                    Some(Span::styled(age(notification.updated_at, now), theme.age)),
                ]
                .into_iter()
                .flatten(),
            ),
            Line::default(),
        ];
        lines.push(Line::styled(says, style));
        lines
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        // The prompt of a Search, of what's looked for in a file, or the command line, after the key that opened it:
        // only a Search's has completions.
        let prompt = match (&self.search_prompt, self.files.typed(), &self.command_line) {
            (Some(prompt), _, _) => {
                Some(('/', self.search_input.as_str(), prompt.completion.as_ref()))
            }
            (None, Some(typed), _) => Some(('/', typed, None)),
            (None, None, Some(typed)) => Some((':', typed.as_str(), None)),
            (None, None, None) => None,
        };
        if let Some((opened_by, typed, completion)) = prompt {
            let typed = format!("{opened_by}{typed}");
            let width = u16::try_from(typed.chars().count()).unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(area.x.saturating_add(width), area.y));
            let mut spans = vec![Span::styled(typed, self.theme.prompt)];
            // The completed words to go through, the one in the input highlighted.
            if let Some(completion) = completion {
                spans.push(Span::raw("  "));
                for (index, input) in completion.inputs.iter().enumerate() {
                    let word = input.rsplit(char::is_whitespace).next().unwrap_or(input);
                    let style = if index == completion.at {
                        self.theme.completion_current
                    } else {
                        self.theme.completion
                    };
                    spans.push(Span::styled(word.to_owned(), style));
                    spans.push(Span::raw(" "));
                }
            }
            frame.render_widget(Line::from(spans), area);
            return;
        }
        let status = match &self.status {
            None => return,
            Some(StatusMessage::Info(text) | StatusMessage::Cached(text)) => {
                Line::styled(text.as_str(), self.theme.status)
            }
            Some(StatusMessage::Error(text) | StatusMessage::FetchFailed(text)) => {
                Line::styled(text.as_str(), self.theme.error)
            }
        };
        frame.render_widget(status, area);
    }

    /// The tabs: each View's name and, once the Inbox has arrived, how many unread Notifications it shows, the
    /// Search included; in the Done list, which is all read, how many Notifications. The View on screen is
    /// highlighted.
    fn tabs(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        if self.showing_done {
            spans.extend([
                Span::styled("Done", self.theme.tab_current),
                Span::raw(": "),
            ]);
        }
        let on_screen = self.on_screen();
        for (index, shown_view) in self.views.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" │ "));
            }
            let label = match &self.inbox {
                InboxState::Loaded(_) => {
                    let shown_here = on_screen.shown(shown_view);
                    let count = if self.showing_done {
                        shown_here.count()
                    } else {
                        shown_here.filter(|n| n.unread).count()
                    };
                    format!("{} ({count})", shown_view.view.name)
                }
                InboxState::Loading | InboxState::Failed(_) => shown_view.view.name.clone(),
            };
            let style = if index == self.current {
                self.theme.tab_current
            } else {
                self.theme.tab
            };
            spans.push(Span::styled(label, style));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }
}

/// The lines of `preview`, of a Subject in `repository`, as of `now`, `width` cells wide: its header, then what it
/// shows at `place`, as the Heatmap of `heat`, the windows of its Assessment, if there are any.
fn preview_lines(
    preview: &Preview,
    place: Place,
    repository: &str,
    theme: &Theme,
    now: DateTime<Utc>,
    width: u16,
    heat: &[Window],
) -> Vec<Line<'static>> {
    let ago = |then| match age(then, now).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    };
    let who = |login: Option<&String>| {
        login.map_or_else(|| "Someone".to_owned(), |login| format!("@{login}"))
    };
    let labels =
        (!preview.labels.is_empty()).then(|| Span::styled(preview.labels.join(", "), theme.reason));
    let discussion = preview.discussion.as_ref();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{} ", preview.reference), theme.age),
            Span::styled(preview.title.clone(), theme.title.bold()),
        ]),
        separated(
            [
                Some(Span::styled(repository.to_owned(), theme.repository)),
                discussion.map(|discussion| Span::raw(discussion.category.clone())),
                preview.state.map(|state| state_span(state, theme)),
                discussion
                    .filter(|discussion| discussion.answered)
                    .map(|_| Span::raw("answered")),
                preview
                    .author
                    .as_ref()
                    .map(|author| Span::raw(format!("@{author}"))),
                labels,
            ]
            .into_iter()
            .flatten(),
        ),
    ];
    if let Some(closed) = &preview.closed {
        let how = if closed.merged { "Merged" } else { "Closed" };
        let by = closed
            .by
            .as_ref()
            .map(|by| format!(" by @{by}"))
            .unwrap_or_default();
        let reason = match preview.state {
            Some(SubjectState::Completed) => " as completed",
            Some(SubjectState::NotPlanned) => " as not planned",
            _ => "",
        };
        lines.push(Line::styled(
            format!("{how}{by}{reason} {}", ago(closed.at)),
            theme.age,
        ));
    }
    if let Some(summary) = &preview.pull_request {
        lines.push(pull_request_line(summary, theme));
    }
    lines.push(Line::default());
    let shown = preview.at(place);
    match &preview.note {
        Some(Note::BodyMentionsYou) if matches!(shown, Shown::Body(_)) => lines.push(Line::styled(
            format!(
                "{} mentioned you in the description",
                who(preview.author.as_ref())
            ),
            theme.age,
        )),
        Some(Note::MentionNotLookedFor(why)) => {
            lines.push(Line::styled(
                format!("Couldn't look for the mention: {why}"),
                theme.hint,
            ));
            lines.push(Line::default());
        }
        Some(Note::BodyMentionsYou) | None => {}
    }
    match shown {
        Shown::Comment(comment) => {
            let did = match comment.mark {
                None => "commented",
                Some(Mark::MentionsYou) => "mentioned you",
                Some(Mark::Answer) => "answered",
            };
            let on = comment
                .path
                .as_ref()
                .map(|path| format!(" on {path}"))
                .unwrap_or_default();
            lines.push(Line::styled(
                format!(
                    "{} {did}{on} {}",
                    who(comment.author.as_ref()),
                    ago(comment.published_at)
                ),
                theme.age,
            ));
            lines.push(Line::default());
            lines.extend(markdown::lines(&comment.body, theme, width, heat));
        }
        Shown::Body(Body::Markdown(body) | Body::Plain(body)) if body.trim().is_empty() => {
            lines.push(Line::styled("No description.", theme.message));
        }
        Shown::Body(Body::Markdown(body)) => {
            lines.extend(markdown::lines(body, theme, width, heat));
        }
        Shown::Body(Body::Plain(body)) => {
            let mut body_lines: Vec<Line<'static>> = body
                .lines()
                .map(|line| Line::raw(line.to_owned()))
                .collect();
            heatmap::shade(&mut body_lines, body, heat, theme);
            lines.extend(body_lines);
        }
    }
    lines
}

/// `count` of `what`: `1 file`, `3 files`.
fn counted(count: u64, what: &str) -> String {
    match count {
        1 => format!("1 {what}"),
        count => format!("{count} {what}s"),
    }
}

/// A pull request's line in its Preview: its changes, then its checks and review decision, when known.
fn pull_request_line(summary: &PullRequestSummary, theme: &Theme) -> Line<'static> {
    let mut line = Line::from(vec![
        Span::styled(format!("+{}", summary.additions), theme.diff_added),
        Span::raw(" "),
        Span::styled(format!("\u{2212}{}", summary.deletions), theme.diff_removed),
    ]);
    let rest = [
        Some(Span::raw(counted(summary.changed_files, "file"))),
        Some(Span::raw(counted(summary.commits, "commit"))),
        summary.ci.map(|ci| match ci {
            Ci::Passing => Span::styled("CI passing", theme.ci_passing),
            Ci::Failing => Span::styled("CI failing", theme.ci_failing),
            Ci::Pending => Span::styled("CI pending", theme.ci_pending),
        }),
        summary.review.map(|review| {
            Span::raw(match review {
                ReviewDecision::Approved => "approved",
                ReviewDecision::ChangesRequested => "changes requested",
                ReviewDecision::ReviewRequired => "review required",
            })
        }),
    ];
    for span in rest.into_iter().flatten() {
        line.push_span(Span::raw(" · "));
        line.push_span(span);
    }
    line
}

/// `state`'s name, in its color in `theme`.
fn state_span(state: SubjectState, theme: &Theme) -> Span<'static> {
    Span::styled(state.name().replace('_', " "), theme.state(state))
}

/// One line of `spans`, with ` · ` between them.
fn separated(spans: impl IntoIterator<Item = Span<'static>>) -> Line<'static> {
    let mut line = Line::default();
    for (index, span) in spans.into_iter().enumerate() {
        if index > 0 {
            line.push_span(Span::raw(" · "));
        }
        line.push_span(span);
    }
    line
}

/// Draws the help over the middle of `frame` in `theme`: every command's keys and what it does.
fn render_help(frame: &mut Frame, theme: &Theme) {
    let keys_width = COMMAND_HELP
        .iter()
        .map(|(_, help)| help)
        .chain(&SEARCH_PROMPT_HELP)
        .map(|help| help.keys.chars().count())
        .max()
        .unwrap_or(0);
    let line = |help: &KeyHelp| {
        Line::from(vec![
            Span::styled(format!("{:keys_width$}  ", help.keys), theme.help_keys),
            Span::raw(help.does),
        ])
    };
    let mut lines: Vec<Line> = COMMAND_HELP.iter().map(|(_, help)| line(help)).collect();
    // No blank line before these: with it, the help wouldn't fit a 24-line terminal.
    lines.extend([
        Line::styled(SELECTION_HELP, theme.help_heading),
        Line::styled("In the search prompt:", theme.help_heading),
    ]);
    lines.extend(SEARCH_PROMPT_HELP.iter().map(line));

    // Two cells of border and two of padding around the text.
    let width = lines.iter().map(Line::width).max().unwrap_or(0) + 4;
    let height = lines.len() + 2;
    let area = frame.area().centered(
        Constraint::Length(u16::try_from(width).unwrap_or(u16::MAX)),
        Constraint::Length(u16::try_from(height).unwrap_or(u16::MAX)),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        // Over the cleared area, the text style paints the theme's background again.
        Paragraph::new(lines).style(theme.text).block(
            Block::bordered()
                .border_style(theme.border)
                .title(" Keys ")
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// The list on screen, the Inbox or the Done list, and what narrows it in every View: the Search, and the Subjects'
/// States that Filters and the Search look at.
#[derive(Clone, Copy)]
struct OnScreen<'a> {
    /// `None` until the Inbox and the Done list arrive.
    list: Option<&'a Inbox>,
    /// `list` is the Done list.
    done: bool,
    search: &'a Search,
    states: &'a States,
}

impl<'a> OnScreen<'a> {
    /// The list on screen of `inbox`: the Done list while `showing_done`, the Inbox otherwise.
    fn new(
        inbox: &'a InboxState,
        showing_done: bool,
        search: &'a Search,
        states: &'a States,
    ) -> Self {
        let list = match inbox {
            InboxState::Loaded(listing) if showing_done => Some(&listing.done),
            InboxState::Loaded(listing) => Some(&listing.inbox),
            InboxState::Loading | InboxState::Failed(_) => None,
        };
        Self {
            list,
            done: showing_done,
            search,
            states,
        }
    }

    /// The Notifications `shown_view` shows, newest first: the ones its Filter and the Search match, only the
    /// unread ones if it shows unread only.
    fn shown(self, shown_view: &ShownView) -> impl Iterator<Item = &'a Notification> {
        let unread_only = self.unread_only(shown_view);
        self.matching(&shown_view.view, self.search)
            .filter(move |n| n.unread || !unread_only)
    }

    /// The Notification at `shown_view`'s cursor; `None` if it shows none.
    fn at_cursor(self, shown_view: &ShownView) -> Option<&'a Notification> {
        self.shown(shown_view).nth(shown_view.cursor.selected()?)
    }

    /// Whether `shown_view` shows unread only: never in the Done list, which GitHub lists all read.
    fn unread_only(self, shown_view: &ShownView) -> bool {
        shown_view.unread_only && !self.done
    }

    /// The Notifications that `view`'s Filter and `search` match, newest first.
    fn matching(self, view: &View, search: &Search) -> impl Iterator<Item = &'a Notification> {
        self.list
            .into_iter()
            .flat_map(Inbox::iter)
            .filter(move |n| {
                let state = self.states.get(n);
                view.filter.matches(n, state) && search.matches(n, state)
            })
    }
}

/// What decides how Notifications' rows look, besides the Notifications themselves.
struct RowLook<'a> {
    theme: &'a Theme,
    kind_labels: KindLabels,
    /// The threads to mark selected.
    selection: &'a HashSet<ThreadId>,
    states: &'a States,
}

/// One row per Notification in `notifications`, as `look` says: selection marker, unread marker, Subject kind (in
/// its State's color, and icon if any), repository, title, Reason, and age relative to `now`.
fn notifications_table<'a>(
    notifications: &[&'a Notification],
    now: DateTime<Utc>,
    look: &RowLook,
) -> Table<'a> {
    let theme = look.theme;
    let rows = notifications.iter().map(|n| {
        let selected = look.selection.contains(&n.thread);
        let subject_state = look.states.get(n);
        // Whether it's read goes over the column's own style, so a theme can, say, grey out read rows; whether it's
        // selected goes over that.
        let mut row_style = if n.unread { theme.unread } else { theme.read };
        if selected {
            row_style = row_style.patch(theme.selected);
        }
        let cell =
            |text: Cow<'a, str>, column: Style| Cell::from(text).style(column.patch(row_style));
        // A State's color goes over the row's style, so that a read Subject still shows it, as the theme tones it.
        let mut kind_style = theme.kind(&n.subject_kind).patch(row_style);
        if let Some(subject_state) = subject_state {
            kind_style = kind_style.patch(theme.state(subject_state));
            if !n.unread {
                kind_style = kind_style.patch(theme.read_state);
            }
        }
        Row::new([
            cell(
                Cow::Borrowed(if selected { "▌" } else { " " }),
                Style::new(),
            ),
            cell(
                Cow::Borrowed(if n.unread { "●" } else { " " }),
                theme.unread_marker,
            ),
            Cell::from(look.kind_labels.label(&n.subject_kind, subject_state)).style(kind_style),
            cell(Cow::Borrowed(&n.repository), theme.repository),
            cell(Cow::Borrowed(&n.subject_title), theme.title),
            cell(Cow::Owned(n.reason.to_string()), theme.reason),
            cell(Cow::Owned(age(n.updated_at, now)), theme.age),
        ])
    });

    let repository_width = notifications
        .iter()
        .map(|n| n.repository.chars().count())
        .max()
        .unwrap_or(0);
    let reason_width = notifications
        .iter()
        .map(|n| n.reason.to_string().len())
        .max()
        .unwrap_or(0);
    let widths = [
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(look.kind_labels.width()),
        Constraint::Length(
            u16::try_from(repository_width)
                .map_or(MAX_REPOSITORY_WIDTH, |w| w.min(MAX_REPOSITORY_WIDTH)),
        ),
        Constraint::Fill(1),
        Constraint::Length(u16::try_from(reason_width).unwrap_or(u16::MAX)),
        Constraint::Length(3),
    ];

    Table::new(rows, widths).row_highlight_style(theme.cursor)
}

/// A label of at most three cells for the Subject column.
/// How the Subject column names the kinds of Subject.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KindLabels {
    /// `PR`, `ISS`, `REL`, …, which any font can draw.
    #[default]
    Text,
    /// GitHub's Octicons, as Nerd Fonts (version 3) have them; a font without them draws boxes.
    Icons,
}

impl KindLabels {
    /// The label for `kind` in `state`, at most [`KindLabels::width`] cells wide. Text labels are the same in any
    /// State: the theme colors it.
    fn label(self, kind: &SubjectKind, state: Option<SubjectState>) -> &'static str {
        // The icons' names in Nerd Fonts: oct-git_pull_request(_draft, _closed), oct-git_merge, oct-issue_opened,
        // oct-issue_closed, oct-skip, oct-tag, oct-comment_discussion, oct-git_commit, oct-checklist and oct-question.
        match (self, kind) {
            (Self::Icons, SubjectKind::PullRequest) => match state {
                Some(SubjectState::Draft) => "\u{f4dd}",
                Some(SubjectState::Merged) => "\u{f419}",
                Some(SubjectState::Closed) => "\u{f4dc}",
                _ => "\u{f407}",
            },
            (Self::Icons, SubjectKind::Issue) => match state {
                Some(SubjectState::Completed) => "\u{f41d}",
                Some(SubjectState::NotPlanned) => "\u{f517}",
                _ => "\u{f41b}",
            },
            (Self::Text, SubjectKind::PullRequest) => "PR",
            (Self::Text, SubjectKind::Issue) => "ISS",
            (Self::Text, SubjectKind::Release) => "REL",
            (Self::Text, SubjectKind::Discussion) => "DIS",
            (Self::Text, SubjectKind::Commit) => "CMT",
            (Self::Text, SubjectKind::CheckSuite) => "CI",
            (Self::Text, SubjectKind::Other(_)) => "?",
            (Self::Icons, SubjectKind::Release) => "\u{f412}",
            (Self::Icons, SubjectKind::Discussion) => "\u{f442}",
            (Self::Icons, SubjectKind::Commit) => "\u{f417}",
            (Self::Icons, SubjectKind::CheckSuite) => "\u{f45e}",
            (Self::Icons, SubjectKind::Other(_)) => "\u{f420}",
        }
    }

    /// The width of the Subject column, in terminal cells.
    fn width(self) -> u16 {
        match self {
            Self::Text => 3,
            Self::Icons => 1,
        }
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
    use std::collections::BTreeMap;

    use anyhow::anyhow;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};

    use super::*;
    use crate::files::{ChangedFile, FileStatus, FileText};
    use crate::filter::Filter;
    use crate::inbox::{ApiPaths, Reason, ThreadId};
    use crate::state::Known;

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

    /// "Add polling" (thread 1, unread review request, just now), then "Old news" (thread 2, read, two days ago).
    fn notifications() -> Vec<Notification> {
        vec![
            Notification {
                thread: ThreadId(1),
                subject_url: "https://github.com/wcgw/ghd/pull/1".to_owned(),
                repository: "wcgw/ghd".to_owned(),
                subject_kind: SubjectKind::PullRequest,
                subject_title: "Add polling".to_owned(),
                reason: Reason::ReviewRequested,
                unread: true,
                updated_at: Utc::now(),
                api_paths: ApiPaths::default(),
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
                api_paths: ApiPaths::default(),
            },
        ]
    }

    /// A fetch that has just brought `notifications`.
    fn fetched(notifications: Vec<Notification>) -> Fetched {
        Fetched {
            began: Instant::now(),
            listing: Ok(Some(Listing {
                inbox: notifications.into_iter().collect(),
                done: Inbox::default(),
            })),
        }
    }

    /// A fetch that has just failed.
    fn fetch_failed() -> Fetched {
        Fetched {
            began: Instant::now(),
            listing: Err(anyhow!("timed out")),
        }
    }

    fn inbox_view_only() -> App {
        App::new(
            vec![View {
                name: "Inbox".to_owned(),
                filter: Filter::default(),
            }],
            Theme::default(),
            KindLabels::Text,
        )
    }

    /// The app with three Views: Inbox, "Reviews" (`reason:review-requested`) and "Read" (`is:read`), the Inbox
    /// View on screen, and [`notifications`] fetched; no styles.
    fn app() -> App {
        app_in(Theme::default())
    }

    /// [`app`], in `theme`.
    fn app_in(theme: Theme) -> App {
        let view = |name: &str, filter: &str| View {
            name: name.to_owned(),
            filter: filter.parse().expect("valid filter"),
        };
        let mut app = App::new(
            vec![
                View {
                    name: "Inbox".to_owned(),
                    filter: Filter::default(),
                },
                view("Reviews", "reason:review-requested"),
                view("Read", "is:read"),
            ],
            theme,
            KindLabels::Text,
        );
        app.fetched(fetched(notifications()));
        app
    }

    /// The titles of the rows `lines` shows, top to bottom.
    fn titles(lines: &[String]) -> Vec<&str> {
        lines[1..4]
            .iter()
            .filter_map(|line| {
                ["Brand new", "Add polling", "Old news"]
                    .into_iter()
                    .find(|title| line.contains(title))
            })
            .collect()
    }

    /// The lines `app` draws on a 5-line screen, with no fetch under way.
    fn screen(app: &mut App) -> Vec<String> {
        screen_while(app, false)
    }

    /// The lines `app` draws on a 5-line screen, saying a fetch is under way if `fetching`.
    fn screen_while(app: &mut App, fetching: bool) -> Vec<String> {
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, 5)).expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, fetching))
            .expect("test backend never fails");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(SCREEN_WIDTH.into())
            .map(|line| line.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect()
    }

    fn cursor_title(app: &App) -> Option<&str> {
        app.at_cursor().map(|n| n.subject_title.as_str())
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
        assert_eq!(press(KeyCode::Char('j'), none), Some(Command::Down));
        assert_eq!(press(KeyCode::Up, none), Some(Command::Up));
        assert_eq!(press(KeyCode::Char('g'), none), Some(Command::First));
        assert_eq!(
            press(KeyCode::Char('G'), KeyModifiers::SHIFT),
            Some(Command::Last)
        );
        // Vim's `:q` quits, not `q`.
        assert_eq!(press(KeyCode::Char('q'), none), None);
        assert_eq!(
            press(KeyCode::Char(':'), KeyModifiers::SHIFT),
            Some(Command::OpenCommandLine)
        );
        assert_eq!(press(KeyCode::Char('r'), none), Some(Command::Refresh));
        assert_eq!(press(KeyCode::Char('/'), none), Some(Command::Search));
        assert_eq!(press(KeyCode::Esc, none), Some(Command::Clear));
        assert_eq!(press(KeyCode::Char('?'), none), Some(Command::Help));
        assert_eq!(
            press(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(Command::HalfPageDown)
        );
        assert_eq!(
            press(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Some(Command::HalfPageUp)
        );
        assert_eq!(press(KeyCode::Char('d'), none), None);
        assert_eq!(
            press(KeyCode::Char('D'), KeyModifiers::SHIFT),
            Some(Command::ToggleDone)
        );
        assert_eq!(
            press(KeyCode::Char(' '), none),
            Some(Command::ToggleSelected)
        );
        assert_eq!(
            press(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Some(Command::SelectAllShown)
        );
        assert_eq!(
            press(KeyCode::Char('u'), none),
            Some(Command::ToggleUnreadOnly)
        );
        assert_eq!(
            press(KeyCode::Char('p'), none),
            Some(Command::TogglePreview)
        );
        assert_eq!(press(KeyCode::Char('f'), none), Some(Command::OpenFiles));
        assert_eq!(
            press(KeyCode::Char('w'), none),
            Some(Command::ToggleWholeFiles)
        );
        assert_eq!(
            press(KeyCode::Char('J'), KeyModifiers::SHIFT),
            Some(Command::ScrollPreviewDown)
        );
        assert_eq!(
            press(KeyCode::Char('K'), KeyModifiers::SHIFT),
            Some(Command::ScrollPreviewUp)
        );
        assert_eq!(
            press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Command::Quit)
        );
        assert_eq!(press(KeyCode::Char('c'), none), None);
        assert_eq!(press(KeyCode::Tab, none), Some(Command::NextView));
        assert_eq!(
            press(KeyCode::BackTab, KeyModifiers::SHIFT),
            Some(Command::PreviousView)
        );
        assert_eq!(press(KeyCode::Char('1'), none), Some(Command::ShowView(0)));
        assert_eq!(press(KeyCode::Char('9'), none), Some(Command::ShowView(8)));
        assert_eq!(press(KeyCode::Char('0'), none), None);
    }

    #[test]
    fn done_leaves_at_once_and_comes_back_if_refused() {
        let mut app = app();

        let change = app
            .execute(Command::MarkDone)
            .pop()
            .expect("the cursor is on a Notification");

        assert!(matches!(&change, Change::MarkDone(n) if n.thread == ThreadId(1)));
        assert_eq!(
            cursor_title(&app),
            Some("Old news"),
            "the cursor moves to the next one"
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
            .pop()
            .expect("the cursor is on a Notification");

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
            .pop()
            .expect("the cursor's Notification is unread");
        assert!(matches!(&change, Change::MarkRead(n) if n.thread == ThreadId(1)));
        assert!(screen(&mut app)[0].contains("Inbox (0)"));
        assert!(app.execute(Command::MarkRead).is_empty(), "already read");

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
    fn views_show_what_matches_and_count_its_unread() {
        let mut app = app();

        let lines = screen(&mut app);
        assert!(
            lines[0].contains(" Inbox (1) │ Reviews (1) │ Read (0) "),
            "{lines:#?}"
        );
        assert_eq!(titles(&lines), ["Add polling", "Old news"]);

        app.execute(Command::ShowView(2));
        assert_eq!(titles(&screen(&mut app)), ["Old news"]);

        app.execute(Command::NextView);
        assert_eq!(
            titles(&screen(&mut app)),
            ["Add polling", "Old news"],
            "wraps to the Inbox View"
        );
        app.execute(Command::PreviousView);
        assert_eq!(
            titles(&screen(&mut app)),
            ["Old news"],
            "wraps back to the last View"
        );
        app.execute(Command::ShowView(1));
        assert_eq!(titles(&screen(&mut app)), ["Add polling"]);
        app.execute(Command::ShowView(8));
        assert_eq!(
            titles(&screen(&mut app)),
            ["Add polling"],
            "no ninth View: stays put"
        );
    }

    /// [`notifications`], with the REST API paths of their Subjects: each an issue numbered as its thread.
    fn previewable(notifications: Vec<Notification>) -> Vec<Notification> {
        notifications
            .into_iter()
            .map(|n| Notification {
                api_paths: ApiPaths {
                    subject: Some(format!("/repos/{}/issues/{}", n.repository, n.thread.0)),
                    latest_comment: None,
                },
                ..n
            })
            .collect()
    }

    /// The lines `app` draws on a screen `height` lines tall.
    fn screen_of(app: &mut App, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, height))
            .expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(SCREEN_WIDTH.into())
            .map(|line| line.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect()
    }

    /// Height of the screen the Preview tests draw on.
    const PREVIEW_SCREEN_HEIGHT: u16 = 20;

    /// [`app`], its Notifications previewable, drawn on a screen tall enough for the Preview, and the Preview open
    /// with the cursor on "Add polling" since the moment returned.
    fn app_previewing() -> (App, Instant) {
        let mut app = app();
        app.fetched(fetched(previewable(notifications())));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("p"));
        let since = Instant::now();
        app.note_cursor(since);
        (app, since)
    }

    /// The Preview of "Add polling", with `body` and no comments.
    fn add_polling_preview(body: Body) -> Preview {
        Preview {
            reference: "#1".to_owned(),
            title: "Add polling".to_owned(),
            state: Some(SubjectState::Open),
            author: Some("octocat".to_owned()),
            labels: vec!["ui".to_owned()],
            closed: None,
            pull_request: None,
            discussion: None,
            body,
            comments: Vec::new(),
            opens_on: Place::Body,
            note: None,
        }
    }

    /// A comment by @hubot, `ago`, saying `body`.
    fn comment_by_hubot(body: &str, ago: TimeDelta) -> crate::preview::Comment {
        crate::preview::Comment {
            author: Some("hubot".to_owned()),
            published_at: Utc::now() - ago,
            path: None,
            body: body.to_owned(),
            mark: None,
        }
    }

    /// The Preview of "Add polling", with no description, opening on `comment`.
    fn add_polling_comment(comment: crate::preview::Comment) -> Preview {
        Preview {
            comments: vec![comment],
            opens_on: Place::Comment(0),
            ..add_polling_preview(Body::Markdown(String::new()))
        }
    }

    /// Fetches the Preview due at `now`, with `preview` as its outcome.
    fn fetch_preview_at(app: &mut App, now: Instant, preview: anyhow::Result<Preview>) {
        let wanted = app.preview_to_fetch(now).expect("a Preview is due");
        app.preview_fetched(PreviewFetched {
            thread: wanted.thread,
            as_of: wanted.as_of,
            preview,
        });
    }

    fn shows(lines: &[String], text: &str) -> bool {
        lines.iter().any(|line| line.contains(text))
    }

    #[test]
    fn p_opens_the_preview_below_the_list() {
        let (mut app, since) = app_previewing();

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let pane_top = lines
            .iter()
            .position(|line| line.contains(" Preview "))
            .expect("the Preview is open");
        // A line for the status, 19 for the screen: the Preview takes three quarters of them.
        assert_eq!(pane_top, 5, "{lines:#?}");
        assert!(shows(&lines, "Fetching…"), "{lines:#?}");
        assert!(
            shows(&lines, "wcgw/ghd · review-requested · now"),
            "{lines:#?}"
        );

        let body = Body::Markdown("Polls the Inbox.".to_owned());
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(body)),
        );
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "#1 Add polling"), "{lines:#?}");
        assert!(
            shows(&lines, "wcgw/ghd · open · @octocat · ui"),
            "{lines:#?}"
        );
        assert!(shows(&lines, "Polls the Inbox."), "{lines:#?}");

        app.press(key_named("p"));
        assert!(!shows(
            &screen_of(&mut app, PREVIEW_SCREEN_HEIGHT),
            " Preview "
        ));
    }

    #[test]
    fn preview_waits_for_the_cursor_to_stay() {
        let (mut app, since) = app_previewing();
        let moved = since + PREVIEW_REST / 2;

        assert!(app.preview_to_fetch(moved).is_none(), "not yet");
        app.execute(Command::Down);
        app.note_cursor(moved);
        assert!(
            app.preview_to_fetch(since + PREVIEW_REST).is_none(),
            "the cursor moved on"
        );

        let wanted = app
            .preview_to_fetch(moved + PREVIEW_REST)
            .expect("the cursor stayed");
        assert_eq!(wanted.thread, ThreadId(2));
        assert!(app.preview_due().is_none(), "being fetched");
    }

    #[test]
    fn closed_preview_fetches_nothing() {
        let mut app = app();
        app.fetched(fetched(previewable(notifications())));
        let now = Instant::now();

        app.note_cursor(now);

        assert!(app.preview_due().is_none());
        assert!(app.preview_to_fetch(now + PREVIEW_REST).is_none());
    }

    #[test]
    fn newer_activity_fetches_the_preview_again() {
        let (mut app, since) = app_previewing();
        let body = Body::Markdown(String::new());
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(body)),
        );
        assert!(app.preview_due().is_none(), "fetched");

        let mut newer = previewable(notifications());
        newer[0].updated_at += TimeDelta::minutes(5);
        app.fetched(fetched(newer));

        assert!(app.preview_due().is_some());
    }

    #[test]
    fn failed_preview_is_tried_again_when_the_cursor_comes_back() {
        let (mut app, since) = app_previewing();
        fetch_preview_at(&mut app, since + PREVIEW_REST, Err(anyhow!("timed out")));
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            shows(&lines, "Couldn't fetch the Preview: timed out"),
            "{lines:#?}"
        );
        assert!(app.preview_due().is_none(), "not again and again");

        let back = since + PREVIEW_REST * 2;
        app.execute(Command::Down);
        app.note_cursor(back);
        app.execute(Command::Up);
        app.note_cursor(back);

        assert_eq!(
            app.preview_due().map(|(due, wanted)| (due, wanted.thread)),
            Some((back + PREVIEW_REST, ThreadId(1)))
        );
    }

    #[test]
    fn r_fetches_the_preview_on_screen_again() {
        let (mut app, since) = app_previewing();
        let body = Body::Markdown(String::new());
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(body)),
        );

        assert!(matches!(app.press(key_named("r")), Some(Outcome::Refresh)));

        assert!(app.preview_due().is_some());
    }

    #[test]
    fn shift_j_and_k_scroll_the_preview_half_its_height() {
        let (mut app, since) = app_previewing();
        let body: Vec<String> = (0..40).map(|i| format!("Line {i}")).collect();
        let shown = Body::Markdown(body.join("\n"));
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(shown)),
        );
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let pane = |app: &App| {
            let pane = app.preview.as_ref().expect("open");
            (pane.scroll.by, pane.scroll.rows, pane.scroll.lines)
        };
        let (_, rows, lines) = pane(&app);
        // The header's three lines, then the body's forty.
        assert_eq!(lines, 43);

        app.press(key_named("J"));
        assert_eq!(pane(&app).0, rows / 2);
        let screen = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let pane_top = screen
            .iter()
            .position(|line| line.contains(" Preview "))
            .expect("open");
        assert!(
            screen[pane_top + 1].contains(&format!("Line {}", rows / 2 - 3)),
            "{screen:#?}"
        );

        for _ in 0..20 {
            app.press(key_named("J"));
        }
        assert_eq!(pane(&app).0, lines - rows, "no further than the last line");
        app.press(key_named("K"));
        assert_eq!(pane(&app).0, lines - rows - rows / 2);

        app.execute(Command::Down);
        app.note_cursor(since);
        assert_eq!(pane(&app).0, 0, "the next Notification's from its top");
    }

    #[test]
    fn moving_on_from_the_last_row_keeps_the_preview_where_it_is() {
        let (mut app, since) = app_previewing();
        app.press(key_named("G"));
        app.note_cursor(since);
        let body: Vec<String> = (0..40).map(|i| format!("Line {i}")).collect();
        let shown = Body::Markdown(body.join("\n"));
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(shown)),
        );
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("J"));
        let by = |app: &App| app.preview.as_ref().expect("open").scroll.by;
        let scrolled = by(&app);
        assert!(scrolled > 0);

        // Not drawn in between: `run` notes the cursor before it draws.
        for key in ["j", "Ctrl-D", "Space"] {
            app.press(key_named(key));
            app.note_cursor(since);
            assert_eq!(by(&app), scrolled, "{key} on the last row");
        }
    }

    /// The Preview of "Add polling" with its description and two long comments, opening on the first.
    fn add_polling_discussed() -> Preview {
        let long = |first: &str| {
            let lines: Vec<String> = (0..40).map(|i| format!("Line {i}")).collect();
            format!("{first}\n\n{}", lines.join("\n\n"))
        };
        Preview {
            comments: vec![
                comment_by_hubot(&long("First"), TimeDelta::hours(2)),
                comment_by_hubot(&long("Second"), TimeDelta::hours(1)),
            ],
            opens_on: Place::Comment(0),
            ..add_polling_preview(Body::Markdown("The description".to_owned()))
        }
    }

    #[test]
    fn shift_j_and_k_step_through_the_comments_once_scrolled_to_their_end() {
        let (mut app, since) = app_previewing();
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(add_polling_discussed()));
        // The Preview's title, how far down it's scrolled, and the screen, as drawn.
        let look = |app: &mut App| {
            let lines = screen_of(app, PREVIEW_SCREEN_HEIGHT);
            let title = lines
                .iter()
                .find(|line| line.contains(" Preview "))
                .expect("open")
                .clone();
            (title, app.preview.as_ref().expect("open").scroll.by, lines)
        };
        // Drawn after each key, as the app does.
        let press = |app: &mut App, key: &str| {
            app.press(key_named(key));
            look(app)
        };
        let (title, _, _) = look(&mut app);
        assert!(title.contains(" Preview · comment 1 of 2 "), "{title}");

        let (title, by, _) = press(&mut app, "J");
        assert!(by > 0 && title.contains("comment 1 of 2"), "scrolls first");
        let (title, by, lines) = (0..20)
            .map(|_| press(&mut app, "J"))
            .find(|(title, _, _)| title.contains("comment 2 of 2"))
            .expect("steps to the next comment");
        assert!(title.contains(" Preview · comment 2 of 2 "));
        assert_eq!(by, 0, "from its top");
        assert!(shows(&lines, "Second"), "{lines:#?}");
        for _ in 0..20 {
            press(&mut app, "J");
        }
        let (title, by, _) = press(&mut app, "J");
        assert!(
            title.contains("comment 2 of 2") && by > 0,
            "the last, scrolled to its end"
        );

        let (title, by, _) = press(&mut app, "K");
        assert!(
            title.contains("comment 2 of 2") && by > 0,
            "scrolls back up first"
        );
        let (_, by, lines) = (0..20)
            .map(|_| press(&mut app, "K"))
            .find(|(title, _, _)| title.contains("comment 1 of 2"))
            .expect("steps to the previous comment");
        assert_eq!(by, 0, "from its top");
        assert!(shows(&lines, "First"), "{lines:#?}");
        let (title, _, lines) = press(&mut app, "K");
        assert!(title.contains(" Preview · 2 comments "), "{title}");
        assert!(shows(&lines, "The description"), "{lines:#?}");
        let (_, _, lines) = press(&mut app, "K");
        assert!(shows(&lines, "The description"), "the first place");

        app.execute(Command::Down);
        app.note_cursor(since);
        app.execute(Command::Up);
        app.note_cursor(since);
        let (title, _, _) = look(&mut app);
        assert!(title.contains("comment 1 of 2"), "back where it opens");
    }

    #[test]
    fn newer_activity_opens_the_preview_where_it_opens() {
        let (mut app, since) = app_previewing();
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(add_polling_discussed()));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("K"));

        let mut newer = previewable(notifications());
        newer[0].updated_at += TimeDelta::minutes(5);
        app.fetched(fetched(newer));
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(add_polling_discussed()));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            lines
                .iter()
                .any(|line| line.contains(" Preview · comment 1 of 2 ")),
            "{lines:#?}"
        );
    }

    #[test]
    fn r_takes_the_preview_back_where_it_opens() {
        let (mut app, since) = app_previewing();
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(add_polling_discussed()));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("K"));
        assert!(app.preview.as_ref().expect("open").stepped.is_some());

        app.press(key_named("r"));

        assert_eq!(app.preview.as_ref().expect("open").stepped, None);
    }

    #[test]
    fn description_mentioning_you_says_so_on_it() {
        let (mut app, since) = app_previewing();
        let preview = Preview {
            note: Some(Note::BodyMentionsYou),
            ..add_polling_discussed()
        };
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(preview));
        let said = "@octocat mentioned you in the description";
        assert!(!shows(&screen_of(&mut app, PREVIEW_SCREEN_HEIGHT), said));

        app.press(key_named("K"));

        assert!(shows(&screen_of(&mut app, PREVIEW_SCREEN_HEIGHT), said));
    }

    #[test]
    fn voight_assesses_the_comment_on_screen() {
        let (mut app, since) = app_previewing();
        app.assessments = Some(Assessments::default());
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(add_polling_discussed()));
        app.voight_said(assessment::Event::Loaded { fallback: None });
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let (key, _) = app.assessment_to_start().expect("ready");
        assert_eq!(key.part, assessment::Part::Preview(Place::Comment(0)));
        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(None),
        });

        app.press(key_named("K"));
        let (key, text) = app.assessment_to_start().expect("another text");

        assert_eq!(key.part, assessment::Part::Preview(Place::Body));
        assert_eq!(text, "The description");
    }

    /// The right border of `lines`, from the line under `top` to the line over `bottom`: where a scrollbar goes.
    fn right_border(lines: &[String], top: usize, bottom: usize) -> String {
        lines[top + 1..bottom]
            .iter()
            .map(|line| line.chars().last().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn longer_preview_has_a_scrollbar_over_what_shows() {
        let (mut app, since) = app_previewing();
        app.theme = Theme {
            scrollbar: Style::new().fg(Color::Magenta),
            ..Theme::default()
        };
        let body: Vec<String> = (0..40).map(|i| format!("Line {i}")).collect();
        let long = add_polling_preview(Body::Markdown(body.join("\n")));
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(long));
        let scrollbar = |app: &mut App| {
            let lines = screen_of(app, PREVIEW_SCREEN_HEIGHT);
            let top = lines
                .iter()
                .position(|line| line.contains(" Preview "))
                .expect("open");
            let bottom = usize::from(PREVIEW_SCREEN_HEIGHT) - 2;
            assert!(
                lines[top].ends_with('┐') && lines[bottom].ends_with('┘'),
                "between the corners: {lines:#?}"
            );
            right_border(&lines, top, bottom)
        };

        let at_top = scrollbar(&mut app);
        let thumb = at_top.chars().filter(|&c| c == '┃').count();
        assert!(
            at_top.starts_with('┃') && at_top.ends_with('│') && thumb < at_top.chars().count() / 2,
            "{at_top}"
        );
        assert_eq!(cell_of(&mut app, "┃").fg, Color::Magenta);
        for _ in 0..20 {
            app.press(key_named("J"));
            screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        }
        let at_end = scrollbar(&mut app);
        assert!(at_end.starts_with('│') && at_end.ends_with('┃'), "{at_end}");
        assert_eq!(at_end.chars().filter(|&c| c == '┃').count(), thumb);
    }

    /// The row of the cursor in the View on screen.
    fn cursor_row(app: &App) -> Option<usize> {
        app.views[app.current].cursor.selected()
    }

    #[test]
    fn wheel_scrolls_the_preview_while_it_shows() {
        let (mut app, since) = app_previewing();
        let body: Vec<String> = (0..40).map(|i| format!("Line {i}")).collect();
        let long = add_polling_preview(Body::Markdown(body.join("\n")));
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(long));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let row = cursor_row(&app);
        let by = |app: &App| app.preview.as_ref().expect("open").scroll.by;

        app.wheel(Wheel::Down);
        app.wheel(Wheel::Down);
        assert_eq!(by(&app), 2 * WHEEL_LINES);
        assert_eq!(cursor_row(&app), row, "the list stays");
        app.wheel(Wheel::Up);
        assert_eq!(by(&app), WHEEL_LINES);
    }

    #[test]
    fn only_the_mouse_wheel_does_anything() {
        let mut app = app();
        app.fetched(fetched(notifications()));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let row = cursor_row(&app).expect("on the first");
        let mouse = |kind| MouseEvent {
            kind,
            column: 10,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };

        app.mouse(mouse(MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        )));
        app.mouse(mouse(MouseEventKind::ScrollRight));
        assert_eq!(cursor_row(&app), Some(row));
        app.mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(cursor_row(&app), Some(row + 1));
        app.mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(cursor_row(&app), Some(row));
    }

    #[test]
    fn wheel_moves_the_cursor_without_the_preview() {
        let mut app = app();
        app.fetched(fetched(notifications()));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let row = cursor_row(&app).expect("on the first");

        app.wheel(Wheel::Down);
        assert_eq!(cursor_row(&app), Some(row + 1));
        app.wheel(Wheel::Up);
        assert_eq!(cursor_row(&app), Some(row));

        app.press(key_named("p"));
        screen_of(&mut app, PREVIEW_MIN_HEIGHT - 1);
        app.wheel(Wheel::Down);
        assert_eq!(
            cursor_row(&app),
            Some(row + 1),
            "the Preview doesn't show on a terminal this short"
        );

        app.press(key_named("?"));
        app.wheel(Wheel::Down);
        assert_eq!(cursor_row(&app), Some(row + 1), "nothing over the help");
        assert!(app.help_shown);
    }

    #[test]
    fn preview_that_fits_has_no_scrollbar() {
        let lines = previewing(add_polling_preview(Body::Markdown("Short".to_owned())));

        assert!(!shows(&lines, "┃"), "{lines:#?}");
    }

    #[test]
    fn comment_shows_who_wrote_it_and_where() {
        let (mut app, since) = app_previewing();
        let comment = crate::preview::Comment {
            path: Some("src/ui.rs".to_owned()),
            ..comment_by_hubot("Typo here", TimeDelta::hours(2))
        };
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_comment(comment)),
        );

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        assert!(
            shows(&lines, "@hubot commented on src/ui.rs 2h ago"),
            "{lines:#?}"
        );
        assert!(shows(&lines, "Typo here"), "{lines:#?}");
    }

    /// The lines `app` draws once the Preview at the cursor is fetched as `preview`.
    fn previewing(preview: Preview) -> Vec<String> {
        let (mut app, since) = app_previewing();
        fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(preview));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT)
    }

    #[test]
    fn pull_request_preview_shows_its_changes_and_checks() {
        let body = Body::Markdown("What it does".to_owned());
        let lines = previewing(Preview {
            state: Some(SubjectState::Merged),
            closed: Some(crate::preview::Closed {
                by: Some("alxs".to_owned()),
                at: Utc::now() - TimeDelta::hours(2),
                merged: true,
            }),
            pull_request: Some(PullRequestSummary {
                additions: 120,
                deletions: 30,
                changed_files: 7,
                commits: 1,
                ci: Some(Ci::Passing),
                review: Some(ReviewDecision::Approved),
            }),
            ..add_polling_preview(body)
        });

        assert!(shows(&lines, "Merged by @alxs 2h ago"), "{lines:#?}");
        assert!(
            shows(
                &lines,
                "+120 \u{2212}30 · 7 files · 1 commit · CI passing · approved"
            ),
            "{lines:#?}"
        );
    }

    #[test]
    fn closed_issue_says_why() {
        let lines = previewing(Preview {
            state: Some(SubjectState::NotPlanned),
            closed: Some(crate::preview::Closed {
                by: None,
                at: Utc::now() - TimeDelta::days(3),
                merged: false,
            }),
            ..add_polling_preview(Body::Markdown("Body".to_owned()))
        });

        assert!(shows(&lines, "Closed as not planned 3d ago"), "{lines:#?}");
    }

    #[test]
    fn mentions_say_where_you_were_mentioned() {
        let mentioned = crate::preview::Comment {
            mark: Some(Mark::MentionsYou),
            ..comment_by_hubot("@alxs look", TimeDelta::hours(2))
        };
        let lines = previewing(add_polling_comment(mentioned));
        assert!(shows(&lines, "@hubot mentioned you 2h ago"), "{lines:#?}");

        let lines = previewing(Preview {
            note: Some(Note::BodyMentionsYou),
            ..add_polling_preview(Body::Markdown("cc @alxs".to_owned()))
        });
        assert!(
            shows(&lines, "@octocat mentioned you in the description"),
            "{lines:#?}"
        );

        let lines = previewing(Preview {
            note: Some(Note::MentionNotLookedFor("rate limited".to_owned())),
            ..add_polling_preview(Body::Markdown(String::new()))
        });
        assert!(
            shows(&lines, "Couldn't look for the mention: rate limited"),
            "{lines:#?}"
        );
    }

    #[test]
    fn discussion_preview_shows_its_category_and_answer() {
        let answer = crate::preview::Comment {
            author: Some("alxs".to_owned()),
            mark: Some(Mark::Answer),
            ..comment_by_hubot("Like so", TimeDelta::days(3))
        };
        let lines = previewing(Preview {
            state: None,
            discussion: Some(crate::preview::DiscussionSummary {
                category: "Q&A".to_owned(),
                answered: true,
            }),
            ..add_polling_comment(answer)
        });

        assert!(
            shows(&lines, "wcgw/ghd · Q&A · answered · @octocat · ui"),
            "{lines:#?}"
        );
        assert!(shows(&lines, "@alxs answered 3d ago"), "{lines:#?}");
    }

    /// [`app_previewing`] with voight on, its model loading, and the Preview of "Add polling" showing `body`
    /// fetched.
    fn app_assessing(body: &str) -> App {
        let (mut app, since) = app_previewing();
        app.assessments = Some(Assessments::default());
        let shown = Body::Markdown(body.to_owned());
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(shown)),
        );
        app
    }

    /// The Preview's bottom border, on the line above the status line.
    fn preview_border(lines: &[String]) -> &str {
        &lines[usize::from(PREVIEW_SCREEN_HEIGHT) - 2]
    }

    /// An Assessment of `share`, its windows left out.
    fn assessed_at(share: f64) -> Assessment {
        Assessment {
            share,
            windows: Vec::new(),
        }
    }

    /// The window over `part` of `text`, which an LLM wrote `share` of.
    fn window_over(text: &str, part: &str, share: f64) -> Window {
        let start = text.find(part).expect("part of the text");
        Window {
            range: start..start + part.len(),
            share,
        }
    }

    /// A theme whose Heatmap is green for text an LLM wrote none of, red for all, and has no color between.
    fn heatmap_theme() -> Theme {
        Theme {
            heatmap_human: Style::new().bg(Color::Green),
            heatmap_llm: Style::new().bg(Color::Red),
            ..Theme::default()
        }
    }

    /// The background of the cell `text` starts at, on the screen `app` draws for the Preview tests.
    fn background_of(app: &mut App, text: &str) -> Color {
        cell_of(app, text).bg
    }

    /// The last cell inside the border on the line `text` is on, on the screen `app` draws for the Preview tests.
    fn line_end_of(app: &mut App, text: &str) -> ratatui::buffer::Cell {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, PREVIEW_SCREEN_HEIGHT))
            .expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");
        let buffer = terminal.backend().buffer();
        let y = (0..PREVIEW_SCREEN_HEIGHT)
            .find(|&y| {
                (0..SCREEN_WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains(text)
            })
            .unwrap_or_else(|| panic!("{text:?} isn't on screen"));
        buffer[(SCREEN_WIDTH - 2, y)].clone()
    }

    /// The cell `text` starts at, on the screen `app` draws for the Preview tests.
    fn cell_of(app: &mut App, text: &str) -> ratatui::buffer::Cell {
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, PREVIEW_SCREEN_HEIGHT))
            .expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");
        let buffer = terminal.backend().buffer();
        let y = (0..PREVIEW_SCREEN_HEIGHT)
            .find(|&y| {
                (0..SCREEN_WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains(text)
            })
            .unwrap_or_else(|| panic!("{text:?} isn't on screen"));
        cell_at(buffer, y, text).clone()
    }

    #[test]
    fn h_shows_the_heatmap_of_the_preview_once_assessed() {
        let body = "Polls the Inbox.\n\nAnd says so.";
        let mut app = app_assessing(body);
        app.theme = heatmap_theme();
        app.voight_said(assessment::Event::Loaded { fallback: None });
        let (key, text) = app.assessment_to_start().expect("ready");

        assert!(app.press(key_named("H")).is_none());
        assert!(
            matches!(&app.status, Some(StatusMessage::Info(said)) if said == "The Heatmap shows on text voight has assessed"),
            "{:?}",
            app.status
        );
        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(Some(Assessment {
                share: 0.9,
                windows: vec![
                    window_over(&text, "Polls the Inbox.", 1.0),
                    window_over(&text, "And says so.", 0.0),
                ],
            })),
        });

        assert_eq!(background_of(&mut app, "Polls the Inbox."), Color::Red);
        assert_eq!(background_of(&mut app, "And says so."), Color::Green);
        assert_eq!(
            background_of(&mut app, "voight 90%"),
            Color::Red,
            "the whole's color"
        );
        assert_eq!(
            background_of(&mut app, "Add polling"),
            Color::Reset,
            "the header isn't assessed"
        );

        app.press(key_named("H"));

        assert_eq!(background_of(&mut app, "Polls the Inbox."), Color::Reset);
        assert_eq!(background_of(&mut app, "voight 90%"), Color::Reset);
    }

    #[test]
    fn heatmap_shades_comments_and_plain_bodies() {
        let comment = add_polling_comment(comment_by_hubot("Polls the Inbox.", TimeDelta::zero()));
        let plain = add_polling_preview(Body::Plain("Polls the Inbox.".to_owned()));
        for preview in [comment, plain] {
            let (mut app, since) = app_previewing();
            app.theme = heatmap_theme();
            app.assessments = Some(Assessments::default());
            fetch_preview_at(&mut app, since + PREVIEW_REST, Ok(preview));
            app.voight_said(assessment::Event::Loaded { fallback: None });
            let (key, text) = app.assessment_to_start().expect("ready");
            app.voight_said(assessment::Event::Assessed {
                key,
                assessment: Ok(Some(Assessment {
                    share: 1.0,
                    windows: vec![window_over(&text, "Polls the Inbox.", 1.0)],
                })),
            });

            app.press(key_named("H"));

            assert_eq!(background_of(&mut app, "Polls the Inbox."), Color::Red);
        }
    }

    #[test]
    fn heatmap_needs_voight() {
        let (mut app, _) = app_previewing();

        app.press(key_named("H"));

        assert!(!app.heatmap);
        assert!(
            matches!(&app.status, Some(StatusMessage::Info(said)) if said == "No Heatmap: voight is off"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn assessment_shows_at_the_end_of_the_preview_border() {
        let mut app = app_assessing("Polls the Inbox.");
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            preview_border(&lines).ends_with(" voight loading… ┘"),
            "{lines:#?}"
        );
        assert!(app.assessment_to_start().is_none(), "not loaded yet");

        app.voight_said(assessment::Event::Loaded { fallback: None });
        let (key, text) = app.assessment_to_start().expect("ready");
        assert_eq!(
            (key.thread, text.as_str()),
            (ThreadId(1), "Polls the Inbox.")
        );
        assert!(app.assessment_to_start().is_none(), "one at a time");
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            preview_border(&lines).ends_with(" voight … ┘"),
            "{lines:#?}"
        );

        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(Some(assessed_at(0.734))),
        });
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            preview_border(&lines).ends_with(" voight 73% ┘"),
            "{lines:#?}"
        );
    }

    #[test]
    fn too_little_prose_shows_no_assessment() {
        let mut app = app_assessing("LGTM");
        app.voight_said(assessment::Event::Loaded { fallback: None });
        let (key, _) = app.assessment_to_start().expect("ready");

        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(None),
        });

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(!preview_border(&lines).contains("voight"), "{lines:#?}");
        assert!(app.assessment_to_start().is_none(), "assessed already");
    }

    #[test]
    fn template_comments_are_not_assessed() {
        let mut app = app_assessing("<!-- Describe your change -->\n\nIt polls.");
        app.voight_said(assessment::Event::Loaded { fallback: None });

        let (_, text) = app.assessment_to_start().expect("ready");

        assert!(!text.contains("Describe"), "{text:?}");
        assert!(text.contains("It polls."), "{text:?}");
    }

    #[test]
    fn voight_failing_to_load_says_why() {
        let mut app = app_assessing("Polls the Inbox.");

        app.voight_said(assessment::Event::Failed("out of memory".to_owned()));

        assert!(app.assessment_to_start().is_none());
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(!preview_border(&lines).contains("voight"), "{lines:#?}");
        assert!(
            lines[usize::from(PREVIEW_SCREEN_HEIGHT) - 1]
                .starts_with("voight couldn't load its model: out of memory"),
            "{lines:#?}"
        );
    }

    #[test]
    fn falling_back_to_the_cpu_says_so() {
        let mut app = app_assessing("Polls the Inbox.");

        app.voight_said(assessment::Event::Loaded {
            fallback: Some("couldn't use CUDA, so voight runs on the CPU".to_owned()),
        });

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            lines[usize::from(PREVIEW_SCREEN_HEIGHT) - 1]
                .starts_with("couldn't use CUDA, so voight runs on the CPU"),
            "{lines:#?}"
        );
        assert!(app.assessment_to_start().is_some(), "assesses all the same");
    }

    #[test]
    fn r_assesses_the_preview_again() {
        let mut app = app_assessing("Polls the Inbox.");
        app.voight_said(assessment::Event::Loaded { fallback: None });
        let (key, _) = app.assessment_to_start().expect("ready");
        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(Some(assessed_at(0.2))),
        });

        app.press(key_named("r"));
        let shown = Body::Markdown("Edited".to_owned());
        fetch_preview_at(
            &mut app,
            Instant::now() + PREVIEW_REST,
            Ok(add_polling_preview(shown)),
        );

        let (_, text) = app.assessment_to_start().expect("assessed again");
        assert_eq!(text, "Edited");
    }

    #[test]
    fn voight_off_assesses_nothing() {
        let (mut app, since) = app_previewing();
        let shown = Body::Markdown("Polls the Inbox.".to_owned());
        fetch_preview_at(
            &mut app,
            since + PREVIEW_REST,
            Ok(add_polling_preview(shown)),
        );

        assert!(app.assessment_to_start().is_none());
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(!preview_border(&lines).contains("voight"), "{lines:#?}");
    }

    /// [`app`] with "Add polling" a pull request whose Changed files the REST API has, drawn on a screen tall
    /// enough for the Files screen.
    fn app_with_a_pull_request() -> App {
        let mut app = app();
        let mut held = notifications();
        held[0].api_paths = ApiPaths {
            subject: Some("/repos/wcgw/ghd/pulls/1".to_owned()),
            latest_comment: None,
        };
        app.fetched(fetched(held));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app
    }

    fn changed(path: &str, status: FileStatus, patch: Option<&str>) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            status,
            previous_path: None,
            additions: 3,
            deletions: 1,
            patch: patch.map(str::to_owned),
            contents_path: format!("/repos/wcgw/ghd/contents/{path}?ref=abc"),
        }
    }

    /// A new Markdown file, a modified Rust one, a removed text file, and a Rust one renamed as it was.
    fn changed_files() -> Vec<ChangedFile> {
        vec![
            changed(
                "docs/guide.md",
                FileStatus::Added,
                Some("@@ -0,0 +1,3 @@\n+# Guide\n+\n+Read me."),
            ),
            changed(
                "src/ui.rs",
                FileStatus::Modified,
                Some("@@ -1,2 +1,2 @@\n fn a() {}\n-old\n+new"),
            ),
            ChangedFile {
                deletions: 5,
                ..changed("old.txt", FileStatus::Removed, None)
            },
            ChangedFile {
                previous_path: Some("src/old.rs".to_owned()),
                additions: 0,
                deletions: 0,
                ..changed("src/new.rs", FileStatus::Renamed, None)
            },
        ]
    }

    /// [`app_with_a_pull_request`] with its Files screen open, its Changed files fetched as `files`.
    fn app_with_files(files: Vec<ChangedFile>) -> App {
        let mut app = app_with_a_pull_request();
        app.press(key_named("f"));
        let wanted = app.files.list_to_fetch().expect("fetches the files");
        app.files.took_list(files::WasFetched {
            thread: wanted.thread,
            as_of: wanted.as_of,
            files: Ok(files),
        });
        app
    }

    #[test]
    fn f_shows_a_pull_requests_files_and_a_new_one_whole() {
        let mut app = app_with_files(changed_files());

        // Tall enough for the four files, in the list's quarter of it.
        let lines = screen_of(&mut app, 24);

        assert!(
            shows(&lines, "#1 Add polling · 4 files · +9 \u{2212}7"),
            "{lines:#?}"
        );
        assert!(shows(&lines, "A docs/guide.md"), "{lines:#?}");
        assert!(shows(&lines, "M src/ui.rs"), "{lines:#?}");
        assert!(shows(&lines, "D old.txt"), "{lines:#?}");
        assert!(
            shows(&lines, "R src/old.rs \u{2192} src/new.rs"),
            "{lines:#?}"
        );
        assert!(shows(&lines, " docs/guide.md "), "the file's pane");
        assert!(shows(&lines, "Read me."), "{lines:#?}");
        assert!(!shows(&lines, "+Read me."), "not as a diff");
        assert!(app.files.list_to_fetch().is_none(), "fetched already");
        assert!(app.files.text_to_fetch().is_none(), "whole from its patch");
    }

    #[test]
    fn changed_file_shows_its_diff_and_w_shows_it_whole() {
        let mut app = app_with_files(changed_files());
        app.press(key_named("j"));
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "-old"), "{lines:#?}");
        assert!(shows(&lines, "+new"), "{lines:#?}");
        assert!(
            app.files.text_to_fetch().is_none(),
            "the diff needs nothing more"
        );

        app.press(key_named("w"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        assert_eq!(
            wanted.contents_path,
            "/repos/wcgw/ghd/contents/src/ui.rs?ref=abc"
        );
        assert!(shows(
            &screen_of(&mut app, PREVIEW_SCREEN_HEIGHT),
            "Fetching…"
        ));
        app.files.took_text(files::TextFetched {
            thread: wanted.thread,
            contents_path: wanted.contents_path,
            text: Ok(FileText::of("fn a() {}\nnew\n".to_owned())),
        });
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "fn a() {}"), "{lines:#?}");
        assert!(!shows(&lines, "-old"), "{lines:#?}");

        app.press(key_named("w"));
        assert!(shows(&screen_of(&mut app, PREVIEW_SCREEN_HEIGHT), "-old"));
    }

    #[test]
    fn w_shows_every_file_whole_until_pressed_again() {
        let mut app = app_with_files(changed_files());
        // On the new file, which shows whole all the same.
        app.press(key_named("w"));

        app.press(key_named("j"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        assert_eq!(
            wanted.contents_path,
            "/repos/wcgw/ghd/contents/src/ui.rs?ref=abc"
        );
        app.press(key_named("j"));
        assert!(
            app.files.text_to_fetch().is_none(),
            "a removed file isn't shown whole all the same"
        );
        app.press(key_named("j"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        assert_eq!(
            wanted.contents_path,
            "/repos/wcgw/ghd/contents/src/new.rs?ref=abc"
        );

        app.press(key_named("w"));
        app.press(key_named("k"));
        app.press(key_named("k"));
        assert!(shows(&screen_of(&mut app, PREVIEW_SCREEN_HEIGHT), "-old"));
    }

    #[test]
    fn diffs_are_highlighted_on_their_lines_background() {
        let mut app = app_with_files(changed_files());
        app.theme = Theme {
            syntax_keyword: Style::new().fg(Color::Magenta),
            diff_added: Style::new().fg(Color::Green),
            diff_removed: Style::new().fg(Color::Red),
            diff_added_line: Style::new().bg(Color::Rgb(1, 1, 1)),
            diff_removed_line: Style::new().bg(Color::Rgb(2, 2, 2)),
            markdown_muted: Style::new().fg(Color::Gray),
            ..Theme::default()
        };
        app.press(key_named("j"));

        assert_eq!(cell_of(&mut app, "@@ -1,2").fg, Color::Gray);
        assert_eq!(cell_of(&mut app, "fn a()").fg, Color::Magenta, "context");
        let added = cell_of(&mut app, "+new");
        assert_eq!((added.fg, added.bg), (Color::Green, Color::Rgb(1, 1, 1)));
        assert_eq!(cell_of(&mut app, "new ").bg, Color::Rgb(1, 1, 1));
        let removed = cell_of(&mut app, "-old");
        assert_eq!((removed.fg, removed.bg), (Color::Red, Color::Rgb(2, 2, 2)));
        assert_eq!(
            line_end_of(&mut app, "-old").bg,
            Color::Rgb(2, 2, 2),
            "past the text, across the pane"
        );
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let removed_at = lines.iter().position(|line| line.contains("-old"));
        let added_at = lines.iter().position(|line| line.contains("+new"));
        assert_eq!(
            added_at,
            removed_at.map(|at| at + 1),
            "no line of its own: {lines:#?}"
        );

        app.press(key_named("w"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        app.files.took_text(files::TextFetched {
            thread: wanted.thread,
            contents_path: wanted.contents_path,
            text: Ok(FileText::of("fn a() {}\nnew\n".to_owned())),
        });

        assert_eq!(
            cell_of(&mut app, "fn a()").fg,
            Color::Magenta,
            "whole, highlighted apart from its diff"
        );
    }

    #[test]
    fn whole_files_show_what_changed_in_their_gutter() {
        let modified = changed(
            "src/ui.rs",
            FileStatus::Modified,
            Some("@@ -1,5 +1,4 @@\n-first\n fn a() {}\n-old\n+new\n+added\n kept\n-gone"),
        );
        let mut app = app_with_files(vec![modified]);
        app.theme = Theme {
            diff_added: Style::new().fg(Color::Green),
            diff_removed: Style::new().fg(Color::Red),
            diff_changed: Style::new().fg(Color::Yellow),
            ..Theme::default()
        };
        app.press(key_named("w"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        app.files.took_text(files::TextFetched {
            thread: wanted.thread,
            contents_path: wanted.contents_path,
            text: Ok(FileText::of("fn a() {}\nnew\nadded\nkept\n".to_owned())),
        });

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        for line in [
            "│\u{2594} fn a() {}",
            "│\u{258e} new",
            "│\u{258e} added",
            "│\u{2581} kept",
        ] {
            assert!(shows(&lines, line), "{line:?} in {lines:#?}");
        }
        assert_eq!(cell_of(&mut app, "\u{2594} fn").fg, Color::Red);
        assert_eq!(cell_of(&mut app, "\u{258e} new").fg, Color::Yellow);
        assert_eq!(cell_of(&mut app, "\u{258e} added").fg, Color::Green);
        assert_eq!(cell_of(&mut app, "\u{2581} kept").fg, Color::Red);
    }

    #[test]
    fn markdown_shown_whole_shows_what_changed_in_its_gutter() {
        // "Gone." is removed below a blank line, which isn't drawn.
        let patch = "@@ -1,7 +1,6 @@\n # Guide\n \n-Old words.\n+New words.\n \n Kept.\n \n-Gone.";
        let table = format!("| {} |\n|---|\n| cell |\n", "wide ".repeat(30));
        let mut app = app_with_files(vec![changed(
            "docs/guide.md",
            FileStatus::Modified,
            Some(patch),
        )]);
        app.press(key_named("w"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        app.files.took_text(files::TextFetched {
            thread: wanted.thread,
            contents_path: wanted.contents_path,
            text: Ok(FileText::of(format!(
                "# Guide\n\nNew words.\n\nKept.\n\n{table}"
            ))),
        });

        let lines = screen_of(&mut app, 30);

        for line in ["│  # Guide", "│\u{258e} New words.", "│\u{2581} Kept."] {
            assert!(shows(&lines, line), "{line:?} in {lines:#?}");
        }
        assert!(
            shows(&lines, "\u{2510}│"),
            "the table fits beside the gutter: {lines:#?}"
        );
    }

    #[test]
    fn the_file_takes_three_quarters_of_the_files_screen() {
        let mut app = app_with_files(changed_files());

        // A line for the status, 20 for the screen.
        let lines = screen_of(&mut app, 21);

        let file_pane = lines
            .iter()
            .position(|line| line.starts_with("┌ docs/guide.md"))
            .expect("the file's pane");
        assert_eq!(file_pane, 5, "{lines:#?}");
    }

    #[test]
    fn the_file_takes_what_the_list_needs_not_of_its_quarter() {
        let mut app = app_with_files(changed_files());

        // A line for the status, 40 for the screen: a quarter of them is 10, and 6 list the 4 files.
        let lines = screen_of(&mut app, 41);

        let file_pane = lines
            .iter()
            .position(|line| line.starts_with("┌ docs/guide.md"))
            .expect("the file's pane");
        assert_eq!(file_pane, 6, "{lines:#?}");
        assert!(shows(&lines, "src/new.rs"), "{lines:#?}");
    }

    #[test]
    fn a_pull_request_without_files_says_so() {
        let mut app = app_with_files(Vec::new());

        let lines = screen_of(&mut app, 21);

        assert!(shows(&lines, "│No files."), "{lines:#?}");
    }

    #[test]
    fn the_list_keeps_its_quarter_while_it_shows_no_files() {
        let mut app = app_with_a_pull_request();
        app.press(key_named("f"));

        let lines = screen_of(&mut app, 21);

        assert!(shows(&lines, "Fetching files…"), "{lines:#?}");
        let panes: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with('┌'))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(panes, [0, 5], "{lines:#?}");
    }

    #[test]
    fn no_gutter_without_a_patch() {
        let mut app = app_with_files(vec![changed("src/big.rs", FileStatus::Modified, None)]);
        app.press(key_named("w"));
        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        app.files.took_text(files::TextFetched {
            thread: wanted.thread,
            contents_path: wanted.contents_path,
            text: Ok(FileText::of("fn big() {}\n".to_owned())),
        });

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        assert!(shows(&lines, "│fn big() {}"), "{lines:#?}");
    }

    #[test]
    fn gutter_shows_lines_removed_next_to_lines_drawn() {
        use crate::files::LineChange::{Added, Modified, RemovedAbove, RemovedBelow};

        assert_eq!(
            files::gutter_changes(&[None, Some(RemovedBelow)], &[Some(0), Some(1)]),
            [None, Some(RemovedBelow)],
            "drawn, where they are"
        );
        // Lines 0 and 2 aren't drawn: blank lines, say. Lines removed below line 2 show below line 1.
        assert_eq!(
            files::gutter_changes(&[None, None, Some(RemovedBelow)], &[None, Some(1), Some(3)]),
            [None, Some(RemovedBelow), None],
        );
        assert_eq!(
            files::gutter_changes(
                &[None, Some(Added), Some(RemovedBelow), Some(Modified)],
                &[Some(1), None, Some(3)]
            ),
            [Some(Added), None, Some(Modified)],
            "already showing a change"
        );
        // With nothing drawn above, lines removed below line 0 are above the first line drawn.
        for change in [RemovedAbove, RemovedBelow] {
            assert_eq!(
                files::gutter_changes(&[Some(change), None], &[None, Some(1)]),
                [None, Some(RemovedAbove)],
                "{change:?}"
            );
        }
    }

    #[test]
    fn whole_files_are_highlighted_as_far_down_as_they_show() {
        let comments = "+    // a comment\n".repeat(200);
        let patch = format!("@@ -0,0 +1,202 @@\n+fn main() {{\n+\tlet x = 1;\n{comments}");
        let mut app = app_with_files(vec![changed(
            "src/main.rs",
            FileStatus::Added,
            Some(&patch),
        )]);
        app.theme = Theme {
            syntax_keyword: Style::new().fg(Color::Magenta),
            ..Theme::default()
        };

        assert_eq!(cell_of(&mut app, "fn main").fg, Color::Magenta);
        assert_eq!(
            cell_of(&mut app, "    let x").fg,
            Color::Reset,
            "a tab, as spaces"
        );
        assert_eq!(cell_of(&mut app, "let x").fg, Color::Magenta);
        let first = app.files.lines_highlighted();
        assert!(
            (1..usize::from(PREVIEW_SCREEN_HEIGHT)).contains(&first),
            "the pane's lines only: {first}"
        );

        app.press(key_named("J"));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        let further = app.files.lines_highlighted();
        assert!(further > first, "further down");

        app.press(key_named("K"));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        assert_eq!(app.files.lines_highlighted(), further, "kept");
    }

    #[test]
    fn another_file_shows_from_its_top() {
        let long: Vec<String> = (0..60).map(|i| format!("+Line {i}")).collect();
        let patch = format!("@@ -0,0 +1,60 @@\n{}", long.join("\n"));
        let mut app = app_with_files(vec![
            changed("long.txt", FileStatus::Added, Some(&patch)),
            changed(
                "short.txt",
                FileStatus::Added,
                Some("@@ -0,0 +1 @@\n+Short"),
            ),
        ]);
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("J"));
        assert!(app.files.scrolled_by() > 0);

        app.press(key_named("j"));

        assert_eq!(app.files.scrolled_by(), 0);
    }

    #[test]
    fn moving_no_further_keeps_the_files_place() {
        let long: Vec<String> = (0..60).map(|i| format!("+Line {i}")).collect();
        let patch = format!("@@ -0,0 +1,60 @@\n{}", long.join("\n"));
        // Its only file, the first and the last.
        let mut app = app_with_files(vec![changed("long.txt", FileStatus::Added, Some(&patch))]);
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("J"));
        let scrolled = app.files.scrolled_by();
        assert!(scrolled > 0);

        for key in ["j", "Ctrl-D", "G", "k", "Ctrl-U", "g"] {
            app.press(key_named(key));
            assert_eq!(app.files.scrolled_by(), scrolled, "{key} on the only file");
        }
    }

    #[test]
    fn w_shows_the_file_from_its_top() {
        let long: Vec<String> = (0..60).map(|i| format!("+Line {i}")).collect();
        let patch = format!("@@ -1,0 +1,60 @@\n{}", long.join("\n"));
        let mut app = app_with_files(vec![changed(
            "long.txt",
            FileStatus::Modified,
            Some(&patch),
        )]);
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("J"));
        assert!(app.files.scrolled_by() > 0);

        app.press(key_named("w"));

        assert_eq!(app.files.scrolled_by(), 0);
    }

    #[test]
    fn wheel_scrolls_the_file_on_the_files_screen() {
        let long: Vec<String> = (0..60).map(|i| format!("+Line {i}")).collect();
        let patch = format!("@@ -0,0 +1,60 @@\n{}", long.join("\n"));
        let mut app = app_with_files(vec![
            changed("long.txt", FileStatus::Added, Some(&patch)),
            changed(
                "short.txt",
                FileStatus::Added,
                Some("@@ -0,0 +1 @@\n+Short"),
            ),
        ]);
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        app.wheel(Wheel::Down);

        assert_eq!(app.files.scrolled_by(), WHEEL_LINES);
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            lines.iter().any(|line| line.starts_with("┌ long.txt")),
            "the same file: {lines:#?}"
        );
    }

    #[test]
    fn longer_file_has_a_scrollbar() {
        let long: Vec<String> = (0..60).map(|i| format!("+Line {i}")).collect();
        let patch = format!("@@ -0,0 +1,60 @@\n{}", long.join("\n"));
        let mut app = app_with_files(vec![
            changed("long.txt", FileStatus::Added, Some(&patch)),
            changed(
                "short.txt",
                FileStatus::Added,
                Some("@@ -0,0 +1 @@\n+Short"),
            ),
        ]);
        app.theme = Theme {
            scrollbar: Style::new().fg(Color::Magenta),
            ..Theme::default()
        };

        assert_eq!(cell_of(&mut app, "┃").fg, Color::Magenta);
        app.press(key_named("j"));
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(!shows(&lines, "┃"), "{lines:#?}");
    }

    /// [`app_with_files`] with a new file, "long.txt", of 60 lines "Line 0" to "Line 59", the ones at `needles`
    /// ending in " needle"; then a short one, "short.txt", that's "Short needle". Drawn once.
    fn app_with_needles(needles: &[usize]) -> App {
        let long: Vec<String> = (0..60)
            .map(|i| {
                if needles.contains(&i) {
                    format!("+Line {i} needle")
                } else {
                    format!("+Line {i}")
                }
            })
            .collect();
        let patch = format!("@@ -0,0 +1,60 @@\n{}", long.join("\n"));
        let mut app = app_with_files(vec![
            changed("long.txt", FileStatus::Added, Some(&patch)),
            changed(
                "short.txt",
                FileStatus::Added,
                Some("@@ -0,0 +1 @@\n+Short needle"),
            ),
        ]);
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app
    }

    /// Presses `/` on the Files screen and types `text`, which does nothing else.
    fn look_for(app: &mut App, text: &str) {
        assert!(app.press(key_named("/")).is_none());
        for character in text.chars() {
            let key = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
            assert!(app.press(key).is_none(), "{character:?} is typed");
        }
    }

    /// The line in the middle of the Files screen's file pane, inside its border, on `lines`; and its bottom border.
    fn file_pane(lines: &[String]) -> (&str, &str) {
        let top = lines
            .iter()
            .rposition(|line| line.starts_with('┌'))
            .expect("a file pane");
        let bottom = lines
            .iter()
            .rposition(|line| line.starts_with('└'))
            .expect("a file pane");
        let rows = bottom - top - 1;
        (&lines[top + 1 + rows / 2], &lines[bottom])
    }

    #[test]
    fn slash_looks_in_the_file_from_where_it_shows_and_brings_it_to_the_middle() {
        let mut app = app_with_needles(&[5, 40]);
        // Past the first.
        for _ in 0..3 {
            app.wheel(Wheel::Down);
        }

        look_for(&mut app, "NEEDLE");

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let (middle, bottom) = file_pane(&lines);
        assert!(middle.contains("Line 40 needle"), "{lines:#?}");
        assert!(bottom.contains(" 2 of 2 "), "{bottom}");
        assert!(!bottom.contains("NEEDLE"), "not shown twice while typing");
        assert!(lines[lines.len() - 1].starts_with("/NEEDLE "), "{lines:#?}");

        app.press(key_named("Enter"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let (middle, bottom) = file_pane(&lines);
        assert!(middle.contains("Line 40 needle"), "stays: {lines:#?}");
        assert!(bottom.contains(" /NEEDLE · 2 of 2 "), "{bottom}");
        assert!(
            !lines[lines.len() - 1].contains("NEEDLE"),
            "the prompt closed"
        );
    }

    #[test]
    fn n_and_shift_n_go_through_the_matches_and_around() {
        let mut app = app_with_needles(&[10, 30, 50]);
        look_for(&mut app, "needle");
        app.press(key_named("Enter"));
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let (middle, bottom) = file_pane(&lines);
        assert!(middle.contains("Line 10 needle"), "{lines:#?}");
        assert!(bottom.contains(" 1 of 3 "), "{bottom}");

        for (key, line, of) in [
            ("n", 30, 2),
            ("n", 50, 3),
            ("n", 10, 1),
            ("N", 50, 3),
            ("N", 30, 2),
        ] {
            assert!(app.press(key_named(key)).is_none());

            let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
            let (middle, bottom) = file_pane(&lines);
            assert!(
                middle.contains(&format!("Line {line} needle")),
                "{key}: {lines:#?}"
            );
            assert!(bottom.contains(&format!(" {of} of 3 ")), "{key}: {bottom}");
        }
    }

    #[test]
    fn n_and_shift_n_go_from_what_shows_once_scrolled_away() {
        let mut app = app_with_needles(&[10, 30, 50]);
        look_for(&mut app, "needle");
        app.press(key_named("Enter"));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        // From line 4 down to 37: past the second, the first gone above.
        for _ in 0..11 {
            app.wheel(Wheel::Down);
        }

        app.press(key_named("n"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let (middle, bottom) = file_pane(&lines);
        assert!(middle.contains("Line 50 needle"), "{lines:#?}");
        assert!(bottom.contains(" 3 of 3 "), "{bottom}");

        // From line 44 up to 17: the third gone below, and the second just below the 13 rows that show.
        for _ in 0..9 {
            app.wheel(Wheel::Up);
        }
        assert_eq!(app.files.scrolled_by(), 17);
        app.press(key_named("N"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let (middle, bottom) = file_pane(&lines);
        assert!(middle.contains("Line 10 needle"), "{lines:#?}");
        assert!(bottom.contains(" 1 of 3 "), "{bottom}");
    }

    #[test]
    fn jumps_go_around_the_ends_of_the_file() {
        let mut app = app_with_needles(&[40]);
        // Past it.
        for _ in 0..15 {
            app.wheel(Wheel::Down);
        }

        look_for(&mut app, "needle");

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(file_pane(&lines).0.contains("Line 40 needle"), "{lines:#?}");

        app.press(key_named("Enter"));
        // Above it, with nothing above it to go back to.
        for _ in 0..12 {
            app.wheel(Wheel::Up);
        }
        assert_eq!(app.files.scrolled_by(), 0);
        app.press(key_named("N"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(file_pane(&lines).0.contains("Line 40 needle"), "{lines:#?}");
    }

    #[test]
    fn n_does_nothing_before_anything_is_looked_for() {
        let mut app = app_with_needles(&[40]);

        app.press(key_named("n"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert_eq!(app.files.scrolled_by(), 0);
        assert!(!file_pane(&lines).1.contains("needle"), "{lines:#?}");
    }

    #[test]
    fn every_match_in_the_file_is_highlighted_the_one_jumped_to_apart() {
        let mut app = app_with_files(vec![changed(
            "notes.txt",
            FileStatus::Added,
            Some("@@ -0,0 +1,3 @@\n+One needle\n+Two\n+Three NEEDLE"),
        )]);
        app.theme = Theme {
            found: Style::new().bg(Color::Yellow),
            found_current: Style::new().bg(Color::Red),
            ..Theme::default()
        };
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        look_for(&mut app, "needle");
        app.press(key_named("Enter"));

        assert_eq!(cell_of(&mut app, "needle").bg, Color::Red);
        assert_eq!(cell_of(&mut app, "NEEDLE").bg, Color::Yellow);
        assert_eq!(cell_of(&mut app, "One").bg, Color::Reset);

        app.press(key_named("n"));

        assert_eq!(cell_of(&mut app, "needle").bg, Color::Yellow);
        assert_eq!(cell_of(&mut app, "NEEDLE").bg, Color::Red);
    }

    #[test]
    fn a_jump_down_the_file_is_highlighted_where_it_lands() {
        let comments = "+// a comment\n".repeat(200);
        let patch = format!("@@ -0,0 +1,201 @@\n{comments}+fn needle() {{}}");
        let mut app = app_with_files(vec![changed(
            "src/main.rs",
            FileStatus::Added,
            Some(&patch),
        )]);
        app.theme = Theme {
            syntax_keyword: Style::new().fg(Color::Magenta),
            ..Theme::default()
        };
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        look_for(&mut app, "needle");

        assert_eq!(cell_of(&mut app, "fn needle").fg, Color::Magenta);
    }

    #[test]
    fn typing_on_looks_again_from_where_the_prompt_opened() {
        let mut app = app_with_needles(&[40]);
        look_for(&mut app, "needle");
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let found_at = app.files.scrolled_by();
        assert!(found_at > 0);

        app.press(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(file_pane(&lines).1.contains(" no match "), "{lines:#?}");
        assert_eq!(app.files.scrolled_by(), 0, "back where it was");

        app.press(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(file_pane(&lines).1.contains(" 1 of 1 "), "{lines:#?}");
        assert_eq!(app.files.scrolled_by(), found_at);
    }

    #[test]
    fn esc_in_the_files_prompt_puts_back_what_was_looked_for_and_where() {
        let mut app = app_with_needles(&[40]);
        look_for(&mut app, "needle");
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(app.files.scrolled_by() > 0);

        app.press(key_named("Esc"));

        assert!(app.files.is_open(), "only the prompt closed");
        assert_eq!(app.files.scrolled_by(), 0, "back where it was");
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(!file_pane(&lines).1.contains("needle"), "{lines:#?}");

        look_for(&mut app, "needle");
        app.press(key_named("Enter"));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        let found_at = app.files.scrolled_by();
        // After what's looked for already.
        look_for(&mut app, "zzz");
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        app.press(key_named("Esc"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            file_pane(&lines).1.contains(" /needle · 1 of 1 "),
            "{lines:#?}"
        );
        assert_eq!(app.files.scrolled_by(), found_at);

        // Before the pane is drawn again: nothing is left to jump to.
        for _ in 0..12 {
            app.wheel(Wheel::Up);
        }
        look_for(&mut app, "x");
        app.press(key_named("Esc"));

        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert_eq!(app.files.scrolled_by(), 0);
    }

    #[test]
    fn esc_stops_looking_in_the_file_before_going_back() {
        let mut app = app_with_needles(&[40]);
        look_for(&mut app, "needle");
        app.press(key_named("Enter"));

        app.press(key_named("Esc"));

        assert!(app.files.is_open());
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(!file_pane(&lines).1.contains("needle"), "{lines:#?}");

        app.press(key_named("Esc"));

        assert!(!app.files.is_open());
    }

    #[test]
    fn keys_go_to_the_files_prompt_while_typing() {
        let mut app = app_with_needles(&[]);

        // Not the command line, nor the next file, nor back.
        look_for(&mut app, ":jf");

        assert!(app.files.is_open());
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            lines.iter().any(|line| line.starts_with("┌ long.txt")),
            "{lines:#?}"
        );
        assert!(lines[lines.len() - 1].starts_with("/:jf "), "{lines:#?}");

        // Nor half a page down.
        assert!(app.press(key_named("Ctrl-D")).is_none());

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(lines[lines.len() - 1].starts_with("/:jf "), "{lines:#?}");
        assert!(matches!(
            app.press(key_named("Ctrl-C")),
            Some(Outcome::Quit)
        ));
    }

    #[test]
    fn another_file_is_looked_in_too_from_its_top() {
        // "needle" where it is in the other.
        let mut app = app_with_files(vec![
            changed("a.txt", FileStatus::Added, Some("@@ -0,0 +1 @@\n+a needle")),
            changed(
                "b.txt",
                FileStatus::Added,
                Some("@@ -0,0 +1,2 @@\n+b needle\n+more needles"),
            ),
        ]);
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        look_for(&mut app, "needle");
        app.press(key_named("Enter"));
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        // Not made before the next file shows.
        app.press(key_named("n"));

        app.press(key_named("j"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            file_pane(&lines).1.contains(" /needle · 2 matches "),
            "{lines:#?}"
        );

        app.press(key_named("n"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            file_pane(&lines).1.contains(" /needle · 1 of 2 "),
            "{lines:#?}"
        );

        app.press(key_named("k"));

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(
            file_pane(&lines).1.contains(" /needle · 1 match "),
            "{lines:#?}"
        );
    }

    #[test]
    fn removed_and_renamed_files_say_so() {
        let mut app = app_with_files(changed_files());
        app.press(key_named("j"));
        app.press(key_named("j"));
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "Deleted: 5 lines"), "{lines:#?}");
        app.press(key_named("w"));
        assert!(app.files.text_to_fetch().is_none(), "nothing whole to show");

        app.press(key_named("j"));
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "Renamed from src/old.rs"), "{lines:#?}");
        assert!(!shows(&lines, "No diff"), "{lines:#?}");
    }

    #[test]
    fn new_file_without_a_patch_is_fetched_whole() {
        let mut app = app_with_files(vec![changed("logo.png", FileStatus::Added, None)]);

        let wanted = app.files.text_to_fetch().expect("fetches it whole");
        app.files.took_text(files::TextFetched {
            thread: wanted.thread,
            contents_path: wanted.contents_path,
            text: Ok(FileText::Binary),
        });

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "Binary: o opens it."), "{lines:#?}");
        assert!(app.files.text_to_fetch().is_none(), "fetched already");
    }

    #[test]
    fn only_pull_requests_have_files() {
        let mut app = app();
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        app.press(key_named("f"));

        assert!(!app.files.is_open());
        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "Only pull requests have files"), "{lines:#?}");
    }

    #[test]
    fn notification_keys_do_nothing_in_files_and_esc_goes_back() {
        let mut app = app_with_files(changed_files());

        assert!(app.press(key_named("e")).is_none(), "not marked Done");
        assert!(app.press(key_named("I")).is_none(), "not marked read");
        assert_eq!(inbox_titles(&app), ["Add polling", "Old news"]);

        app.press(key_named("Esc"));
        assert!(!app.files.is_open());
        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
    }

    #[test]
    fn help_shows_over_the_files_screen() {
        let mut app = app_with_files(changed_files());

        assert!(app.press(key_named("?")).is_none());

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        assert!(shows(&lines, "show these keys"), "{lines:#?}");

        app.press(key_named("Esc"));
        assert!(app.files.is_open(), "dismissing the help leaves the screen");
    }

    #[test]
    fn r_in_files_fetches_them_again() {
        let mut app = app_with_files(changed_files());

        assert!(app.press(key_named("r")).is_none(), "not the Inbox");

        assert!(app.files.list_to_fetch().is_some());
    }

    #[test]
    fn o_goes_to_the_file_on_github() {
        let mut app = app_with_files(changed_files());
        app.press(key_named("j"));

        assert_eq!(
            app.files.selected_url().as_deref(),
            Some(
                format!(
                    "https://github.com/wcgw/ghd/pull/1/files#{}",
                    changed("src/ui.rs", FileStatus::Modified, None).anchor()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn g_and_shift_g_go_to_the_first_and_last_file() {
        let files = changed_files();
        let mut app = app_with_files(files.clone());
        let url_of = |file: &ChangedFile| {
            Some(format!(
                "https://github.com/wcgw/ghd/pull/1/files#{}",
                file.anchor()
            ))
        };

        app.press(key_named("G"));

        // At once, not only once drawn: fetching goes by the selected file before that.
        assert_eq!(app.files.selected_url(), url_of(&files[3]));

        app.press(key_named("g"));

        assert_eq!(app.files.selected_url(), url_of(&files[0]));
    }

    #[test]
    fn moving_before_the_files_arrive_stays_on_the_first() {
        let files = changed_files();
        let mut app = app_with_a_pull_request();
        app.press(key_named("f"));
        let wanted = app.files.list_to_fetch().expect("fetches the files");

        app.press(key_named("j"));
        app.files.took_list(files::WasFetched {
            thread: wanted.thread,
            as_of: wanted.as_of,
            files: Ok(files.clone()),
        });

        assert_eq!(
            app.files.selected_url(),
            Some(format!(
                "https://github.com/wcgw/ghd/pull/1/files#{}",
                files[0].anchor()
            ))
        );
    }

    #[test]
    fn voight_assesses_new_markdown_files_only() {
        let mut app = app_with_files(changed_files());
        app.assessments = Some(Assessments::default());
        app.voight_said(assessment::Event::Loaded { fallback: None });

        let (key, text) = app.assessment_to_start().expect("a new Markdown file");
        assert_eq!(
            key.part,
            assessment::Part::File("/repos/wcgw/ghd/contents/docs/guide.md?ref=abc".to_owned())
        );
        assert_eq!(text, "# Guide\n\nRead me.\n");
        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(Some(assessed_at(0.4))),
        });
        assert!(
            preview_border(&screen_of(&mut app, PREVIEW_SCREEN_HEIGHT)).ends_with(" voight 40% ┘")
        );

        app.theme = heatmap_theme();
        app.press(key_named("H"));
        assert_eq!(
            background_of(&mut app, "Read me."),
            Color::Reset,
            "no windows to shade by"
        );

        app.press(key_named("j"));
        assert!(app.assessment_to_start().is_none(), "a diff, of code");

        let mut app = app_with_files(vec![changed(
            "notes.txt",
            FileStatus::Added,
            Some("@@ -0,0 +1 @@\n+Plain prose, not Markdown."),
        )]);
        app.assessments = Some(Assessments::default());
        app.voight_said(assessment::Event::Loaded { fallback: None });
        assert!(app.assessment_to_start().is_none(), "not Markdown");
    }

    #[test]
    fn h_shows_the_heatmap_of_a_file_shown_whole() {
        let mut app = app_with_files(changed_files());
        app.theme = heatmap_theme();
        app.assessments = Some(Assessments::default());
        app.voight_said(assessment::Event::Loaded { fallback: None });
        let (key, text) = app.assessment_to_start().expect("a new Markdown file");
        app.voight_said(assessment::Event::Assessed {
            key,
            assessment: Ok(Some(Assessment {
                share: 1.0,
                windows: vec![window_over(&text, "Read me.", 1.0)],
            })),
        });

        app.press(key_named("H"));

        assert!(app.files.is_open(), "still on the Files screen");
        assert_eq!(background_of(&mut app, "Read me."), Color::Red);
    }

    #[test]
    fn files_need_a_tall_enough_terminal() {
        let mut app = app_with_a_pull_request();
        screen_of(&mut app, PREVIEW_MIN_HEIGHT - 1);

        app.press(key_named("f"));

        assert!(!app.files.is_open());
    }

    #[test]
    fn too_short_a_terminal_keeps_the_preview_closed() {
        let mut app = app();
        screen_of(&mut app, PREVIEW_MIN_HEIGHT - 1);

        app.press(key_named("p"));

        assert!(app.preview.is_none());
        let lines = screen_of(&mut app, PREVIEW_MIN_HEIGHT - 1);
        assert!(
            shows(
                &lines,
                "The Preview needs a terminal at least 12 lines tall"
            ),
            "{lines:#?}"
        );
    }

    #[test]
    fn kinds_without_a_preview_say_so() {
        let mut app = app();
        screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);
        app.press(key_named("p"));
        app.note_cursor(Instant::now());

        let lines = screen_of(&mut app, PREVIEW_SCREEN_HEIGHT);

        assert!(
            shows(
                &lines,
                "No Preview for this kind of Notification yet: o opens it."
            ),
            "{lines:#?}"
        );
        assert!(app.preview_due().is_none());
    }

    #[test]
    fn u_shows_only_the_unread_in_the_view_on_screen() {
        let mut app = app();

        assert!(app.press(key_named("u")).is_none());
        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Add polling"]);
        assert!(lines[3].contains(" ● unread only "), "{lines:#?}");

        app.execute(Command::ShowView(2));
        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Old news"], "each View has its own");
        assert!(!lines[3].contains("unread only"), "{lines:#?}");

        app.execute(Command::ShowView(0));
        assert_eq!(titles(&screen(&mut app)), ["Add polling"], "and keeps it");
        app.press(key_named("u"));
        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
    }

    #[test]
    fn unread_only_keeps_the_cursor_on_its_notification() {
        let mut app = inbox_view_only();
        let mut held = notifications();
        let brand_new = Notification {
            thread: ThreadId(4),
            subject_title: "Brand new".to_owned(),
            unread: false,
            updated_at: held[0].updated_at + TimeDelta::minutes(1),
            ..held[0].clone()
        };
        held.push(brand_new);
        app.fetched(fetched(held));
        app.execute(Command::Down);
        assert_eq!(cursor_title(&app), Some("Add polling"));

        app.press(key_named("u"));

        assert_eq!(cursor_title(&app), Some("Add polling"));
    }

    #[test]
    fn read_leaves_a_view_showing_unread_only() {
        let mut app = app();
        app.press(key_named("u"));

        assert!(app.press(key_named("I")).is_some());
        let lines = screen(&mut app);
        assert!(
            lines[1].contains("Nothing unread in this View."),
            "{lines:#?}"
        );

        app.set_search("polling".to_owned());
        let lines = screen(&mut app);
        assert!(
            lines[1].contains("Nothing unread in this View matches the search."),
            "{lines:#?}"
        );
    }

    #[test]
    fn unread_only_is_for_the_inbox() {
        let mut app = app();
        app.fetched(fetched_with_done(notifications()));
        app.press(key_named("u"));

        app.press(key_named("D"));
        let lines = screen(&mut app);
        assert_eq!(row_titles(&lines), ["Gone quiet"], "the whole Done list");
        assert!(!lines[3].contains("unread only"), "{lines:#?}");

        app.press(key_named("u"));
        let lines = screen(&mut app);
        assert!(
            lines[4].starts_with("Unread only is for the Inbox, not the Done list"),
            "{lines:#?}"
        );

        app.press(key_named("D"));
        assert_eq!(
            titles(&screen(&mut app)),
            ["Add polling"],
            "still on in the Inbox"
        );
    }

    /// The app with an Inbox of `count` Notifications, "Notification 0" newest, drawn on a screen with room for
    /// `rows` of them.
    fn app_with_many(count: u64, rows: u16) -> App {
        let mut app = inbox_view_only();
        let template = notifications().remove(0);
        app.fetched(fetched(
            (0..count)
                .map(|i| Notification {
                    thread: ThreadId(100 + i),
                    subject_title: format!("Notification {i}"),
                    updated_at: template.updated_at
                        - TimeDelta::minutes(i64::try_from(i).expect("small")),
                    ..template.clone()
                })
                .collect(),
        ));
        // Two lines of border and one of status around the rows.
        let mut terminal = Terminal::new(TestBackend::new(SCREEN_WIDTH, rows + 3))
            .expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");
        app
    }

    #[test]
    fn ctrl_d_and_ctrl_u_move_half_a_page() {
        let mut app = app_with_many(30, 10);

        app.press(key_named("Ctrl-D"));
        assert_eq!(cursor_title(&app), Some("Notification 5"));
        app.press(key_named("Ctrl-D"));
        assert_eq!(cursor_title(&app), Some("Notification 10"));
        app.press(key_named("Ctrl-U"));
        assert_eq!(cursor_title(&app), Some("Notification 5"));
        app.press(key_named("Ctrl-U"));
        app.press(key_named("Ctrl-U"));
        assert_eq!(
            cursor_title(&app),
            Some("Notification 0"),
            "no further than the top"
        );
    }

    #[test]
    fn ctrl_d_stops_at_the_last_row() {
        let mut app = app_with_many(7, 10);

        app.press(key_named("Ctrl-D"));
        app.press(key_named("Ctrl-D"));

        // At once, not only once drawn.
        assert_eq!(cursor_title(&app), Some("Notification 6"));
    }

    #[test]
    fn g_and_shift_g_go_to_the_first_and_last_row() {
        let mut app = app_with_many(30, 10);

        app.press(key_named("G"));

        // At once, not only once drawn: the Preview goes by the Notification at the cursor before that.
        assert_eq!(cursor_title(&app), Some("Notification 29"));
        let lines = screen_of(&mut app, 13);
        assert!(lines[10].contains("Notification 29"), "{lines:#?}");

        app.press(key_named("g"));

        assert_eq!(cursor_title(&app), Some("Notification 0"));
        let lines = screen_of(&mut app, 13);
        assert!(lines[1].contains("Notification 0"), "{lines:#?}");
    }

    #[test]
    fn shift_g_goes_to_the_last_row_shown() {
        let mut app = app_with_many(30, 10);
        app.set_search("1".to_owned());

        app.press(key_named("G"));

        assert_eq!(cursor_title(&app), Some("Notification 21"));
    }

    #[test]
    fn each_view_keeps_its_own_cursor() {
        let mut app = app();

        app.execute(Command::Down);
        assert_eq!(cursor_title(&app), Some("Old news"));
        app.execute(Command::ShowView(1));
        assert_eq!(cursor_title(&app), Some("Add polling"));
        app.execute(Command::ShowView(0));
        assert_eq!(cursor_title(&app), Some("Old news"));
    }

    #[test]
    fn done_in_one_view_leaves_every_view() {
        let mut app = app();
        app.execute(Command::ShowView(1));

        assert_eq!(app.execute(Command::MarkDone).len(), 1);

        let lines = screen(&mut app);
        assert!(
            lines[0].contains(" Inbox (0) │ Reviews (0) │ Read (0) "),
            "{lines:#?}"
        );
        assert!(
            lines[1].contains("Nothing in your Inbox matches this View."),
            "{lines:#?}"
        );
        app.execute(Command::ShowView(0));
        assert_eq!(titles(&screen(&mut app)), ["Old news"]);
    }

    #[test]
    fn poll_keeps_each_cursor_on_its_notification() {
        let mut app = app();
        app.execute(Command::Down);
        let mut newer = notifications();
        newer.push(Notification {
            thread: ThreadId(3),
            subject_title: "Brand new".to_owned(),
            updated_at: Utc::now() + TimeDelta::minutes(1),
            ..newer[0].clone()
        });

        app.fetched(fetched(newer));

        let inbox_view: Vec<&str> = app
            .on_screen()
            .shown(&app.views[0])
            .map(|n| n.subject_title.as_str())
            .collect();
        assert_eq!(inbox_view, ["Brand new", "Add polling", "Old news"]);
        assert_eq!(cursor_title(&app), Some("Old news"));
    }

    #[test]
    fn poll_does_not_bring_back_a_change_on_its_way_to_github() {
        let mut app = app();
        let done = app
            .execute(Command::MarkDone)
            .pop()
            .expect("the cursor is on a Notification");

        app.fetched(fetched(notifications()));
        assert_eq!(
            titles(&screen(&mut app)),
            ["Old news"],
            "GitHub hadn't applied it yet"
        );

        app.settle(Reply {
            change: done,
            accepted: Ok(()),
        });
        app.fetched(fetched(notifications().split_off(1)));
        assert_eq!(titles(&screen(&mut app)), ["Old news"]);
    }

    #[test]
    fn confirmed_change_gives_way_to_newer_activity() {
        let mut app = app();
        let read = app
            .execute(Command::MarkRead)
            .pop()
            .expect("the cursor's Notification is unread");
        app.settle(Reply {
            change: read,
            accepted: Ok(()),
        });

        // New activity on "Add polling", in a fetch that began after GitHub confirmed it read.
        app.fetched(Fetched {
            began: Instant::now() + Duration::from_secs(1),
            ..fetched(notifications())
        });

        assert!(screen(&mut app)[0].contains("Inbox (1)"), "unread again");
    }

    /// GitHub's `GET /notifications?all=true` keeps listing threads marked Done (seen on 2026-10-21): a fetch after
    /// the Done is confirmed still has the thread.
    #[test]
    fn done_stays_done_though_github_keeps_listing_it() {
        let mut app = app();
        let mut held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();
        let done = app
            .execute(Command::MarkDone)
            .pop()
            .expect("a Notification at the cursor");
        app.settle(Reply {
            change: done,
            accepted: Ok(()),
        });

        // As GitHub lists a thread marked Done: read.
        held[0].unread = false;
        app.fetched(Fetched {
            began: Instant::now() + Duration::from_secs(1),
            ..fetched(held.clone())
        });
        assert_eq!(
            titles(&screen(&mut app)),
            ["Old news"],
            "still listed, but Done"
        );

        let mut newer = held;
        newer[0].updated_at += TimeDelta::minutes(5);
        newer[0].unread = true;
        app.fetched(Fetched {
            began: Instant::now() + Duration::from_secs(2),
            ..fetched(newer)
        });
        assert_eq!(
            titles(&screen(&mut app)),
            ["Add polling", "Old news"],
            "new activity brings it back, as on GitHub"
        );
    }

    #[test]
    fn done_thread_github_lists_unread_shows_again() {
        let mut app = app();
        let held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();
        let done = app
            .execute(Command::MarkDone)
            .pop()
            .expect("a Notification at the cursor");
        app.settle(Reply {
            change: done,
            accepted: Ok(()),
        });

        // "Add polling" is unread in what GitHub lists, with no newer activity: marked unread on github.com.
        app.fetched(Fetched {
            began: Instant::now() + Duration::from_secs(1),
            ..fetched(held)
        });

        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
    }

    /// `notifications` fetched, with GitHub's data making "Gone quiet" (thread 3, read, older than any) seem Done.
    fn fetched_with_done(notifications: Vec<Notification>) -> Fetched {
        let gone_quiet = Notification {
            thread: ThreadId(3),
            subject_title: "Gone quiet".to_owned(),
            unread: false,
            // From the Notifications given, so that the same ones give the same "Gone quiet".
            updated_at: notifications[1].updated_at - TimeDelta::days(5),
            ..notifications[1].clone()
        };
        Fetched {
            began: Instant::now() + Duration::from_secs(1),
            listing: Ok(Some(Listing {
                inbox: notifications.into_iter().collect(),
                done: [gone_quiet].into_iter().collect(),
            })),
        }
    }

    /// The titles in `app`'s Inbox, newest first.
    fn inbox_titles(app: &App) -> Vec<&str> {
        app.inbox()
            .expect("fetched")
            .iter()
            .map(|n| n.subject_title.as_str())
            .collect()
    }

    /// The titles of the rows on `lines`, of any Notification.
    fn row_titles(lines: &[String]) -> Vec<&str> {
        lines[1..4]
            .iter()
            .filter_map(|line| {
                ["Brand new", "Add polling", "Old news", "Gone quiet"]
                    .into_iter()
                    .find(|title| line.contains(title))
            })
            .collect()
    }

    #[test]
    fn d_shows_the_done_list_in_every_view() {
        let mut app = app();
        app.fetched(fetched_with_done(notifications()));
        app.execute(Command::MarkDone);

        assert_eq!(row_titles(&screen(&mut app)), ["Old news"], "the Inbox");
        assert!(app.press(key_named("D")).is_none());

        let lines = screen(&mut app);
        assert_eq!(
            row_titles(&lines),
            ["Add polling", "Gone quiet"],
            "marked Done here, and seemingly Done"
        );
        assert!(
            lines[0].contains(" Done: Inbox (2) │ Reviews (1) │ Read (1) "),
            "{lines:#?}"
        );
        app.execute(Command::ShowView(2));
        assert_eq!(
            row_titles(&screen(&mut app)),
            ["Gone quiet"],
            "the Views apply"
        );

        app.press(key_named("D"));
        app.execute(Command::ShowView(0));
        assert_eq!(
            row_titles(&screen(&mut app)),
            ["Old news"],
            "the Inbox again"
        );
    }

    #[test]
    fn e_in_the_done_list_brings_back_to_ghds_inbox_only() {
        let mut app = app();
        let held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();
        app.fetched(fetched_with_done(held.clone()));
        app.press(key_named("D"));
        app.press(key_named("A"));

        let outcome = app.press(key_named("e"));

        assert!(outcome.is_none(), "nothing for GitHub");
        let lines = screen(&mut app);
        assert!(lines[1].contains("Nothing in the Done list."), "{lines:#?}");
        assert!(
            lines[4].starts_with("Back in ghd's Inbox (GitHub keeps it Done): Gone quiet"),
            "{lines:#?}"
        );
        app.press(key_named("D"));
        assert_eq!(
            inbox_titles(&app),
            ["Add polling", "Old news", "Gone quiet"]
        );

        // GitHub's data still makes "Gone quiet" seem Done: brought back, it stays in the Inbox.
        app.fetched(fetched_with_done(held));
        assert_eq!(
            inbox_titles(&app),
            ["Add polling", "Old news", "Gone quiet"]
        );
    }

    #[test]
    fn switching_lists_drops_the_selection() {
        let mut app = app();
        app.press(key_named("Space"));

        app.press(key_named("D"));

        assert!(app.selection.is_empty());
    }

    #[test]
    fn refused_done_leaves_the_done_list() {
        let mut app = app();
        let done = app
            .execute(Command::MarkDone)
            .pop()
            .expect("a Notification at the cursor");

        app.settle(Reply {
            change: done,
            accepted: refused(),
        });
        app.press(key_named("D"));

        assert!(screen(&mut app)[1].contains("Nothing in the Done list."));
    }

    #[test]
    fn done_list_states_are_looked_up_too() {
        let mut app = app();
        app.states_to_look_up();
        app.fetched(fetched_with_done(notifications()));

        let asked = app.states_to_look_up();

        assert!(
            asked.iter().any(|lookup| lookup.thread == ThreadId(3)),
            "Gone quiet, in the Done list"
        );
    }

    #[test]
    fn refused_done_is_not_hidden_again() {
        let mut app = app();
        let held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();
        let done = app
            .execute(Command::MarkDone)
            .pop()
            .expect("a Notification at the cursor");

        app.settle(Reply {
            change: done,
            accepted: refused(),
        });
        app.fetched(fetched(held));

        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
    }

    #[test]
    fn done_is_remembered_across_runs() {
        let mut app = app();
        let mut held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();
        // As GitHub lists a thread marked Done: read.
        held[0].unread = false;
        let done = app
            .execute(Command::MarkDone)
            .pop()
            .expect("a Notification at the cursor");
        app.settle(Reply {
            change: done,
            accepted: Ok(()),
        });
        let snapshot = app.into_snapshot(Utc::now()).expect("an Inbox");

        let mut next_run = app_without_inbox();
        next_run.restore(snapshot, Utc::now());
        next_run.fetched(fetched(held));

        assert_eq!(titles(&screen(&mut next_run)), ["Old news"]);
    }

    #[test]
    fn unchanged_inbox_changes_nothing() {
        let mut app = app();
        app.execute(Command::Down);

        app.fetched(Fetched {
            began: Instant::now(),
            listing: Ok(None),
        });

        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
        assert_eq!(cursor_title(&app), Some("Old news"));
    }

    #[test]
    fn failed_poll_keeps_the_inbox_until_one_succeeds() {
        let mut app = app();

        app.fetched(fetch_failed());

        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Add polling", "Old news"]);
        assert!(
            lines[4].starts_with("Couldn't refresh the Inbox, it may be out of date: timed out"),
            "{lines:#?}"
        );

        app.fetched(fetched(notifications()));
        assert_eq!(screen(&mut app)[4].trim(), "", "the failure is over");
    }

    #[test]
    fn failed_first_fetch_is_retried_by_the_next_poll() {
        let mut app = inbox_view_only();

        app.fetched(fetch_failed());
        let lines = screen(&mut app);
        assert!(
            lines[1].contains("Couldn't fetch notifications: timed out"),
            "{lines:#?}"
        );

        app.fetched(fetched(notifications()));
        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
    }

    #[test]
    fn fetch_under_way_shows_in_the_corner() {
        let mut app = app();

        assert!(!screen(&mut app)[0].contains("fetching"));
        let lines = screen_while(&mut app, true);
        assert!(lines[0].ends_with(" fetching… ┐"), "{lines:#?}");
    }

    /// Types `text` into the Search prompt.
    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            app.type_search(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
    }

    fn type_key(app: &mut App, code: KeyCode) {
        app.type_search(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn search_narrows_as_it_is_typed() {
        let mut app = app();

        app.execute(Command::Search);
        assert!(app.is_typing_search());
        type_text(&mut app, "old");

        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Old news"]);
        assert!(lines[4].starts_with("/old "), "{lines:#?}");
        assert!(!lines[3].contains("/old"), "not shown twice while typing");

        for _ in 0..3 {
            type_key(&mut app, KeyCode::Backspace);
        }
        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
    }

    #[test]
    fn keys_go_to_the_prompt_while_typing() {
        let mut app = app();
        let press = |app: &mut App, code, modifiers| app.press(KeyEvent::new(code, modifiers));
        let none = KeyModifiers::NONE;

        assert!(press(&mut app, KeyCode::Char('/'), none).is_none());
        for character in [':', 'e', 'r', '/'] {
            assert!(
                press(&mut app, KeyCode::Char(character), none).is_none(),
                "{character}"
            );
        }
        assert!(press(&mut app, KeyCode::Esc, none).is_none());

        assert!(!app.is_typing_search());
        assert_eq!(
            titles(&screen(&mut app)),
            ["Add polling", "Old news"],
            "nothing marked Done"
        );
        assert!(matches!(
            press(&mut app, KeyCode::Char('e'), none),
            Some(Outcome::Send(changes)) if matches!(changes.as_slice(), [Change::MarkDone(_)])
        ));
    }

    #[test]
    fn ctrl_c_quits_even_while_typing() {
        let mut app = app();
        app.execute(Command::Search);

        let outcome = app.press(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(matches!(outcome, Some(Outcome::Quit)));
    }

    /// Opens the command line with `:` and types `text` on it, each key press leaving nothing for `run` to do.
    fn type_command_line(app: &mut App, text: &str) {
        for character in format!(":{text}").chars() {
            let key = KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
            assert!(app.press(key).is_none(), "{character}");
        }
    }

    // Enter goes to the command line itself in these tests: pressed with it closed, it would open a browser.

    #[test]
    fn colon_q_quits_as_in_vim() {
        for command in [
            "q", "quit", "q!", "quit!", "qa", "qall", "qa!", "qall!", " q ",
        ] {
            let mut app = app();
            type_command_line(&mut app, command);

            let lines = screen(&mut app);
            assert!(lines[4].starts_with(&format!(":{command}")), "{lines:#?}");
            assert!(
                matches!(app.type_command(key_named("Enter")), Some(Outcome::Quit)),
                "{command:?}"
            );
        }
    }

    #[test]
    fn colon_q_quits_from_the_files_screen_too() {
        let mut app = app_with_files(changed_files());

        type_command_line(&mut app, "q");

        assert!(app.files.is_open());
        assert!(matches!(
            app.type_command(key_named("Enter")),
            Some(Outcome::Quit)
        ));
    }

    #[test]
    fn what_isnt_a_command_says_so() {
        let mut app = app();
        type_command_line(&mut app, "bad");

        assert!(app.type_command(key_named("Enter")).is_none());

        assert!(app.command_line.is_none());
        let lines = screen(&mut app);
        assert!(lines[4].starts_with("Not a command: bad "), "{lines:#?}");
    }

    #[test]
    fn nothing_typed_on_the_command_line_does_nothing() {
        let mut app = app();
        type_command_line(&mut app, " ");

        assert!(app.type_command(key_named("Enter")).is_none());

        assert!(app.command_line.is_none());
        assert!(app.status.is_none());
    }

    #[test]
    fn keys_go_to_the_command_line_while_typing() {
        let mut app = app();

        // Not the next Notification, nor marking Done, nor a Search, nor the help.
        type_command_line(&mut app, "je/?");

        assert_eq!(cursor_title(&app), Some("Add polling"));
        assert!(!app.is_typing_search());
        assert!(!app.help_shown);
        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Add polling", "Old news"]);
        assert!(lines[4].starts_with(":je/? "), "{lines:#?}");

        // Nor half a page down, nor a `d`.
        assert!(app.press(key_named("Ctrl-D")).is_none());

        assert_eq!(cursor_title(&app), Some("Add polling"));
        let lines = screen(&mut app);
        assert!(lines[4].starts_with(":je/? "), "{lines:#?}");
        assert!(matches!(
            app.press(key_named("Ctrl-C")),
            Some(Outcome::Quit)
        ));
    }

    #[test]
    fn esc_or_backspace_with_nothing_typed_closes_the_command_line() {
        let mut app = app();
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        type_command_line(&mut app, "q");
        assert!(app.press(key_named("Esc")).is_none());

        assert!(app.press(key_named("j")).is_none());
        assert_eq!(cursor_title(&app), Some("Old news"), "moves again");

        type_command_line(&mut app, "q");
        assert!(app.press(backspace).is_none());

        let lines = screen(&mut app);
        assert!(lines[4].starts_with(": "), "still open: {lines:#?}");

        assert!(app.press(backspace).is_none());

        assert!(app.press(key_named("k")).is_none());
        assert_eq!(cursor_title(&app), Some("Add polling"), "moves again");
    }

    #[test]
    fn enter_keeps_the_search_and_it_follows_across_views() {
        let mut app = app();
        app.execute(Command::Search);
        type_text(&mut app, "old");
        type_key(&mut app, KeyCode::Enter);

        assert!(!app.is_typing_search());
        let lines = screen(&mut app);
        assert!(lines[3].contains(" /old "), "{lines:#?}");
        assert!(
            lines[0].contains(" Inbox (0) │ Reviews (0) │ Read (0) "),
            "counts follow the search"
        );

        app.execute(Command::ShowView(2));
        assert_eq!(titles(&screen(&mut app)), ["Old news"]);
        app.execute(Command::ShowView(1));
        assert!(screen(&mut app)[1].contains("Nothing in this View matches the search."));

        app.execute(Command::Clear);
        assert_eq!(titles(&screen(&mut app)), ["Add polling"]);
        assert!(!screen(&mut app)[3].contains('/'));
    }

    #[test]
    fn esc_in_the_prompt_puts_back_the_previous_search() {
        let mut app = app();
        app.execute(Command::Search);
        type_text(&mut app, "old");
        type_key(&mut app, KeyCode::Enter);

        app.execute(Command::Search);
        type_text(&mut app, " nothing-like-this");
        assert_eq!(titles(&screen(&mut app)), Vec::<&str>::new());
        type_key(&mut app, KeyCode::Esc);

        assert!(!app.is_typing_search());
        assert_eq!(app.search_input, "old");
        assert_eq!(titles(&screen(&mut app)), ["Old news"]);
    }

    #[test]
    fn tab_goes_through_the_fields_a_word_could_start() {
        let mut app = app();
        app.execute(Command::Search);
        type_text(&mut app, "re");

        assert!(app.press(key_named("Tab")).is_none());
        assert_eq!(app.search_input, "repo:");
        assert_eq!(app.current, 0, "Tab completes rather than switching Views");
        let lines = screen(&mut app);
        assert!(lines[4].starts_with("/repo:  repo: reason: "), "{lines:#?}");

        type_key(&mut app, KeyCode::Tab);
        assert_eq!(app.search_input, "reason:");
        type_key(&mut app, KeyCode::Tab);
        assert_eq!(app.search_input, "repo:", "wraps around");
        type_key(&mut app, KeyCode::BackTab);
        assert_eq!(app.search_input, "reason:");

        type_text(&mut app, "review");
        assert!(
            !screen(&mut app)[4].contains("repo:"),
            "done going through them"
        );
        assert_eq!(titles(&screen(&mut app)), ["Add polling"]);
        type_key(&mut app, KeyCode::Tab);
        assert_eq!(
            app.search_input, "reason:review",
            "nothing to complete after a field"
        );
    }

    #[test]
    fn shift_tab_starts_from_the_last_field() {
        let mut app = app();
        app.execute(Command::Search);

        type_key(&mut app, KeyCode::BackTab);
        assert_eq!(app.search_input, "state:");
        type_key(&mut app, KeyCode::BackTab);
        assert_eq!(app.search_input, "type:");
        type_key(&mut app, KeyCode::Tab);
        assert_eq!(app.search_input, "state:");
    }

    #[test]
    fn tab_with_nothing_to_complete_changes_nothing() {
        let mut app = app();
        app.execute(Command::Search);
        type_text(&mut app, "poll");

        type_key(&mut app, KeyCode::Tab);

        assert_eq!(app.search_input, "poll");
        assert!(app.is_typing_search());
        assert_eq!(screen(&mut app)[4].trim_end(), "/poll");
    }

    #[test]
    fn cursor_stays_put_while_the_search_still_shows_it() {
        let mut app = app();
        app.execute(Command::Down);

        app.execute(Command::Search);
        type_text(&mut app, "n");

        assert_eq!(cursor_title(&app), Some("Old news"));
    }

    /// The key a help label names: a character, `Ctrl-` and a letter, `Space`, `Tab`, `Shift-Tab`, `Enter`, `Esc`, `↓` or
    /// `↑`.
    fn key_named(label: &str) -> KeyEvent {
        let (code, modifiers) = match label {
            "Space" => (KeyCode::Char(' '), KeyModifiers::NONE),
            "Tab" => (KeyCode::Tab, KeyModifiers::NONE),
            "Shift-Tab" => (KeyCode::BackTab, KeyModifiers::SHIFT),
            "Enter" => (KeyCode::Enter, KeyModifiers::NONE),
            "Esc" => (KeyCode::Esc, KeyModifiers::NONE),
            "↓" => (KeyCode::Down, KeyModifiers::NONE),
            "↑" => (KeyCode::Up, KeyModifiers::NONE),
            control if control.starts_with("Ctrl-") => {
                let letter = control["Ctrl-".len()..].to_lowercase();
                let mut characters = letter.chars();
                let (Some(letter), None) = (characters.next(), characters.next()) else {
                    panic!("no key is named {label:?}");
                };
                (KeyCode::Char(letter), KeyModifiers::CONTROL)
            }
            character => {
                let mut characters = character.chars();
                let (Some(character), None) = (characters.next(), characters.next()) else {
                    panic!("no key is named {label:?}");
                };
                (KeyCode::Char(character), KeyModifiers::NONE)
            }
        };
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn help_names_the_keys_that_work() {
        for (commands, help) in COMMAND_HELP {
            if let [Command::ShowView(_)] = commands {
                assert_eq!(help.keys, "1–9");
                assert_eq!(command_for(key_named("1")), Some(Command::ShowView(0)));
                assert_eq!(command_for(key_named("9")), Some(Command::ShowView(8)));
                continue;
            }
            for label in help.keys.split_whitespace() {
                // A command is typed after the key that opens the command line.
                let label = if label.starts_with(':') { ":" } else { label };
                let command = command_for(key_named(label)).expect("a known key");
                assert!(
                    commands.contains(&command),
                    "{label} is {command:?}, not on its line"
                );
            }
        }
    }

    #[test]
    fn help_lists_every_command() {
        // On a line for its command, named in that line's keys.
        let listed = |label: &str, command: Command| {
            COMMAND_HELP.iter().any(|(commands, help)| {
                commands
                    .iter()
                    .any(|listed| mem::discriminant(listed) == mem::discriminant(&command))
                    && help.keys.split_whitespace().any(|name| {
                        // As the help names some keys: `1` in `1–9`, and `:` with the command typed after it.
                        name.split('–').next() == Some(label)
                            || (label == ":" && name.starts_with(':'))
                    })
            })
        };
        for label in [
            "D",
            "u",
            "p",
            "J",
            "K",
            "H",
            "f",
            "w",
            "Space",
            "A",
            "j",
            "k",
            "↓",
            "↑",
            "g",
            "G",
            "Tab",
            "Shift-Tab",
            "1",
            "o",
            "Enter",
            "I",
            "e",
            "/",
            "n",
            "N",
            "Esc",
            "r",
            "?",
            ":",
            "Ctrl-C",
            "Ctrl-D",
            "Ctrl-U",
        ] {
            let command = command_for(key_named(label)).expect("a known key");
            assert!(
                listed(label, command),
                "{label} ({command:?}) isn't in the help"
            );
        }
    }

    #[test]
    fn any_key_closes_the_help() {
        let mut app = app();

        assert!(app.press(key_named("?")).is_none());
        let lines = screen(&mut app);
        assert!(lines.iter().any(|line| line.contains("Keys")), "{lines:#?}");

        assert!(
            app.press(key_named("e")).is_none(),
            "closes the help, doesn't mark Done"
        );
        assert!(!app.help_shown);
        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);

        app.press(key_named("?"));
        assert!(matches!(
            app.press(key_named("Ctrl-C")),
            Some(Outcome::Quit)
        ));
    }

    #[test]
    fn help_fits_a_normal_terminal() {
        let mut app = app();
        app.press(key_named("?"));
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("test backend never fails");

        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");

        let lines: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|line| line.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect();
        assert!(
            lines.iter().any(|line| line.contains(&format!(
                "{:16}  mark Done; in the Done list, back to the Inbox",
                "e"
            ))),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line
                    .contains(&format!("{:16}  complete a field's name", "Tab  Shift-Tab"))),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("put back the search there was before")),
            "the last line: {lines:#?}"
        );
    }

    /// The cell on line `y` of `buffer` where `text` starts.
    fn cell_at<'b>(buffer: &'b Buffer, y: u16, text: &str) -> &'b ratatui::buffer::Cell {
        let width = buffer.area.width;
        let line: Vec<&str> = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
        let wanted: Vec<String> = text.chars().map(String::from).collect();
        let x = line
            .windows(wanted.len())
            .position(|window| {
                window
                    .iter()
                    .zip(&wanted)
                    .all(|(cell, wanted)| cell == wanted)
            })
            .unwrap_or_else(|| panic!("{text:?} isn't on line {y}: {line:?}"));
        &buffer[(u16::try_from(x).expect("fits the screen"), y)]
    }

    #[test]
    fn rows_and_tabs_are_drawn_in_the_theme() {
        let rgb = |shade| Color::Rgb(shade, shade, shade);
        let theme = Theme {
            text: Style::new().bg(rgb(1)),
            repository: Style::new().fg(rgb(2)),
            read: Style::new().fg(rgb(3)),
            unread: Style::new().add_modifier(Modifier::BOLD),
            cursor: Style::new().bg(rgb(4)),
            tab_current: Style::new().fg(rgb(5)),
            kind_pull_request: Style::new().fg(rgb(6)),
            ..Theme::default()
        };
        let mut app = app_in(theme);
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, 5)).expect("test backend never fails");

        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");

        let buffer = terminal.backend().buffer();
        let unread_at_cursor = cell_at(buffer, 1, "wcgw/ghd");
        assert_eq!(unread_at_cursor.fg, rgb(2), "the column's color");
        assert_eq!(unread_at_cursor.bg, rgb(4), "the cursor");
        assert!(unread_at_cursor.modifier.contains(Modifier::BOLD), "unread");
        let read = cell_at(buffer, 2, "wcgw/other");
        assert_eq!(read.fg, rgb(3), "read rows go over the column's color");
        assert_eq!(read.bg, rgb(1), "the background");
        assert_eq!(cell_at(buffer, 1, "PR").fg, rgb(6));
        assert_eq!(cell_at(buffer, 0, "Inbox (1)").fg, rgb(5));
        assert_eq!(
            buffer[(0, 4)].bg,
            rgb(1),
            "the background reaches the status line"
        );
    }

    #[test]
    fn help_is_drawn_over_the_theme_background() {
        let theme = Theme {
            text: Style::new().bg(Color::Rgb(1, 1, 1)),
            help_keys: Style::new().fg(Color::Rgb(2, 2, 2)),
            ..Theme::default()
        };
        let mut app = app_in(theme);
        app.press(key_named("?"));
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("test backend never fails");

        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");

        let buffer = terminal.backend().buffer();
        let (y, _) = (0..24)
            .map(|y| {
                (
                    y,
                    (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>(),
                )
            })
            .find(|(_, line)| line.contains("mark Done"))
            .expect("the help lists mark Done");
        let key = cell_at(buffer, y, "e ");
        assert_eq!(key.fg, Color::Rgb(2, 2, 2));
        assert_eq!(cell_at(buffer, y, "mark Done").bg, Color::Rgb(1, 1, 1));
    }

    /// What [`app`] would keep with "Read" on screen and the cursor on "Old news" in the Inbox View, two hours before
    /// `now`.
    fn snapshot(now: DateTime<Utc>) -> Snapshot {
        Snapshot {
            listing: Listing {
                inbox: notifications().into_iter().collect(),
                done: Inbox::default(),
            },
            saved_at: now - TimeDelta::hours(2),
            view: "Read".to_owned(),
            cursors: BTreeMap::from([
                ("Inbox".to_owned(), ThreadId(2)),
                ("Read".to_owned(), ThreadId(2)),
                ("Gone".to_owned(), ThreadId(1)),
            ]),
            marks: DoneMarks::default(),
            // "Add polling" was merged, as of before its latest activity.
            states: [(
                ThreadId(1),
                Known {
                    state: Some(SubjectState::Merged),
                    as_of: now - TimeDelta::hours(3),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    /// [`app`] before its Inbox arrives.
    fn app_without_inbox() -> App {
        let mut app = app();
        app.inbox = InboxState::Loading;
        app
    }

    #[test]
    fn kept_inbox_shows_until_the_latest_arrives() {
        let now = Utc::now();
        let mut app = app_without_inbox();

        app.restore(snapshot(now), now);

        assert_eq!(app.current, 2, "the View that was on screen");
        assert_eq!(cursor_title(&app), Some("Old news"));
        app.execute(Command::ShowView(0));
        assert_eq!(cursor_title(&app), Some("Old news"), "each View's cursor");
        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Add polling", "Old news"]);
        assert!(
            lines[4].starts_with("Showing the Inbox from 2h ago until the latest arrives"),
            "{lines:#?}"
        );

        app.fetched(fetched(notifications()));
        assert_eq!(screen(&mut app)[4].trim(), "", "the latest has arrived");
        assert_eq!(cursor_title(&app), Some("Old news"));
    }

    #[test]
    fn kept_inbox_stays_when_the_first_fetch_fails() {
        let now = Utc::now();
        let mut app = app_without_inbox();
        app.restore(snapshot(now), now);

        app.fetched(fetch_failed());

        let lines = screen(&mut app);
        assert_eq!(titles(&lines), ["Old news"], "the Read View");
        assert!(
            lines[4].starts_with("Couldn't refresh the Inbox"),
            "{lines:#?}"
        );
    }

    #[test]
    fn snapshot_has_the_view_on_screen_and_each_cursor() {
        let now = Utc::now();
        let mut app = app();
        app.execute(Command::Down);
        app.execute(Command::ShowView(2));
        app.execute(Command::Search);
        type_text(&mut app, "old");

        let snapshot = app.into_snapshot(now).expect("the Inbox has arrived");

        assert_eq!(snapshot.view, "Read");
        assert_eq!(snapshot.saved_at, now);
        assert_eq!(
            snapshot.cursors,
            BTreeMap::from([
                ("Inbox".to_owned(), ThreadId(2)),
                ("Read".to_owned(), ThreadId(2))
            ]),
            "Reviews shows nothing under the search"
        );
        let threads: Vec<u64> = snapshot.listing.inbox.iter().map(|n| n.thread.0).collect();
        assert_eq!(threads, [1, 2]);
    }

    #[test]
    fn nothing_to_keep_before_an_inbox() {
        assert!(app_without_inbox().into_snapshot(Utc::now()).is_none());
    }

    fn selected_titles(app: &App) -> Vec<&str> {
        let mut titles: Vec<&str> = app
            .inbox()
            .into_iter()
            .flat_map(Inbox::iter)
            .filter(|n| app.selection.contains(&n.thread))
            .map(|n| n.subject_title.as_str())
            .collect();
        titles.sort_unstable();
        titles
    }

    #[test]
    fn space_selects_and_moves_down() {
        let mut app = app();

        app.press(key_named("Space"));
        assert_eq!(selected_titles(&app), ["Add polling"]);
        assert_eq!(cursor_title(&app), Some("Old news"), "moved down");
        app.press(key_named("Space"));
        assert_eq!(selected_titles(&app), ["Add polling", "Old news"]);
        assert_eq!(
            cursor_title(&app),
            Some("Old news"),
            "no further than the last"
        );

        app.press(key_named("k"));
        app.press(key_named("Space"));
        assert_eq!(selected_titles(&app), ["Old news"], "unselected");
    }

    #[test]
    fn a_selects_all_shown_or_unselects_them() {
        let mut app = app();
        app.execute(Command::Search);
        type_text(&mut app, "old");
        type_key(&mut app, KeyCode::Enter);

        app.press(key_named("A"));
        assert_eq!(
            selected_titles(&app),
            ["Old news"],
            "only what the search shows"
        );
        app.press(key_named("Esc"));
        app.press(key_named("Esc"));
        assert_eq!(app.search_input, "");

        app.press(key_named("Space"));
        app.press(key_named("A"));
        assert_eq!(
            selected_titles(&app),
            ["Add polling", "Old news"],
            "some were selected: all are"
        );
        app.press(key_named("A"));
        assert!(
            selected_titles(&app).is_empty(),
            "all were selected: none are"
        );
    }

    #[test]
    fn esc_clears_the_selection_before_the_search() {
        let mut app = app();
        app.execute(Command::Search);
        type_text(&mut app, "o");
        type_key(&mut app, KeyCode::Enter);
        app.press(key_named("A"));

        app.press(key_named("Esc"));
        assert_eq!(selected_titles(&app), [] as [&str; 0]);
        assert_eq!(app.search_input, "o", "the search stays");
        app.press(key_named("Esc"));
        assert_eq!(app.search_input, "");
    }

    #[test]
    fn done_acts_on_the_whole_selection_across_views() {
        let mut app = app();
        app.press(key_named("A"));
        app.execute(Command::ShowView(1));

        let changes = app.execute(Command::MarkDone);

        assert_eq!(
            changes.len(),
            2,
            "Old news isn't in Reviews, but it's selected"
        );
        assert!(
            matches!(changes.as_slice(), [Change::MarkDone(first), Change::MarkDone(second)]
            if first.thread == ThreadId(1) && second.thread == ThreadId(2))
        );
        assert!(selected_titles(&app).is_empty(), "the selection is used up");
        app.execute(Command::ShowView(0));
        let lines = screen(&mut app);
        assert!(lines[1].contains("Nothing in your Inbox."), "{lines:#?}");
        assert!(lines[4].starts_with("Marked 2 Done"), "{lines:#?}");
    }

    #[test]
    fn refused_part_of_a_batch_comes_back_alone() {
        let mut app = app();
        app.press(key_named("A"));
        let mut changes = app.execute(Command::MarkDone);
        let old_news = changes.pop().expect("two changes");
        let add_polling = changes.pop().expect("two changes");

        app.settle(Reply {
            change: add_polling,
            accepted: Ok(()),
        });
        app.settle(Reply {
            change: old_news,
            accepted: refused(),
        });

        assert_eq!(titles(&screen(&mut app)), ["Old news"]);
    }

    #[test]
    fn read_acts_on_the_unread_part_of_the_selection() {
        let mut app = app();
        app.press(key_named("A"));

        let changes = app.execute(Command::MarkRead);

        assert!(matches!(changes.as_slice(), [Change::MarkRead(n)] if n.thread == ThreadId(1)));
        assert!(screen(&mut app)[0].contains("Inbox (0)"));
        assert_eq!(selected_titles(&app), [] as [&str; 0]);
    }

    #[test]
    fn selection_drops_what_the_inbox_no_longer_holds() {
        let mut app = app();
        app.press(key_named("A"));

        app.fetched(fetched(notifications().split_off(1)));

        assert_eq!(selected_titles(&app), ["Old news"]);
        assert_eq!(app.selection.len(), 1);
    }

    #[test]
    fn selection_is_marked_and_counted() {
        let theme = Theme {
            selected: Style::new().fg(Color::Rgb(7, 7, 7)),
            ..Theme::default()
        };
        let mut app = app_in(theme);
        app.press(key_named("j"));
        app.press(key_named("Space"));
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, 5)).expect("test backend never fails");

        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");

        let buffer = terminal.backend().buffer();
        assert_eq!(cell_at(buffer, 2, "▌").fg, Color::Rgb(7, 7, 7));
        assert_eq!(
            cell_at(buffer, 2, "wcgw/other").fg,
            Color::Rgb(7, 7, 7),
            "the whole row"
        );
        assert_ne!(
            cell_at(buffer, 1, "wcgw/ghd").fg,
            Color::Rgb(7, 7, 7),
            "not the others"
        );
        assert_eq!(cell_at(buffer, 3, " 1 selected ").fg, Color::Rgb(7, 7, 7));
    }

    #[test]
    fn icons_take_the_place_of_text_labels() {
        let mut app = app();
        app.kind_labels = KindLabels::Icons;

        let lines = screen(&mut app);

        assert!(lines[1].contains("● \u{f407} wcgw/ghd"), "{lines:#?}");
        assert!(lines[2].contains("  \u{f41b} wcgw/other"), "{lines:#?}");
        assert!(!lines[1].contains("PR"), "{lines:#?}");
    }

    #[test]
    fn every_kind_has_a_label_that_fits() {
        let kinds = [
            SubjectKind::PullRequest,
            SubjectKind::Issue,
            SubjectKind::Release,
            SubjectKind::Discussion,
            SubjectKind::Commit,
            SubjectKind::CheckSuite,
            SubjectKind::Other("RepositoryAdvisory".to_owned()),
        ];
        let states = [
            None,
            Some(SubjectState::Open),
            Some(SubjectState::Draft),
            Some(SubjectState::Merged),
            Some(SubjectState::Closed),
            Some(SubjectState::Completed),
            Some(SubjectState::NotPlanned),
        ];
        for labels in [KindLabels::Text, KindLabels::Icons] {
            for kind in &kinds {
                for state in states {
                    let width = labels.label(kind, state).chars().count();
                    assert!(
                        width >= 1 && width <= labels.width().into(),
                        "{labels:?} {kind:?} {state:?}"
                    );
                }
            }
        }
    }

    fn lookup_threads(lookups: &[Lookup]) -> Vec<u64> {
        let mut threads: Vec<u64> = lookups.iter().map(|lookup| lookup.thread.0).collect();
        threads.sort_unstable();
        threads
    }

    #[test]
    fn states_are_looked_up_once_for_each_inbox() {
        let mut app = app();
        // The very Notifications the app holds: `notifications()` would give them newer timestamps.
        let held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();

        let asked = app.states_to_look_up();
        assert_eq!(
            lookup_threads(&asked),
            [1, 2],
            "a pull request and an issue"
        );
        assert!(app.states_to_look_up().is_empty(), "until the next Inbox");

        app.states_looked_up(LookedUp {
            asked,
            found: Ok(HashMap::from([(ThreadId(1), SubjectState::Merged)])),
        });
        app.fetched(fetched(held[1..].to_vec()));
        assert!(app.states_to_look_up().is_empty(), "nothing new or changed");

        let mut newer = held;
        newer[0].updated_at += TimeDelta::minutes(5);
        app.fetched(fetched(newer));
        assert_eq!(
            lookup_threads(&app.states_to_look_up()),
            [1],
            "new activity, new State maybe"
        );
    }

    #[test]
    fn states_color_the_kind_and_choose_its_icon() {
        let theme = Theme {
            kind_pull_request: Style::new().fg(Color::Rgb(1, 1, 1)),
            state_merged: Style::new().fg(Color::Rgb(8, 8, 8)),
            state_not_planned: Style::new().fg(Color::Rgb(9, 9, 9)),
            ..Theme::default()
        };
        let mut app = app_in(theme);
        app.kind_labels = KindLabels::Icons;
        let asked = app.states_to_look_up();

        app.states_looked_up(LookedUp {
            asked,
            found: Ok(HashMap::from([
                (ThreadId(1), SubjectState::Merged),
                (ThreadId(2), SubjectState::NotPlanned),
            ])),
        });
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, 5)).expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");

        let buffer = terminal.backend().buffer();
        assert_eq!(
            cell_at(buffer, 1, "\u{f419}").fg,
            Color::Rgb(8, 8, 8),
            "merged, over the kind's color"
        );
        assert_eq!(
            cell_at(buffer, 2, "\u{f517}").fg,
            Color::Rgb(9, 9, 9),
            "not planned"
        );
    }

    #[test]
    fn read_rows_keep_their_state_color_toned_down() {
        let theme = Theme {
            read: Style::new().fg(Color::Rgb(3, 3, 3)),
            state_merged: Style::new().fg(Color::Rgb(8, 8, 8)),
            state_completed: Style::new().fg(Color::Rgb(9, 9, 9)),
            read_state: Style::new().add_modifier(Modifier::DIM),
            ..Theme::default()
        };
        let mut app = app_in(theme);
        let asked = app.states_to_look_up();
        app.states_looked_up(LookedUp {
            asked,
            found: Ok(HashMap::from([
                (ThreadId(1), SubjectState::Merged),
                (ThreadId(2), SubjectState::Completed),
            ])),
        });
        let mut terminal =
            Terminal::new(TestBackend::new(SCREEN_WIDTH, 5)).expect("test backend never fails");
        terminal
            .draw(|frame| app.render(frame, false))
            .expect("test backend never fails");

        let buffer = terminal.backend().buffer();
        let unread = cell_at(buffer, 1, "PR");
        assert_eq!(unread.fg, Color::Rgb(8, 8, 8));
        assert!(!unread.modifier.contains(Modifier::DIM));
        let read = cell_at(buffer, 2, "ISS");
        assert_eq!(
            read.fg,
            Color::Rgb(9, 9, 9),
            "the State's color, not the read rows'"
        );
        assert!(read.modifier.contains(Modifier::DIM), "toned down");
        assert_eq!(
            cell_at(buffer, 2, "wcgw/other").fg,
            Color::Rgb(3, 3, 3),
            "the rest of the row is read"
        );
    }

    #[test]
    fn failed_lookup_waits_for_the_next_inbox() {
        let mut app = app();
        let asked = app.states_to_look_up();

        app.states_looked_up(LookedUp {
            asked,
            found: Err(anyhow!("timed out")),
        });

        assert!(
            screen(&mut app)[4]
                .starts_with("Couldn't look up the state of pull requests and issues: timed out")
        );
        assert!(
            app.states_to_look_up().is_empty(),
            "not straight away again"
        );
        app.fetched(fetched(notifications()));
        assert_eq!(screen(&mut app)[4].trim(), "");
        assert_eq!(lookup_threads(&app.states_to_look_up()), [1, 2]);
    }

    #[test]
    fn states_leave_with_their_notifications() {
        let mut app = app();
        let held: Vec<Notification> = app.inbox().expect("fetched").iter().cloned().collect();
        let asked = app.states_to_look_up();
        app.states_looked_up(LookedUp {
            asked,
            found: Ok(HashMap::from([
                (ThreadId(1), SubjectState::Merged),
                (ThreadId(2), SubjectState::Completed),
            ])),
        });

        app.fetched(fetched(held[1..].to_vec()));

        let kept = app.into_snapshot(Utc::now()).expect("an Inbox").states;
        assert_eq!(
            kept.iter().map(|(thread, _)| thread.0).collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn views_on_a_state_fill_in_as_states_arrive() {
        let view = |name: &str, filter: &str| View {
            name: name.to_owned(),
            filter: filter.parse().expect("valid filter"),
        };
        let mut app = App::new(
            vec![
                View {
                    name: "Inbox".to_owned(),
                    filter: Filter::default(),
                },
                view("Merged", "is:merged"),
                view("Not merged", "-is:merged"),
            ],
            Theme::default(),
            KindLabels::Text,
        );
        // "Add polling", then "Middle", then "Old news".
        let mut notifications = notifications();
        notifications.push(Notification {
            thread: ThreadId(3),
            subject_title: "Middle".to_owned(),
            updated_at: Utc::now() - TimeDelta::days(1),
            ..notifications[0].clone()
        });
        app.fetched(fetched(notifications));
        app.execute(Command::ShowView(2));
        app.execute(Command::Down);
        assert_eq!(cursor_title(&app), Some("Middle"));
        app.execute(Command::ShowView(1));
        assert!(titles(&screen(&mut app)).is_empty(), "no State known yet");

        let asked = app.states_to_look_up();
        app.states_looked_up(LookedUp {
            asked,
            found: Ok(HashMap::from([(ThreadId(1), SubjectState::Merged)])),
        });

        assert_eq!(titles(&screen(&mut app)), ["Add polling"]);
        app.execute(Command::ShowView(2));
        assert_eq!(
            cursor_title(&app),
            Some("Middle"),
            "the cursor stays on its Notification"
        );
    }

    #[test]
    fn search_on_state() {
        let mut app = app();
        let asked = app.states_to_look_up();
        app.states_looked_up(LookedUp {
            asked,
            found: Ok(HashMap::from([
                (ThreadId(1), SubjectState::Draft),
                (ThreadId(2), SubjectState::NotPlanned),
            ])),
        });
        app.execute(Command::Search);

        type_text(&mut app, "state:planned");
        assert_eq!(titles(&screen(&mut app)), ["Old news"]);
        for _ in 0.."planned".len() {
            type_key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "dra");
        assert_eq!(titles(&screen(&mut app)), ["Add polling"]);
    }

    #[test]
    fn kept_states_show_at_once() {
        let now = Utc::now();
        let mut app = app_without_inbox();
        app.kind_labels = KindLabels::Icons;

        app.restore(snapshot(now), now);
        app.execute(Command::ShowView(0));

        assert!(
            screen(&mut app)[1].contains("\u{f419} wcgw/ghd"),
            "merged, from the cache"
        );
        assert_eq!(
            app.into_snapshot(now).expect("an Inbox").states,
            snapshot(now).states
        );
    }

    #[test]
    fn nothing_to_act_on_while_loading() {
        let mut app = inbox_view_only();

        assert!(app.execute(Command::MarkDone).is_empty());
        assert!(app.execute(Command::MarkRead).is_empty());
        assert!(app.execute(Command::Open).is_empty());
    }
}
