//! The terminal UI: the Views as tabs, the one on screen listing its Notifications.

use std::mem;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use chrono::{DateTime, TimeDelta, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::future::OptionFuture;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Style, Stylize as _};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::time::Instant;

use crate::change::{Change, RecentChanges, Reply, send};
use crate::config::View;
use crate::github::GitHub;
use crate::inbox::{Inbox, Notification, SubjectKind, ThreadId};
use crate::search::{self, Search};

/// Widest the repository column gets, in terminal cells; longer names are cut off.
const MAX_REPOSITORY_WIDTH: u16 = 32;

/// Longest that quitting waits for changes still on their way to GitHub.
const QUIT_GRACE: Duration = Duration::from_secs(10);

/// Runs the UI on `terminal` until the user quits, showing `views` as tabs in that order. Meanwhile it keeps the
/// Inbox up to date from `github`, polling as often as GitHub allows, and sends it the changes the user makes.
///
/// # Errors
///
/// Fails if the terminal can't be drawn on or read from, or if changes made just before quitting were refused or
/// didn't reach GitHub in time. Failing to fetch the Inbox, or a refused change while the UI runs, isn't an error
/// here: the screen shows it, and the next poll tries again.
///
/// # Panics
///
/// If `views` is empty: the configuration always has the built-in Inbox View.
pub async fn run(
    terminal: &mut DefaultTerminal,
    github: &GitHub,
    views: Vec<View>,
) -> anyhow::Result<()> {
    let mut app = App::new(views);
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
                    Some(Outcome::Send(change)) => in_flight.push(send(github, change)),
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
        .draw(|frame| app.render(frame, false))
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
    /// Send this change to GitHub; the screen shows it made already.
    Send(Change),
}

/// What a key press can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Quit,
    /// Fetches the whole Inbox now, rather than at the next poll.
    Refresh,
    SelectNext,
    SelectPrevious,
    NextView,
    PreviousView,
    /// Shows the View at this index in tab order, counting from 0; nothing happens past the last View.
    ShowView(usize),
    /// Opens the prompt to type a Search into.
    Search,
    ClearSearch,
    /// Opens the selected Subject in the browser, which also marks it read.
    Open,
    MarkRead,
    MarkDone,
    Help,
}

/// The commands in the order the help lists them. Adding a command means adding it here too: the help test checks
/// every key the keymap knows is listed.
const HELP_ORDER: [Command; 13] = [
    Command::SelectNext,
    Command::SelectPrevious,
    Command::NextView,
    Command::PreviousView,
    Command::ShowView(0),
    Command::Open,
    Command::MarkRead,
    Command::MarkDone,
    Command::Search,
    Command::ClearSearch,
    Command::Refresh,
    Command::Help,
    Command::Quit,
];

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
            Self::SelectNext => ("j  ↓", "select the next Notification"),
            Self::SelectPrevious => ("k  ↑", "select the previous Notification"),
            Self::NextView => ("Tab", "show the next View"),
            Self::PreviousView => ("Shift-Tab", "show the previous View"),
            Self::ShowView(_) => ("1–9", "show a View by its place in the tabs"),
            Self::Open => ("o  Enter", "open in the browser, which marks it read"),
            Self::MarkRead => ("I", "mark read"),
            Self::MarkDone => ("e", "mark Done (GitHub can't undo it)"),
            Self::Search => ("/", "search the View on screen"),
            Self::ClearSearch => ("Esc", "clear the search"),
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
        KeyCode::Char('j') | KeyCode::Down => Some(Command::SelectNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Command::SelectPrevious),
        KeyCode::Tab => Some(Command::NextView),
        KeyCode::BackTab => Some(Command::PreviousView),
        KeyCode::Char(digit @ '1'..='9') => Some(Command::ShowView((digit as u8 - b'1').into())),
        KeyCode::Char('o') | KeyCode::Enter => Some(Command::Open),
        KeyCode::Char('I') => Some(Command::MarkRead),
        KeyCode::Char('e') => Some(Command::MarkDone),
        KeyCode::Char('/') => Some(Command::Search),
        KeyCode::Esc => Some(Command::ClearSearch),
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
}

/// What the app has of the Inbox so far.
enum InboxState {
    Loading,
    Loaded(Inbox),
    Failed(anyhow::Error),
}

/// A View on screen, with the user's selection in it.
struct ShownView {
    view: View,
    /// The selected row among the Notifications the View shows.
    selection: TableState,
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
    /// The help is shown over the View.
    help_shown: bool,
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
    /// The app waiting for the Inbox, with `views` as tabs in that order and the first of them on screen.
    ///
    /// # Panics
    ///
    /// If `views` is empty.
    fn new(views: Vec<View>) -> Self {
        assert!(
            !views.is_empty(),
            "the configuration always has the Inbox View"
        );
        let views = views
            .into_iter()
            .map(|view| ShownView {
                view,
                selection: TableState::new().with_selected(Some(0)),
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
            help_shown: false,
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
                if matches!(self.status, Some(StatusMessage::FetchFailed(_))) {
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

    /// Shows `inbox` instead of the Inbox on screen.
    fn replace_inbox(&mut self, inbox: Inbox) {
        self.keeping_selections(|app| app.inbox = InboxState::Loaded(inbox));
    }

    /// Makes `input` the Search.
    fn set_search(&mut self, input: String) {
        self.keeping_selections(|app| {
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

    /// Runs `change`, keeping each View's selection on the same Notification if the View still shows it.
    fn keeping_selections(&mut self, change: impl FnOnce(&mut Self)) {
        let selected: Vec<Option<ThreadId>> = self
            .views
            .iter()
            .map(|shown_view| selected_in(&self.inbox, shown_view, &self.search).map(|n| n.thread))
            .collect();
        change(self);
        for (shown_view, thread) in self.views.iter_mut().zip(selected) {
            let row = thread.and_then(|thread| {
                shown(&self.inbox, &shown_view.view, &self.search).position(|n| n.thread == thread)
            });
            if let Some(row) = row {
                shown_view.selection.select(Some(row));
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
            command => self.execute(command).map(Outcome::Send),
        }
    }

    /// Carries out `command` on screen. Returns the change to send to GitHub, if the command makes one.
    fn execute(&mut self, command: Command) -> Option<Change> {
        // Moving the selection past either end is corrected when the table is next drawn.
        let change = match command {
            // Nothing to do on screen: `press` hands these to `run`.
            Command::Quit | Command::Refresh => None,
            Command::SelectNext => {
                self.views[self.current].selection.select_next();
                None
            }
            Command::SelectPrevious => {
                self.views[self.current].selection.select_previous();
                None
            }
            Command::NextView => {
                self.current = (self.current + 1) % self.views.len();
                None
            }
            Command::PreviousView => {
                self.current = (self.current + self.views.len() - 1) % self.views.len();
                None
            }
            Command::ShowView(index) => {
                if index < self.views.len() {
                    self.current = index;
                }
                None
            }
            Command::Search => {
                self.search_prompt = Some(SearchPrompt {
                    input_before: self.search_input.clone(),
                    completion: None,
                });
                None
            }
            Command::ClearSearch => {
                self.set_search(String::new());
                None
            }
            Command::Help => {
                self.help_shown = true;
                None
            }
            Command::Open => match self
                .selected()
                .map(|n| open::that_detached(&n.subject_url))?
            {
                Ok(()) => self.mark_selected_read().map(Change::MarkRead),
                Err(error) => {
                    self.status = Some(StatusMessage::Error(format!(
                        "Couldn't open a browser: {error}"
                    )));
                    None
                }
            },
            Command::MarkRead => self.mark_selected_read().map(Change::MarkRead),
            Command::MarkDone => {
                let notification = self.take_selected()?;
                self.status = Some(StatusMessage::Info(format!(
                    "Marked Done: {}",
                    notification.subject_title
                )));
                Some(Change::MarkDone(notification))
            }
        };
        if let Some(change) = &change {
            self.recent.made(change);
        }
        change
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

    /// The selected Notification in the View on screen; `None` until the Inbox arrives, or if the View shows none.
    fn selected(&self) -> Option<&Notification> {
        selected_in(&self.inbox, &self.views[self.current], &self.search)
    }

    /// Clears Unread on the selected Notification. Returns it as it was before; `None` if it was already read.
    fn mark_selected_read(&mut self) -> Option<Notification> {
        // Cloned to keep it as it was, should the change need undoing.
        let notification = self.selected()?.clone();
        self.inbox_mut()?
            .mark_read(notification.thread)
            .then_some(notification)
    }

    /// Takes the selected Notification out of the Inbox, and so out of every View, and returns it. The selection
    /// moves to the one below.
    fn take_selected(&mut self) -> Option<Notification> {
        let thread = self.selected()?.thread;
        self.inbox_mut()?.remove(thread)
    }

    fn inbox_mut(&mut self) -> Option<&mut Inbox> {
        match &mut self.inbox {
            InboxState::Loaded(inbox) => Some(inbox),
            InboxState::Loading | InboxState::Failed(_) => None,
        }
    }

    /// Draws the app on `frame`, saying so when a fetch of the Inbox is under way (`fetching`).
    fn render(&mut self, frame: &mut Frame, fetching: bool) {
        let [view_area, status_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        self.render_status(frame, status_area);
        self.render_view(frame, view_area, fetching);
        if self.help_shown {
            render_help(frame);
        }
    }

    /// Draws the View on screen in `area`, its tabs and what's going on in its border.
    fn render_view(&mut self, frame: &mut Frame, area: Rect, fetching: bool) {
        let mut block = Block::bordered()
            .title(self.tabs())
            .title_bottom(Line::from(" ? keys ").right_aligned());
        if fetching {
            block = block.title_top(Line::from(" fetching… ").right_aligned());
        }
        if !self.search_input.is_empty() && !self.is_typing_search() {
            block = block.title_bottom(format!(" /{} ", self.search_input));
        }
        let current = &mut self.views[self.current];
        let rows: Vec<&Notification> = shown(&self.inbox, &current.view, &self.search).collect();
        let message = match &self.inbox {
            InboxState::Loading => Paragraph::new("Fetching notifications…"),
            InboxState::Failed(error) => {
                Paragraph::new(format!("Couldn't fetch notifications: {error:#}"))
                    .red()
                    .wrap(Wrap { trim: false })
            }
            InboxState::Loaded(inbox) if inbox.is_empty() => {
                Paragraph::new("Nothing in your Inbox.")
            }
            InboxState::Loaded(_)
                if rows.is_empty()
                    && shown(&self.inbox, &current.view, &Search::default())
                        .next()
                        .is_some() =>
            {
                Paragraph::new("Nothing in this View matches the search.")
            }
            InboxState::Loaded(_) if rows.is_empty() => {
                Paragraph::new("Nothing in your Inbox matches this View.")
            }
            InboxState::Loaded(_) => {
                let table = notifications_table(&rows, Utc::now()).block(block);
                frame.render_stateful_widget(table, area, &mut current.selection);
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
            let mut spans = vec![Span::raw(typed)];
            // The completed words to go through, the one in the input highlighted.
            if let Some(completion) = &prompt.completion {
                spans.push(Span::raw("  "));
                for (index, input) in completion.inputs.iter().enumerate() {
                    let word = input.rsplit(char::is_whitespace).next().unwrap_or(input);
                    let word = Span::raw(word.to_owned());
                    spans.push(if index == completion.at {
                        word.reversed()
                    } else {
                        word.dim()
                    });
                    spans.push(Span::raw(" "));
                }
            }
            frame.render_widget(Line::from(spans), area);
            return;
        }
        let status = match &self.status {
            None => return,
            Some(StatusMessage::Info(text)) => Line::from(text.as_str()),
            Some(StatusMessage::Error(text) | StatusMessage::FetchFailed(text)) => {
                Line::from(text.as_str()).red()
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
            let tab = Span::raw(label);
            spans.push(if index == self.current {
                tab.reversed()
            } else {
                tab
            });
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }
}

/// Draws the help over the middle of `frame`: every command's keys and what it does.
fn render_help(frame: &mut Frame) {
    let commands: Vec<KeyHelp> = HELP_ORDER.iter().map(|command| command.help()).collect();
    let keys_width = commands
        .iter()
        .chain(&SEARCH_PROMPT_HELP)
        .map(|help| help.keys.chars().count())
        .max()
        .unwrap_or(0);
    let line = |help: &KeyHelp| {
        Line::from(vec![
            Span::raw(format!("{:keys_width$}  ", help.keys)).bold(),
            Span::raw(help.does),
        ])
    };
    let mut lines: Vec<Line> = commands.iter().map(line).collect();
    lines.extend([Line::default(), Line::from("In the search prompt:").dim()]);
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
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Keys ")
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

/// The Notification selected in `shown_view` narrowed by `search`; `None` until the Inbox has arrived, or if
/// nothing is shown.
fn selected_in<'a>(
    inbox: &'a InboxState,
    shown_view: &'a ShownView,
    search: &'a Search,
) -> Option<&'a Notification> {
    shown(inbox, &shown_view.view, search).nth(shown_view.selection.selected()?)
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

/// One row per Notification in `notifications`: unread marker, Subject kind, repository, title, Reason, and age
/// relative to `now`.
fn notifications_table<'a>(notifications: &[&'a Notification], now: DateTime<Utc>) -> Table<'a> {
    let rows = notifications.iter().map(|n| {
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
        App::new(vec![View {
            name: "Inbox".to_owned(),
            filter: Filter::default(),
        }])
    }

    /// The app with three Views: Inbox, "Reviews" (`reason:review-requested`) and "Read" (`is:read`), the Inbox
    /// View on screen, and [`notifications`] fetched.
    fn app() -> App {
        let view = |name: &str, filter: &str| View {
            name: name.to_owned(),
            filter: filter.parse().expect("valid filter"),
        };
        let mut app = App::new(vec![
            View {
                name: "Inbox".to_owned(),
                filter: Filter::default(),
            },
            view("Reviews", "reason:review-requested"),
            view("Read", "is:read"),
        ]);
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

    fn selected_title(app: &App) -> Option<&str> {
        app.selected().map(|n| n.subject_title.as_str())
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
        assert_eq!(press(KeyCode::Char('r'), none), Some(Command::Refresh));
        assert_eq!(press(KeyCode::Char('/'), none), Some(Command::Search));
        assert_eq!(press(KeyCode::Esc, none), Some(Command::ClearSearch));
        assert_eq!(press(KeyCode::Char('?'), none), Some(Command::Help));
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
    fn each_view_keeps_its_own_selection() {
        let mut app = app();

        app.execute(Command::SelectNext);
        assert_eq!(selected_title(&app), Some("Old news"));
        app.execute(Command::ShowView(1));
        assert_eq!(selected_title(&app), Some("Add polling"));
        app.execute(Command::ShowView(0));
        assert_eq!(selected_title(&app), Some("Old news"));
    }

    #[test]
    fn done_in_one_view_leaves_every_view() {
        let mut app = app();
        app.execute(Command::ShowView(1));

        app.execute(Command::MarkDone)
            .expect("a Notification is selected");

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
    fn poll_keeps_each_selection_on_its_notification() {
        let mut app = app();
        app.execute(Command::SelectNext);
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
        assert_eq!(selected_title(&app), Some("Old news"));
    }

    #[test]
    fn poll_does_not_bring_back_a_change_on_its_way_to_github() {
        let mut app = app();
        let done = app
            .execute(Command::MarkDone)
            .expect("a Notification is selected");

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
            .expect("the selection is unread");
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
        app.execute(Command::SelectNext);

        app.fetched(Fetched {
            began: Instant::now(),
            inbox: Ok(None),
        });

        assert_eq!(titles(&screen(&mut app)), ["Add polling", "Old news"]);
        assert_eq!(selected_title(&app), Some("Old news"));
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
            Some(Outcome::Send(Change::MarkDone(_)))
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

        app.execute(Command::ClearSearch);
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
    fn selection_stays_put_while_the_search_still_shows_it() {
        let mut app = app();
        app.execute(Command::SelectNext);

        app.execute(Command::Search);
        type_text(&mut app, "n");

        assert_eq!(selected_title(&app), Some("Old news"));
    }

    /// The key a help label names: a character, or `Tab`, `Shift-Tab`, `Enter`, `Esc`, `Ctrl-C`, `↓`, `↑`.
    fn key_named(label: &str) -> KeyEvent {
        let (code, modifiers) = match label {
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

    #[test]
    fn nothing_to_act_on_while_loading() {
        let mut app = inbox_view_only();

        assert!(app.execute(Command::MarkDone).is_none());
        assert!(app.execute(Command::MarkRead).is_none());
        assert!(app.execute(Command::Open).is_none());
    }
}
