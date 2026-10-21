//! The terminal UI: the Views as tabs, the one on screen listing its Notifications.

use std::borrow::Cow;
use std::collections::HashSet;
use std::mem;
use std::time::Duration;

use anyhow::{Context as _, bail};
use chrono::{DateTime, TimeDelta, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::future::OptionFuture;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::time::Instant;

use crate::cache::{Cache, Snapshot};
use crate::change::{Change, RecentChanges, Reply, send};
use crate::config::View;
use crate::github::GitHub;
use crate::inbox::{Inbox, Notification, SubjectKind, ThreadId};
use crate::search::{self, Search};
use crate::theme::Theme;

/// Widest the repository column gets, in terminal cells; longer names are cut off.
const MAX_REPOSITORY_WIDTH: u16 = 32;

/// Longest that quitting waits for changes still on their way to GitHub.
const QUIT_GRACE: Duration = Duration::from_secs(10);

/// Runs the UI on `terminal` until the user quits, showing `views` as tabs in that order, in `theme`, naming Subject
/// kinds with `kind_labels`. Meanwhile it
/// keeps the Inbox up to date from `github`, polling as often as GitHub allows, and sends it the changes the user
/// makes. Until the first fetch arrives, it shows what `cache` kept from last time; on quitting, it keeps the Inbox
/// there for next time.
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
) -> anyhow::Result<()> {
    let mut app = App::new(views, theme, kind_labels);
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

    loop {
        terminal
            .draw(|frame| app.render(frame, fetch.is_some()))
            .context("drawing the terminal")?;

        tokio::select! {
            Some(fetched) = OptionFuture::from(fetch.as_mut()), if fetch.is_some() => {
                fetch = None;
                app.fetched(fetched);
                next_poll = Instant::now() + github.poll_interval();
            }
            () = tokio::time::sleep_until(next_poll), if fetch.is_none() => {
                fetch = Some(Box::pin(fetch_inbox(github, Fetch::IfChanged)));
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
    /// When the fetch began: changes GitHub confirmed later may be missing from `inbox`.
    began: Instant,
    /// The Inbox; `None` if it hadn't changed.
    inbox: anyhow::Result<Option<Inbox>>,
}

/// Fetches as much of the Inbox from `github` as `fetch` says.
///
/// # Cancel Safety
///
/// Cancel safe, as [`GitHub::inbox`] is.
async fn fetch_inbox(github: &GitHub, fetch: Fetch) -> Fetched {
    let began = Instant::now();
    let inbox = match fetch {
        Fetch::Whole => github.inbox().await.map(Some),
        Fetch::IfChanged => github.inbox_if_changed().await,
    };
    Fetched { began, inbox }
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

/// What a key press can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Quit,
    /// Fetches the whole Inbox now, rather than at the next poll.
    Refresh,
    Down,
    Up,
    /// Selects the Notification at the cursor, or unselects it, then moves the cursor down.
    ToggleSelected,
    /// Selects every Notification the View on screen shows, or unselects them if they're all selected.
    SelectAllShown,
    NextView,
    PreviousView,
    /// Shows the View at this index in tab order, counting from 0; nothing happens past the last View.
    ShowView(usize),
    /// Opens the prompt to type a Search into.
    Search,
    /// Clears the Selection, or the Search if nothing is selected.
    Clear,
    /// Opens the Subjects in the browser, which also marks them read.
    Open,
    MarkRead,
    MarkDone,
    Help,
}

/// The commands in the order the help lists them. Adding a command means adding it here too: the help test checks
/// every key the keymap knows is listed.
const HELP_ORDER: [Command; 15] = [
    Command::Down,
    Command::Up,
    Command::ToggleSelected,
    Command::SelectAllShown,
    Command::NextView,
    Command::PreviousView,
    Command::ShowView(0),
    Command::Open,
    Command::MarkRead,
    Command::MarkDone,
    Command::Search,
    Command::Clear,
    Command::Refresh,
    Command::Help,
    Command::Quit,
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

impl Command {
    fn help(self) -> KeyHelp {
        let (keys, does) = match self {
            Self::Down => ("j  ↓", "move to the next Notification"),
            Self::Up => ("k  ↑", "move to the previous Notification"),
            Self::ToggleSelected => ("Space", "select or unselect, and move down"),
            Self::SelectAllShown => ("A", "select all shown, or unselect them"),
            Self::NextView => ("Tab", "show the next View"),
            Self::PreviousView => ("Shift-Tab", "show the previous View"),
            Self::ShowView(_) => ("1–9", "show a View by its place in the tabs"),
            Self::Open => ("o  Enter", "open in the browser, which marks read"),
            Self::MarkRead => ("I", "mark read"),
            Self::MarkDone => ("e", "mark Done (GitHub can't undo it)"),
            Self::Search => ("/", "search the View on screen"),
            Self::Clear => ("Esc", "clear the selection, or else the search"),
            Self::Refresh => ("r", "fetch the Inbox now"),
            Self::Help => ("?", "show these keys"),
            Self::Quit => ("q  Ctrl-C", "quit"),
        };
        KeyHelp { keys, does }
    }
}

/// The command a key press stands for. Marking read and Done use GitHub's own notification shortcuts.
fn command_for(key: KeyEvent) -> Option<Command> {
    match key.code {
        _ if is_interrupt(key) => Some(Command::Quit),
        KeyCode::Char('q') => Some(Command::Quit),
        KeyCode::Char('r') => Some(Command::Refresh),
        KeyCode::Char('j') | KeyCode::Down => Some(Command::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Command::Up),
        KeyCode::Char(' ') => Some(Command::ToggleSelected),
        KeyCode::Char('A') => Some(Command::SelectAllShown),
        KeyCode::Tab => Some(Command::NextView),
        KeyCode::BackTab => Some(Command::PreviousView),
        KeyCode::Char(digit @ '1'..='9') => Some(Command::ShowView((digit as u8 - b'1').into())),
        KeyCode::Char('o') | KeyCode::Enter => Some(Command::Open),
        KeyCode::Char('I') => Some(Command::MarkRead),
        KeyCode::Char('e') => Some(Command::MarkDone),
        KeyCode::Char('/') => Some(Command::Search),
        KeyCode::Esc => Some(Command::Clear),
        KeyCode::Char('?') => Some(Command::Help),
        _ => None,
    }
}

/// Whether `key` is Ctrl-C, which quits even while the user types a Search.
fn is_interrupt(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
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
    Loaded(Inbox),
    Failed(anyhow::Error),
}

/// A View on screen, with its cursor.
struct ShownView {
    view: View,
    /// The cursor's row among the Notifications the View shows.
    cursor: TableState,
}

/// Everything on screen.
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
    /// The Selection, by thread: only ever threads the Inbox holds, whichever View shows them.
    selection: HashSet<ThreadId>,
    /// The help is shown over the View.
    help_shown: bool,
    theme: Theme,
    kind_labels: KindLabels,
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

/// Which way Tab and Shift-Tab go through the completions.
#[derive(Debug, Clone, Copy)]
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
            selection: HashSet::new(),
            help_shown: false,
            theme,
            kind_labels,
        }
    }

    /// Takes in the outcome of fetching the Inbox. A fetched Inbox replaces the one on screen, with the user's recent
    /// changes made again. A failed fetch leaves the Inbox on screen as it was, and says why.
    fn fetched(&mut self, fetched: Fetched) {
        match fetched.inbox {
            // Unchanged since the last fetch.
            Ok(None) => {}
            Ok(Some(mut inbox)) => {
                self.recent.reapply(&mut inbox, fetched.began);
                self.replace_inbox(inbox);
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
                    self.inbox = InboxState::Failed(error)
                }
            },
        }
    }

    /// Shows `inbox` instead of the Inbox on screen. What it doesn't hold any more leaves the Selection.
    fn replace_inbox(&mut self, inbox: Inbox) {
        self.selection.retain(|&thread| inbox.contains(thread));
        self.keeping_cursors(|app| app.inbox = InboxState::Loaded(inbox));
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
        let completion = match &mut prompt.completion {
            Some(completion) => {
                let count = completion.inputs.len();
                completion.at = match step {
                    Step::Next => (completion.at + 1) % count,
                    Step::Previous => (completion.at + count - 1) % count,
                };
                completion
            }
            None => {
                let inputs = search::completions(&self.search_input);
                let at = match step {
                    Step::Next => 0,
                    Step::Previous => inputs.len().saturating_sub(1),
                };
                if inputs.is_empty() {
                    return;
                }
                prompt.completion.insert(Completion { inputs, at })
            }
        };
        let input = completion.inputs[completion.at].clone();
        self.set_search(input);
    }

    /// Shows `snapshot`'s Inbox, as it was `now`, until a fetched one arrives, with the View that was on screen and
    /// each View's cursor as they were, where they still can be.
    fn restore(&mut self, snapshot: Snapshot, now: DateTime<Utc>) {
        self.inbox = InboxState::Loaded(snapshot.inbox);
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

    /// What to keep for next time, as of `now`: the Inbox, the View on screen and each View's cursor; `None`
    /// until there's an Inbox.
    fn into_snapshot(self, now: DateTime<Utc>) -> Option<Snapshot> {
        let cursors = self
            .views
            .iter()
            .filter_map(|shown_view| {
                let notification = at_cursor_in(&self.inbox, shown_view, &self.search)?;
                Some((shown_view.view.name.clone(), notification.thread))
            })
            .collect();
        let view = self.views[self.current].view.name.clone();
        let InboxState::Loaded(inbox) = self.inbox else {
            return None;
        };
        Some(Snapshot {
            inbox,
            saved_at: now,
            view,
            cursors,
        })
    }

    /// Runs `change`, keeping each View's cursor on the same Notification if the View still shows it.
    fn keeping_cursors(&mut self, change: impl FnOnce(&mut Self)) {
        let cursors = self
            .views
            .iter()
            .map(|shown_view| at_cursor_in(&self.inbox, shown_view, &self.search).map(|n| n.thread))
            .collect();
        change(self);
        self.move_cursors(cursors);
    }

    /// Selects in each View the Notification of the thread at the View's index in `threads`, if the View shows it.
    fn move_cursors(&mut self, threads: Vec<Option<ThreadId>>) {
        for (shown_view, thread) in self.views.iter_mut().zip(threads) {
            let row = thread.and_then(|thread| {
                shown(&self.inbox, &shown_view.view, &self.search).position(|n| n.thread == thread)
            });
            if let Some(row) = row {
                shown_view.cursor.select(Some(row));
            }
        }
    }

    /// Takes in a key press: while the help is shown, any key but Ctrl-C closes it; while the user types a Search,
    /// every key but Ctrl-C goes to the prompt; otherwise the key's command is carried out on screen. Returns what's
    /// left for `run` to do.
    fn press(&mut self, key: KeyEvent) -> Option<Outcome> {
        if self.help_shown && !is_interrupt(key) {
            self.help_shown = false;
            return None;
        }
        if self.is_typing_search() && !is_interrupt(key) {
            self.type_search(key);
            return None;
        }
        match command_for(key)? {
            Command::Quit => Some(Outcome::Quit),
            Command::Refresh => Some(Outcome::Refresh),
            command => {
                let changes = self.execute(command);
                (!changes.is_empty()).then_some(Outcome::Send(changes))
            }
        }
    }

    /// Carries out `command` on screen. Returns the changes to send to GitHub, if the command makes any.
    fn execute(&mut self, command: Command) -> Vec<Change> {
        // Moving the cursor past either end is corrected when the table is next drawn.
        match command {
            // Nothing to do on screen: `press` hands these to `run`.
            Command::Quit | Command::Refresh => {}
            Command::Down => self.views[self.current].cursor.select_next(),
            Command::Up => self.views[self.current].cursor.select_previous(),
            Command::ToggleSelected => {
                if let Some(thread) = self.at_cursor().map(|n| n.thread) {
                    if !self.selection.remove(&thread) {
                        self.selection.insert(thread);
                    }
                    self.views[self.current].cursor.select_next();
                }
            }
            Command::SelectAllShown => {
                let shown: Vec<ThreadId> =
                    shown(&self.inbox, &self.views[self.current].view, &self.search)
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

    /// What an action acts on, taking it: the Selection, in the Inbox's order, which empties it; or else the
    /// Notification at the cursor. Empty when there's nothing to act on.
    fn take_targets(&mut self) -> Vec<ThreadId> {
        if self.selection.is_empty() {
            return self.at_cursor().map(|n| n.thread).into_iter().collect();
        }
        let selection = mem::take(&mut self.selection);
        self.inbox()
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
                .inbox()
                .and_then(|inbox| inbox.get(thread))
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
        let Some(inbox) = self.inbox_mut() else {
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

    /// Takes what an action acts on out of the Inbox, and so out of every View. Returns a change for each; the
    /// cursor moves to the Notification below.
    fn mark_done(&mut self) -> Vec<Change> {
        let targets = self.take_targets();
        let Some(inbox) = self.inbox_mut() else {
            return Vec::new();
        };
        let changes: Vec<Change> = targets
            .into_iter()
            .filter_map(|thread| inbox.remove(thread))
            .map(Change::MarkDone)
            .collect();
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
                )))
            }
        }
        self.made(changes)
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
        if let Some(inbox) = self.inbox_mut() {
            reply.change.undo(inbox);
        }
    }

    /// The Notification at the cursor of the View on screen; `None` until the Inbox arrives, or if the View shows none.
    fn at_cursor(&self) -> Option<&Notification> {
        at_cursor_in(&self.inbox, &self.views[self.current], &self.search)
    }

    fn inbox(&self) -> Option<&Inbox> {
        match &self.inbox {
            InboxState::Loaded(inbox) => Some(inbox),
            InboxState::Loading | InboxState::Failed(_) => None,
        }
    }

    fn inbox_mut(&mut self) -> Option<&mut Inbox> {
        match &mut self.inbox {
            InboxState::Loaded(inbox) => Some(inbox),
            InboxState::Loading | InboxState::Failed(_) => None,
        }
    }

    /// Draws the app on `frame`, saying so when a fetch of the Inbox is under way (`fetching`).
    fn render(&mut self, frame: &mut Frame, fetching: bool) {
        // The theme's text style, background included, under everything else.
        frame.render_widget(Block::new().style(self.theme.text), frame.area());
        let [view_area, status_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        self.render_status(frame, status_area);
        self.render_view(frame, view_area, fetching);
        if self.help_shown {
            render_help(frame, &self.theme);
        }
    }

    /// Draws the View on screen in `area`, its tabs and what's going on in its border.
    fn render_view(&mut self, frame: &mut Frame, area: Rect, fetching: bool) {
        let theme = &self.theme;
        let mut block = Block::bordered()
            .border_style(theme.border)
            .title(self.tabs())
            .title_bottom(Line::styled(" ? keys ", theme.hint).right_aligned());
        if fetching {
            block = block.title_top(Line::styled(" fetching… ", theme.fetching).right_aligned());
        }
        if !self.selection.is_empty() {
            let count = format!(" {} selected ", self.selection.len());
            block = block.title_bottom(Line::styled(count, theme.selected));
        }
        if !self.search_input.is_empty() && !self.is_typing_search() {
            block = block.title_bottom(Line::styled(
                format!(" /{} ", self.search_input),
                theme.search,
            ));
        }
        let current = &mut self.views[self.current];
        let rows: Vec<&Notification> = shown(&self.inbox, &current.view, &self.search).collect();
        let message = match &self.inbox {
            InboxState::Loading => Paragraph::new("Fetching notifications…").style(theme.message),
            InboxState::Failed(error) => {
                Paragraph::new(format!("Couldn't fetch notifications: {error:#}"))
                    .style(theme.error)
                    .wrap(Wrap { trim: false })
            }
            InboxState::Loaded(inbox) if inbox.is_empty() => {
                Paragraph::new("Nothing in your Inbox.").style(theme.message)
            }
            InboxState::Loaded(_)
                if rows.is_empty()
                    && shown(&self.inbox, &current.view, &Search::default())
                        .next()
                        .is_some() =>
            {
                Paragraph::new("Nothing in this View matches the search.").style(theme.message)
            }
            InboxState::Loaded(_) if rows.is_empty() => {
                Paragraph::new("Nothing in your Inbox matches this View.").style(theme.message)
            }
            InboxState::Loaded(_) => {
                let table = notifications_table(
                    &rows,
                    &self.selection,
                    Utc::now(),
                    theme,
                    self.kind_labels,
                )
                .block(block);
                frame.render_stateful_widget(table, area, &mut current.cursor);
                return;
            }
        };
        frame.render_widget(message.block(block), area);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        if let Some(prompt) = &self.search_prompt {
            let typed = format!("/{}", self.search_input);
            let width = u16::try_from(typed.chars().count()).unwrap_or(u16::MAX);
            frame.set_cursor_position(Position::new(area.x.saturating_add(width), area.y));
            let mut spans = vec![Span::styled(typed, self.theme.prompt)];
            // The completed words to go through, the one in the input highlighted.
            if let Some(completion) = &prompt.completion {
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
    /// Search included. The View on screen is highlighted.
    fn tabs(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for (index, shown_view) in self.views.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" │ "));
            }
            let label = match &self.inbox {
                InboxState::Loaded(_) => {
                    let unread = shown(&self.inbox, &shown_view.view, &self.search)
                        .filter(|n| n.unread)
                        .count();
                    format!("{} ({unread})", shown_view.view.name)
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

/// Draws the help over the middle of `frame` in `theme`: every command's keys and what it does.
fn render_help(frame: &mut Frame, theme: &Theme) {
    let commands: Vec<KeyHelp> = HELP_ORDER.iter().map(|command| command.help()).collect();
    let keys_width = commands
        .iter()
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
    let mut lines: Vec<Line> = commands.iter().map(line).collect();
    lines.extend([
        Line::default(),
        Line::styled(SELECTION_HELP, theme.help_heading),
        Line::default(),
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

/// The Notification at the cursor of `shown_view` narrowed by `search`; `None` until the Inbox has arrived, or if
/// nothing is shown.
fn at_cursor_in<'a>(
    inbox: &'a InboxState,
    shown_view: &'a ShownView,
    search: &'a Search,
) -> Option<&'a Notification> {
    shown(inbox, &shown_view.view, search).nth(shown_view.cursor.selected()?)
}

/// The Notifications `view` shows narrowed by `search`, newest first; none until the Inbox has arrived.
fn shown<'a>(
    inbox: &'a InboxState,
    view: &'a View,
    search: &'a Search,
) -> impl Iterator<Item = &'a Notification> {
    let inbox = match inbox {
        InboxState::Loaded(inbox) => Some(inbox),
        InboxState::Loading | InboxState::Failed(_) => None,
    };
    inbox
        .into_iter()
        .flat_map(Inbox::iter)
        .filter(|n| view.filter.matches(n) && search.matches(n))
}

/// One row per Notification in `notifications`, in `theme`: selection marker, unread marker, Subject kind,
/// repository, title, Reason, and age relative to `now`, the kinds named with `kind_labels`. Those whose thread is in
/// `selection` are marked selected.
fn notifications_table<'a>(
    notifications: &[&'a Notification],
    selection: &HashSet<ThreadId>,
    now: DateTime<Utc>,
    theme: &Theme,
    kind_labels: KindLabels,
) -> Table<'a> {
    let rows = notifications.iter().map(|n| {
        let selected = selection.contains(&n.thread);
        // Whether it's read goes over the column's own style, so a theme can, say, grey out read rows; whether it's
        // selected goes over that.
        let mut state = if n.unread { theme.unread } else { theme.read };
        if selected {
            state = state.patch(theme.selected);
        }
        let cell = |text: Cow<'a, str>, column: Style| Cell::from(text).style(column.patch(state));
        Row::new([
            cell(
                Cow::Borrowed(if selected { "▌" } else { " " }),
                Style::new(),
            ),
            cell(
                Cow::Borrowed(if n.unread { "●" } else { " " }),
                theme.unread_marker,
            ),
            cell(
                Cow::Borrowed(kind_labels.label(&n.subject_kind)),
                theme.kind(&n.subject_kind),
            ),
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
        Constraint::Length(kind_labels.width()),
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
    /// The label for `kind`, at most [`KindLabels::width`] cells wide.
    fn label(self, kind: &SubjectKind) -> &'static str {
        // The icons' names in Nerd Fonts: oct-git_pull_request, oct-issue_opened, oct-tag, oct-comment_discussion,
        // oct-git_commit, oct-checklist and oct-question.
        match (self, kind) {
            (Self::Text, SubjectKind::PullRequest) => "PR",
            (Self::Text, SubjectKind::Issue) => "ISS",
            (Self::Text, SubjectKind::Release) => "REL",
            (Self::Text, SubjectKind::Discussion) => "DIS",
            (Self::Text, SubjectKind::Commit) => "CMT",
            (Self::Text, SubjectKind::CheckSuite) => "CI",
            (Self::Text, SubjectKind::Other(_)) => "?",
            (Self::Icons, SubjectKind::PullRequest) => "\u{f407}",
            (Self::Icons, SubjectKind::Issue) => "\u{f41b}",
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
    use crate::filter::Filter;
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
    }

    /// A fetch that has just brought `notifications`.
    fn fetched(notifications: Vec<Notification>) -> Fetched {
        Fetched {
            began: Instant::now(),
            inbox: Ok(Some(notifications.into_iter().collect())),
        }
    }

    /// A fetch that has just failed.
    fn fetch_failed() -> Fetched {
        Fetched {
            began: Instant::now(),
            inbox: Err(anyhow!("timed out")),
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
            .map(|line| line.iter().map(|cell| cell.symbol()).collect())
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
        assert_eq!(press(KeyCode::Char('q'), none), Some(Command::Quit));
        assert_eq!(press(KeyCode::Char('r'), none), Some(Command::Refresh));
        assert_eq!(press(KeyCode::Char('/'), none), Some(Command::Search));
        assert_eq!(press(KeyCode::Esc, none), Some(Command::Clear));
        assert_eq!(press(KeyCode::Char('?'), none), Some(Command::Help));
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
            None,
            "GitHub can't mark unread"
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

        let inbox_view: Vec<&str> = shown(&app.inbox, &app.views[0].view, &app.search)
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

    #[test]
    fn unchanged_inbox_changes_nothing() {
        let mut app = app();
        app.execute(Command::Down);

        app.fetched(Fetched {
            began: Instant::now(),
            inbox: Ok(None),
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
        for character in ['q', 'e', 'r', '/'] {
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
        assert_eq!(app.search_input, "type:");
        type_key(&mut app, KeyCode::BackTab);
        assert_eq!(app.search_input, "reason:");
        type_key(&mut app, KeyCode::Tab);
        assert_eq!(app.search_input, "type:");
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

    /// The key a help label names: a character, or `Space`, `Tab`, `Shift-Tab`, `Enter`, `Esc`, `Ctrl-C`, `↓`, `↑`.
    fn key_named(label: &str) -> KeyEvent {
        let (code, modifiers) = match label {
            "Space" => (KeyCode::Char(' '), KeyModifiers::NONE),
            "Tab" => (KeyCode::Tab, KeyModifiers::NONE),
            "Shift-Tab" => (KeyCode::BackTab, KeyModifiers::SHIFT),
            "Enter" => (KeyCode::Enter, KeyModifiers::NONE),
            "Esc" => (KeyCode::Esc, KeyModifiers::NONE),
            "Ctrl-C" => (KeyCode::Char('c'), KeyModifiers::CONTROL),
            "↓" => (KeyCode::Down, KeyModifiers::NONE),
            "↑" => (KeyCode::Up, KeyModifiers::NONE),
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
        for command in HELP_ORDER {
            let keys = command.help().keys;
            if let Command::ShowView(_) = command {
                assert_eq!(keys, "1–9");
                assert_eq!(command_for(key_named("1")), Some(Command::ShowView(0)));
                assert_eq!(command_for(key_named("9")), Some(Command::ShowView(8)));
                continue;
            }
            for label in keys.split_whitespace() {
                assert_eq!(command_for(key_named(label)), Some(command), "{label}");
            }
        }
    }

    #[test]
    fn help_lists_every_command() {
        let listed = |command: Command| {
            HELP_ORDER
                .iter()
                .any(|listed| mem::discriminant(listed) == mem::discriminant(&command))
        };
        for label in [
            "Space",
            "A",
            "j",
            "k",
            "↓",
            "↑",
            "Tab",
            "Shift-Tab",
            "1",
            "o",
            "Enter",
            "I",
            "e",
            "/",
            "Esc",
            "r",
            "?",
            "q",
            "Ctrl-C",
        ] {
            let command = command_for(key_named(label)).expect("a known key");
            assert!(listed(command), "{command:?} isn't in the help");
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
            .map(|line| line.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!("{:14}  mark Done (GitHub can't undo it)", "e"))),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Tab  Shift-Tab  complete a field's name")),
            "{lines:#?}"
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
            inbox: notifications().into_iter().collect(),
            saved_at: now - TimeDelta::hours(2),
            view: "Read".to_owned(),
            cursors: BTreeMap::from([
                ("Inbox".to_owned(), ThreadId(2)),
                ("Read".to_owned(), ThreadId(2)),
                ("Gone".to_owned(), ThreadId(1)),
            ]),
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
        let threads: Vec<u64> = snapshot.inbox.iter().map(|n| n.thread.0).collect();
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

        app.press(key_named("k"));
        app.press(key_named("Space"));
        assert_eq!(selected_titles(&app), ["Add polling"], "unselected");
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
        assert!(app.search_input.is_empty());

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
        assert!(selected_titles(&app).is_empty());
        assert_eq!(app.search_input, "o", "the search stays");
        app.press(key_named("Esc"));
        assert!(app.search_input.is_empty());
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
        assert!(selected_titles(&app).is_empty());
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
        for labels in [KindLabels::Text, KindLabels::Icons] {
            for kind in &kinds {
                let width = labels.label(kind).chars().count();
                assert!(
                    width >= 1 && width <= labels.width().into(),
                    "{labels:?} {kind:?}"
                );
            }
        }
    }

    #[test]
    fn nothing_to_act_on_while_loading() {
        let mut app = inbox_view_only();

        assert!(app.execute(Command::MarkDone).is_empty());
        assert!(app.execute(Command::MarkRead).is_empty());
        assert!(app.execute(Command::Open).is_empty());
    }
}
