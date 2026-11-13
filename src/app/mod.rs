//! ghd in wcgw-kit: what GitHub's notifications are, and what ghd does with them. The editor around it (buffers,
//! windows, keys, the command line, highlight groups) is the kit's; [`Ghd`] is the application it runs.
//!
//! Each View is a listed buffer, its Notifications one block each; the Preview is a window below the list showing a
//! buffer of the Notification at the list's cursor; the Files screen shows a pull request's Changed files in the
//! list's window and the file chosen below. Everything ghd asks of GitHub is an [`Io`] effect, which the runtime's
//! [`Perform`](wcgw_kit::runtime::Perform) carries out, its answer coming back as a [`Msg`].

mod commands;
mod files;
mod list;
mod preview;
#[cfg(test)]
mod tests;
mod voight;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::time::Instant;
use wcgw_kit::doc::{Cursor, Document};
use wcgw_kit::shell::{CursorLine, Segment, Side, WindowId};
use wcgw_kit::{App, BufferId};

use crate::assessment::{self, Assessments};
use crate::cache::Snapshot;
use crate::change::{Change, RecentChanges};
use crate::config::View;
use crate::done::DoneMarks;
use crate::files::{ChangedFile, FileText};
use crate::filter::Filter;
use crate::github::{FilesRequest, PreviewRequest};
use crate::inbox::{Inbox, Listing, Notification, ThreadId};
use crate::preview::{Place, Preview, Previews};
use crate::state::{Lookup, States, SubjectState};

/// The kit ghd runs in.
pub type Kit = wcgw_kit::Kit<Ghd>;

/// What ghd's code returns for the runtime to do.
pub type Effects = wcgw_kit::Effects<Ghd>;

/// One thing for the runtime to do.
pub type Effect = wcgw_kit::Effect<Msg, Io>;

/// The fewest rows the screen needs for the Preview window, or the Files screen's file: the list's row at the
/// cursor, and a few of what's read below.
pub const PREVIEW_MIN_HEIGHT: u16 = 12;

/// How long the cursor stays on a Notification before its Preview is fetched, so that moving through the list
/// fetches nothing.
pub const PREVIEW_REST: Duration = Duration::from_millis(200);

/// What text in ghd's buffers refers to: what `:open` (`<CR>`) and ghd's commands act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A Notification, in a View.
    Notification(ThreadId),
    /// A link, in a Preview or a file.
    Url(String),
    /// One of a pull request's Changed files on the Files screen, by its place in the list.
    File(ThreadId, usize),
    /// What a part of a Preview shows: the Subject's body, or one of its comments.
    Place(ThreadId, Place),
}

/// How much of the Inbox to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Whole,
    /// The whole Inbox if it changed since the last fetch, nothing otherwise.
    IfChanged,
}

/// A Preview to fetch, for the activity its Notification has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedPreview {
    pub thread: ThreadId,
    pub as_of: DateTime<Utc>,
    pub request: PreviewRequest,
}

/// What ghd's own work tells it: GitHub's answers, and times come.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// The Inbox fetched, by a fetch that began at `began`: changes GitHub confirmed later may be missing from it.
    /// `None` if it hadn't changed. Poll again `poll_after`, as GitHub asks.
    Fetched {
        began: Instant,
        listing: Result<Option<Listing>, String>,
        poll_after: Duration,
    },
    /// Time to poll the Inbox, unless polling restarted since this was asked for.
    Poll { generation: u64 },
    /// The States of the Subjects `asked` about, looked up.
    LookedUp {
        asked: Vec<Lookup>,
        found: Result<HashMap<ThreadId, SubjectState>, String>,
    },
    /// The cursor stayed on `thread` for [`PREVIEW_REST`], unless it moved since this was asked for.
    Rested { thread: ThreadId, generation: u64 },
    /// A Preview, for the thread and activity it was asked for.
    PreviewFetched {
        thread: ThreadId,
        as_of: DateTime<Utc>,
        preview: Result<Preview, String>,
    },
    /// A pull request's Changed files.
    FilesFetched {
        thread: ThreadId,
        as_of: DateTime<Utc>,
        files: Result<Vec<ChangedFile>, String>,
    },
    /// A Changed file's whole text.
    TextFetched {
        thread: ThreadId,
        contents_path: String,
        text: Result<FileText, String>,
    },
    /// GitHub's answer to change `id`.
    Settled {
        id: u64,
        change: Change,
        accepted: Result<(), String>,
    },
    /// What voight says.
    #[cfg_attr(
        not(feature = "voight"),
        allow(
            dead_code,
            reason = "built without voight, only the tests say anything"
        )
    )]
    Voight(assessment::Event),
}

/// What ghd asks its side that talks to GitHub, and voight, to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Io {
    /// Fetch the Inbox, answering with [`Msg::Fetched`].
    FetchInbox(Fetch),
    /// Look up these Subjects' States, answering with [`Msg::LookedUp`].
    LookUp(Vec<Lookup>),
    /// Fetch a Preview, answering with [`Msg::PreviewFetched`].
    FetchPreview(WantedPreview),
    /// Fetch a pull request's Changed files, answering with [`Msg::FilesFetched`].
    FetchFiles {
        thread: ThreadId,
        as_of: DateTime<Utc>,
        request: FilesRequest,
    },
    /// Fetch a Changed file's whole text, answering with [`Msg::TextFetched`].
    FetchText {
        thread: ThreadId,
        contents_path: String,
    },
    /// Send change `id` to GitHub, answering with [`Msg::Settled`]; the screen shows it made already.
    Send { id: u64, change: Change },
    /// Ask voight for the Assessment of `text`, which comes as [`Msg::Voight`].
    Assess { key: assessment::Key, text: String },
}

/// What ghd has of the Inbox so far.
#[derive(Debug)]
enum InboxState {
    Loading,
    /// The Inbox, and the Done list.
    Loaded(Listing),
    Failed(String),
}

/// A View, and its buffer.
#[derive(Debug)]
struct ShownView {
    view: View,
    buffer: BufferId,
    /// The View shows only its unread Notifications, while the Inbox is on screen.
    unread_only: bool,
    /// The Notification at its cursor, when it was last on screen.
    cursor: Option<ThreadId>,
    /// Where its cursor goes when it's first shown, as kept from last time.
    restoring: Option<ThreadId>,
    /// What its buffer was drawn for, besides what ghd knows: the width, the icons, the colorscheme.
    drawn_for: Option<(u16, String, String)>,
}

/// What ghd knows that the editor around it does not.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the Done list, States wanted, the fetches under way and the Heatmap are independent of each other: they \
              are not a state to model as an enum"
)]
#[derive(Debug)]
pub struct Ghd {
    inbox: InboxState,
    /// The Views in tab order; never empty, the built-in Inbox View first.
    views: Vec<ShownView>,
    /// Index into `views` of the View on screen.
    current: usize,
    /// The window the list is in.
    list_window: WindowId,
    /// Changes the user made that a fetched Inbox may not show yet.
    recent: RecentChanges,
    /// The filter typed with `:filter`, as typed, narrowing whichever View is on screen.
    narrowing: Option<(String, Filter)>,
    /// The Selection, by thread: only ever threads the Inbox holds, whichever View shows them.
    selection: HashSet<ThreadId>,
    /// The Subjects' States known so far.
    states: States,
    /// The user's word on which threads are Done, which GitHub's API can't say.
    marks: DoneMarks,
    /// The Done list is on screen, in place of the Inbox, in every View.
    showing_done: bool,
    /// An Inbox arrived since Subjects' States were last looked up: the ones it changed want looking up.
    states_wanted: bool,
    looking_up: bool,
    fetching: bool,
    /// Polls of an older generation are stale.
    poll_generation: u64,
    previews: Previews,
    preview: preview::Pane,
    files: files::Files,
    /// voight's Assessments, and whether it's ready to make more; `None` while voight is off.
    assessments: Option<Assessments>,
    /// voight was started, and so can be turned on again with `:voight on`.
    voight_built: bool,
    /// voight's model is loaded.
    voight_ready: bool,
    /// The text voight assessed on screen shows as its Heatmap.
    heatmap: bool,
    /// What on screen voight would assess, as the last look at the screen found.
    assessable: Option<assessment::Key>,
    /// Counts what changes how Previews and files are drawn (an Assessment, the Heatmap, the colorscheme), for their
    /// buffers to be drawn again.
    heat_version: u64,
    /// The colorscheme they were drawn in.
    scheme: String,
    /// The changes sent and not answered, by id.
    unsettled: Vec<u64>,
    next_change: u64,
}

impl Ghd {
    /// ghd waiting for the Inbox, showing `views` as buffers in that order, the first on screen, in the kit it runs
    /// in. With `voight`, voight is starting, and assesses what Previews show once its model is loaded.
    ///
    /// # Panics
    ///
    /// If `views` is empty: the configuration always has the Inbox View.
    #[cfg(test)]
    #[must_use]
    pub fn session(views: Vec<View>, voight: bool) -> (Self, Kit) {
        Self::restored(views, voight, None)
    }

    /// As [`Ghd::session`], showing `snapshot`'s Inbox, as it was then, until a fetched one arrives, on the View that
    /// was on screen at its cursor.
    ///
    /// # Panics
    ///
    /// If `views` is empty.
    #[must_use]
    pub fn restored(views: Vec<View>, voight: bool, snapshot: Option<Snapshot>) -> (Self, Kit) {
        assert!(
            !views.is_empty(),
            "the configuration always has the Inbox View"
        );
        // The Preview's buffer first, for the window to leave for the View on screen: a View shown in it for the
        // first time starts where it was kept.
        let mut kit = Kit::new("preview", Document::new(), false);
        *kit.theme_mut() = crate::hl::theme();
        *kit.keymap_mut() = crate::keymap::keymap();
        for (name, nerd, plain) in list::ICONS {
            kit.define_icon(name, nerd, plain);
        }
        commands::add(&mut kit);
        voight::install(&mut kit);
        add_segments(&mut kit);
        let placeholder = kit.current_buffer();
        let views: Vec<ShownView> = views
            .into_iter()
            .map(|view| {
                let buffer = kit.add_buffer(view.name.clone(), Document::new(), true);
                kit.buffer_mut(buffer).set_cursorline(CursorLine::Item);
                ShownView {
                    buffer,
                    view,
                    unread_only: false,
                    cursor: None,
                    restoring: None,
                    drawn_for: None,
                }
            })
            .collect();
        let mut ghd = Self {
            inbox: InboxState::Loading,
            list_window: kit.current_window(),
            current: 0,
            recent: RecentChanges::default(),
            narrowing: None,
            selection: HashSet::new(),
            states: States::default(),
            marks: DoneMarks::default(),
            showing_done: false,
            states_wanted: false,
            looking_up: false,
            fetching: false,
            poll_generation: 0,
            previews: Previews::default(),
            preview: preview::Pane::new(placeholder),
            files: files::Files::default(),
            assessments: voight.then(Assessments::default),
            voight_built: voight,
            voight_ready: false,
            heatmap: false,
            assessable: None,
            heat_version: 0,
            scheme: String::new(),
            unsettled: Vec::new(),
            next_change: 0,
            views,
        };
        kit.run_ex(&mut ghd, "set showtabline=2")
            .expect("a setting the kit has");
        if let Some(snapshot) = snapshot {
            if let Some(index) = ghd
                .views
                .iter()
                .position(|shown| shown.view.name == snapshot.view)
            {
                ghd.current = index;
            }
            ghd.restore(&mut kit, snapshot, Utc::now());
        }
        kit.show_buffer(ghd.views[ghd.current].buffer);
        ghd.render_views(&mut kit);
        (ghd, kit)
    }

    /// Shows `snapshot`'s Inbox, as it was `now`, until a fetched one arrives.
    fn restore(&mut self, kit: &mut Kit, snapshot: Snapshot, now: DateTime<Utc>) {
        self.inbox = InboxState::Loaded(snapshot.listing);
        self.states = snapshot.states;
        self.marks = snapshot.marks;
        for shown in &mut self.views {
            shown.cursor = snapshot.cursors.get(&shown.view.name).copied();
            shown.restoring = shown.cursor;
        }
        let when = match list::age(snapshot.saved_at, now).as_str() {
            "now" => "just now".to_owned(),
            age => format!("{age} ago"),
        };
        kit.info(format!(
            "Showing the Inbox from {when} until the latest arrives"
        ));
    }

    /// The changes sent to GitHub that it hasn't answered, by id: for quitting to wait for.
    pub fn unsettled(&self) -> &[u64] {
        &self.unsettled
    }

    /// What to keep for next time, as of `now`: the Inbox and the Done list, the View on screen and each View's
    /// cursor; `None` until there's an Inbox.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Option<Snapshot> {
        let InboxState::Loaded(listing) = &self.inbox else {
            return None;
        };
        Some(Snapshot {
            listing: listing.clone(),
            saved_at: now,
            states: self.states.clone(),
            marks: self.marks.clone(),
            view: self.views[self.current].view.name.clone(),
            cursors: self
                .views
                .iter()
                .filter_map(|shown| Some((shown.view.name.clone(), shown.cursor?)))
                .collect(),
        })
    }

    // --- The Inbox. ---

    /// Takes in the outcome of fetching the Inbox. A fetched Inbox replaces the one on screen, with the user's recent
    /// changes made again. A failed fetch leaves the Inbox on screen as it was, and says why.
    fn fetched(
        &mut self,
        kit: &mut Kit,
        began: Instant,
        listing: Result<Option<Listing>, String>,
        poll_after: Duration,
    ) -> Effects {
        self.fetching = false;
        match listing {
            // Unchanged since the last fetch.
            Ok(None) => {}
            Ok(Some(mut listing)) => {
                self.marks.apply(&mut listing);
                self.recent.reapply(&mut listing.inbox, began);
                self.replace_listing(listing);
            }
            Err(error) => match self.inbox {
                InboxState::Loaded(_) => kit.error(format!(
                    "Couldn't refresh the Inbox, it may be out of date: {error}"
                )),
                InboxState::Loading | InboxState::Failed(_) => {
                    self.inbox = InboxState::Failed(error);
                }
            },
        }
        self.render_views(kit);
        self.poll_generation += 1;
        let mut effects = vec![Effect::After {
            after: poll_after,
            msg: Msg::Poll {
                generation: self.poll_generation,
            },
        }];
        effects.extend(self.look_up_states());
        effects
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
        self.inbox = InboxState::Loaded(listing);
    }

    /// Fetches the Inbox, unless a fetch is under way already.
    fn fetch(&mut self, fetch: Fetch) -> Effects {
        if self.fetching {
            return Vec::new();
        }
        self.fetching = true;
        vec![Effect::App(Io::FetchInbox(fetch))]
    }

    /// The lookup of the Subjects' States the latest Inbox changed, once after it arrives, unless one is under way.
    fn look_up_states(&mut self) -> Effects {
        if self.looking_up || !self.states_wanted {
            return Vec::new();
        }
        self.states_wanted = false;
        let InboxState::Loaded(listing) = &self.inbox else {
            return Vec::new();
        };
        // The Done list's too, for when it's on screen.
        let mut stale = self.states.stale(&listing.inbox);
        stale.extend(self.states.stale(&listing.done));
        if stale.is_empty() {
            return Vec::new();
        }
        self.looking_up = true;
        vec![Effect::App(Io::LookUp(stale))]
    }

    /// Takes in Subjects' States looked up; a failed lookup is said until an Inbox is fetched again.
    fn looked_up(
        &mut self,
        kit: &mut Kit,
        asked: &[Lookup],
        found: Result<HashMap<ThreadId, SubjectState>, String>,
    ) -> Effects {
        self.looking_up = false;
        match found {
            // A View that asks for a State may show more or fewer rows now.
            Ok(found) => self.states.found(asked, &found),
            Err(error) => kit.error(format!(
                "Couldn't look up the state of pull requests and issues: {error}"
            )),
        }
        self.render_views(kit);
        self.look_up_states()
    }

    /// The list on screen: the Done list while it's shown, the Inbox otherwise; `None` until they arrive.
    fn list(&self) -> Option<&Inbox> {
        match &self.inbox {
            InboxState::Loaded(listing) if self.showing_done => Some(&listing.done),
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

    /// Whether View `index` shows unread only: never in the Done list, which GitHub lists all read.
    fn unread_only(&self, index: usize) -> bool {
        self.views[index].unread_only && !self.showing_done
    }

    /// The Notifications View `index` shows, newest first: those its Filter and the filter typed match, only the
    /// unread ones if it shows unread only.
    fn shown_in(&self, index: usize) -> impl Iterator<Item = &Notification> {
        let unread_only = self.unread_only(index);
        let view = &self.views[index].view;
        self.list()
            .into_iter()
            .flat_map(Inbox::iter)
            .filter(move |n| {
                let state = self.states.get(n);
                view.filter.matches(n, state)
                    && self
                        .narrowing
                        .as_ref()
                        .is_none_or(|(_, filter)| filter.matches(n, state))
                    && (n.unread || !unread_only)
            })
    }

    /// The Notifications the View on screen shows, with their States, as completion wants them.
    fn shown_with_states(&self) -> Vec<(&Notification, Option<SubjectState>)> {
        self.shown_in(self.current)
            .map(|n| (n, self.states.get(n)))
            .collect()
    }

    /// The Notification of `thread` in the list on screen.
    fn notification(&self, thread: ThreadId) -> Option<&Notification> {
        self.list()?.get(thread)
    }

    // --- Drawing the Views. ---

    /// Draws every View's buffer again, from what ghd knows now, and names it with how many Notifications are
    /// unread in it, or in the Done list, how many it shows.
    fn render_views(&mut self, kit: &mut Kit) {
        for index in 0..self.views.len() {
            self.render_view(kit, index);
            let name = match self.inbox {
                InboxState::Loaded(_) => {
                    let shown = self.shown_in(index);
                    let count = if self.showing_done {
                        shown.count()
                    } else {
                        shown.filter(|n| n.unread).count()
                    };
                    format!("{} ({count})", self.views[index].view.name)
                }
                InboxState::Loading | InboxState::Failed(_) => self.views[index].view.name.clone(),
            };
            kit.buffer_mut(self.views[index].buffer).set_name(name);
        }
    }

    /// What View `index`'s buffer is drawn for besides what ghd knows: the width of its window, the icons as
    /// `:set icons` draws them, the colorscheme.
    fn drawn_for(&self, kit: &Kit, index: usize) -> (u16, String, String) {
        (
            kit.text_width(self.views[index].buffer),
            kit.icon("GhdPullRequest"),
            kit.theme().name().to_owned(),
        )
    }

    /// Draws View `index`'s buffer again: its rows, or what's said in their place.
    fn render_view(&mut self, kit: &mut Kit, index: usize) {
        let buffer = self.views[index].buffer;
        let drawn_for = self.drawn_for(kit, index);
        let blocks = match &self.inbox {
            InboxState::Loading => list::message("Fetching notifications…", crate::hl::MESSAGE),
            InboxState::Failed(error) => list::message(
                &format!("Couldn't fetch notifications: {error}"),
                crate::hl::ERROR,
            ),
            InboxState::Loaded(_) => {
                let rows: Vec<list::Row<'_>> = self
                    .shown_in(index)
                    .map(|n| list::Row {
                        notification: n,
                        state: self.states.get(n),
                        selected: self.selection.contains(&n.thread),
                    })
                    .collect();
                if rows.is_empty() {
                    list::message(self.nothing_shown(index), crate::hl::MESSAGE)
                } else {
                    let icon = |name: &str| kit.icon(name);
                    let look = list::Look {
                        theme: kit.theme(),
                        icon: &icon,
                        now: Utc::now(),
                    };
                    list::blocks(&rows, drawn_for.0, &look)
                }
            }
        };
        kit.buffer_mut(buffer).doc_mut().set_blocks(blocks);
        self.views[index].drawn_for = Some(drawn_for);
    }

    /// What View `index` says when it shows nothing.
    fn nothing_shown(&self, index: usize) -> &'static str {
        let list = self.list();
        let done = self.showing_done;
        if list.is_none_or(Inbox::is_empty) {
            return if done {
                "Nothing in the Done list."
            } else {
                "Nothing in your Inbox."
            };
        }
        let view = &self.views[index].view;
        let matches_view = list
            .into_iter()
            .flat_map(Inbox::iter)
            .any(|n| view.filter.matches(n, self.states.get(n)));
        if !matches_view {
            return if done {
                "Nothing in the Done list matches this View."
            } else {
                "Nothing in your Inbox matches this View."
            };
        }
        match (self.unread_only(index), self.narrowing.is_some()) {
            (true, false) => "Nothing unread in this View.",
            (true, true) => "Nothing unread in this View matches the filter.",
            (false, _) => "Nothing in this View matches the filter.",
        }
    }

    // --- The windows, and where their cursors are. ---

    /// The Notification at the list's cursor, in the View on screen; `None` while the list shows none, or another
    /// buffer.
    fn at_cursor(&self, kit: &Kit) -> Option<ThreadId> {
        let buffer = self.views[self.current].buffer;
        if kit.window_buffer(self.list_window) != Some(buffer) {
            return None;
        }
        match kit.targets_in(self.list_window)[1] {
            Some(Target::Notification(thread)) => Some(*thread),
            _ => None,
        }
    }

    /// Puts the windows right after the user closed one ghd needs: when the list's window is gone, the one left
    /// shows the list; when the window below is, the Preview or the Files screen closes with it.
    fn repair(&mut self, kit: &mut Kit) {
        let open = kit.window_ids();
        if !open.contains(&self.list_window) {
            self.list_window = kit.current_window();
            self.preview.closed();
            self.files.closed();
            kit.show_buffer_in(self.list_window, self.views[self.current].buffer);
            return;
        }
        if let Some(window) = self.preview.window()
            && !open.contains(&window)
        {
            self.preview.closed();
        }
        if let Some(window) = self.files.window()
            && !open.contains(&window)
        {
            self.leave_files(kit);
        }
    }

    /// Brings what's on screen up to date with where the cursors are: the row drawn as the cursor's, the Preview of
    /// the Notification at it or the file chosen, and what voight assesses. Returns what to fetch for them.
    fn sync(&mut self, kit: &mut Kit) -> Effects {
        self.repair(kit);
        if kit.theme().name() != self.scheme {
            // What's drawn in the colorscheme's colours of its own is drawn again.
            self.scheme = kit.theme().name().to_owned();
            self.heat_version += 1;
        }
        let mut effects = Vec::new();
        if self.files.is_open() {
            effects.extend(self.sync_files(kit));
        } else {
            let buffer = self.views[self.current].buffer;
            if kit.window_buffer(self.list_window) == Some(buffer) {
                self.views[self.current].cursor = self.at_cursor(kit);
                // Drawn again when its window's width, the icons or the colorscheme changed.
                if self.views[self.current].drawn_for.as_ref()
                    != Some(&self.drawn_for(kit, self.current))
                {
                    self.render_view(kit, self.current);
                }
            }
            effects.extend(self.sync_preview(kit));
        }
        effects.extend(self.assess(kit));
        effects
    }

    /// A View's buffer is shown: in the list's window, it's the View on screen now.
    fn view_shown(&mut self, kit: &mut Kit, buffer: BufferId) {
        if kit.window_buffer(self.list_window) != Some(buffer) {
            return;
        }
        if let Some(index) = self.views.iter().position(|shown| shown.buffer == buffer) {
            if self.files.is_open() {
                self.leave_files(kit);
            }
            self.current = index;
            self.render_view(kit, index);
            // First shown, it goes where it was last time.
            if let Some(thread) = self.views[index].restoring.take() {
                kit.set_cursor(self.list_window, Cursor::on_block(list::key(thread)));
            }
        }
    }

    // --- Acting on Notifications. ---

    /// What an action acts on, taking it: the Selection, in the order of the list on screen, which empties it; or
    /// else the Notification at the cursor. Empty when there's nothing to act on.
    fn take_targets(&mut self, kit: &mut Kit) -> Vec<ThreadId> {
        if self.selection.is_empty() {
            return self.at_cursor(kit).into_iter().collect();
        }
        let selection = std::mem::take(&mut self.selection);
        self.list()
            .into_iter()
            .flat_map(Inbox::iter)
            .filter(|n| selection.contains(&n.thread))
            .map(|n| n.thread)
            .collect()
    }

    /// Opens what an action acts on in the browser, and marks read what was opened.
    fn browse(&mut self, kit: &mut Kit, targets: Vec<ThreadId>) -> Effects {
        let mut effects: Effects = targets
            .iter()
            .filter_map(|&thread| self.notification(thread))
            .map(|n| Effect::OpenUrl(n.subject_url.clone()))
            .collect();
        effects.extend(self.mark_read(targets));
        self.render_views(kit);
        effects
    }

    /// Clears Unread on the Notifications of `threads`. Returns a change for each that was unread, to send.
    fn mark_read(&mut self, threads: Vec<ThreadId>) -> Effects {
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

    /// Moves what an action acts on from the Inbox to the Done list, in every View; in the Done list, brings it back
    /// instead (see [`Ghd::bring_back`]).
    fn mark_done(&mut self, kit: &mut Kit) -> Effects {
        if self.showing_done {
            self.bring_back(kit);
            return Vec::new();
        }
        let targets = self.take_targets(kit);
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
                kit.info(format!("Marked Done: {}", notification.subject_title));
            }
            changes => kit.info(format!("Marked {} Done", changes.len())),
        }
        self.made(changes)
    }

    /// Moves what an action acts on from the Done list back to ghd's Inbox, and keeps it there until GitHub lists it
    /// unread or with newer activity. Only in ghd: GitHub's API can't move a thread back to the inbox, so GitHub
    /// keeps it Done.
    fn bring_back(&mut self, kit: &mut Kit) {
        let targets = self.take_targets(kit);
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
        match brought.as_slice() {
            [] => return,
            [notification] => kit.info(format!(
                "Back in ghd's Inbox (GitHub keeps it Done): {}",
                notification.subject_title
            )),
            brought => kit.info(format!(
                "{} back in ghd's Inbox (GitHub keeps them Done)",
                brought.len()
            )),
        }
        if let Some(listing) = self.listing_mut() {
            for notification in brought {
                listing.inbox.insert(notification);
            }
        }
    }

    /// Notes `changes`, made on screen, as on their way to GitHub, and has them sent.
    fn made(&mut self, changes: Vec<Change>) -> Effects {
        changes
            .into_iter()
            .map(|change| {
                self.recent.made(&change);
                let id = self.next_change;
                self.next_change += 1;
                self.unsettled.push(id);
                Effect::App(Io::Send { id, change })
            })
            .collect()
    }

    /// Takes in GitHub's answer to change `id`: a refused change is undone on screen and said.
    fn settle(&mut self, kit: &mut Kit, id: u64, change: &Change, accepted: Result<(), String>) {
        self.unsettled.retain(|&unsettled| unsettled != id);
        let Err(error) = accepted else {
            self.recent.confirmed(change, Instant::now());
            return;
        };
        self.recent.refused(change);
        kit.error(format!("GitHub refused to {change}, undone: {error}"));
        if let Change::MarkDone(n) = change {
            self.marks.forget(n.thread);
        }
        if let Some(listing) = self.listing_mut() {
            if let Change::MarkDone(n) = change {
                listing.done.remove(n.thread);
            }
            change.clone().undo(&mut listing.inbox);
        }
        self.render_views(kit);
    }

    /// Fetches the whole Inbox now, and the Preview or the files on screen again: what they show can change without
    /// notifying, like labels.
    fn refresh(&mut self, kit: &mut Kit) -> Effects {
        if self.files.is_open() {
            if let Some(thread) = self.files.forget() {
                self.forget_assessments(thread);
            }
        } else if let Some(thread) = self.at_cursor(kit)
            && self.preview.is_open()
        {
            self.previews.forget(thread);
            self.preview.forget(thread);
            self.forget_assessments(thread);
        }
        // A refresh asked for while a fetch is under way gets that fetch.
        let mut effects = self.fetch(Fetch::Whole);
        effects.extend(self.sync(kit));
        effects
    }

    fn forget_assessments(&mut self, thread: ThreadId) {
        if let Some(assessments) = &mut self.assessments {
            assessments.forget(thread);
        }
    }
}

/// `count` of `what`: `1 file`, `3 files`.
fn counted(count: u64, what: &str) -> String {
    match count {
        1 => format!("1 {what}"),
        count => format!("{count} {what}s"),
    }
}

/// ghd's statusline segments (the kit's D10): what the old borders said, in their colours, dropped least important
/// first when the line is short.
fn add_segments(kit: &mut Kit) {
    kit.add_segment(Segment::new(Side::Left, 50, |ghd: &Ghd, _| {
        ghd.showing_done.then(|| "Done list".to_owned())
    }));
    kit.add_segment(
        Segment::new(Side::Left, 40, |ghd: &Ghd, _| {
            (!ghd.selection.is_empty()).then(|| format!("{} selected", ghd.selection.len()))
        })
        .with_group(crate::hl::SELECTED),
    );
    kit.add_segment(
        Segment::new(Side::Left, 35, |ghd: &Ghd, _| {
            ghd.narrowing
                .as_ref()
                .map(|(typed, _)| format!("filter {typed}"))
        })
        .with_group(crate::hl::FILTER),
    );
    kit.add_segment(
        Segment::new(Side::Left, 30, |ghd: &Ghd, _| {
            ghd.unread_only(ghd.current)
                .then(|| "● unread only".to_owned())
        })
        .with_group(crate::hl::UNREAD_MARKER),
    );
    kit.add_segment(
        Segment::new(Side::Right, 10, |ghd: &Ghd, _| {
            (ghd.fetching || ghd.looking_up).then(|| "fetching…".to_owned())
        })
        .with_group(crate::hl::FETCHING),
    );
}

impl App for Ghd {
    type Target = Target;
    type Msg = Msg;
    type Effect = Io;

    fn update(&mut self, kit: &mut Kit, msg: Msg) -> Effects {
        let mut effects = match msg {
            Msg::Fetched {
                began,
                listing,
                poll_after,
            } => self.fetched(kit, began, listing, poll_after),
            Msg::Poll { generation } if generation == self.poll_generation => {
                self.fetch(Fetch::IfChanged)
            }
            Msg::Poll { .. } => Vec::new(),
            Msg::LookedUp { asked, found } => self.looked_up(kit, &asked, found),
            Msg::Rested { thread, generation } => {
                self.preview.rested(thread, generation);
                Vec::new()
            }
            Msg::PreviewFetched {
                thread,
                as_of,
                preview,
            } => {
                self.preview.fetched();
                self.previews.fetched(
                    thread,
                    as_of,
                    preview.map_err(|error| anyhow::anyhow!(error)),
                );
                Vec::new()
            }
            Msg::FilesFetched {
                thread,
                as_of,
                files,
            } => {
                self.files.took_list(thread, as_of, files);
                Vec::new()
            }
            Msg::TextFetched {
                thread,
                contents_path,
                text,
            } => {
                self.files.took_text(thread, &contents_path, text);
                Vec::new()
            }
            Msg::Settled {
                id,
                change,
                accepted,
            } => {
                self.settle(kit, id, &change, accepted);
                Vec::new()
            }
            Msg::Voight(event) => {
                self.voight_said(kit, event);
                Vec::new()
            }
        };
        effects.extend(self.sync(kit));
        effects
    }

    fn start(&mut self, kit: &mut Kit) -> Effects {
        let mut effects = self.fetch(Fetch::Whole);
        effects.extend(self.sync(kit));
        effects
    }

    /// What the text under the cursor refers to: a Notification opens in the browser, as the Selection does when
    /// there is one, which marks them read; a link opens in the browser; a Changed file shows below, the cursor
    /// going there.
    fn open(&mut self, kit: &mut Kit, target: Target) -> Result<Effects, String> {
        match target {
            Target::Notification(thread) => {
                let targets = if self.selection.is_empty() {
                    vec![thread]
                } else {
                    self.take_targets(kit)
                };
                Ok(self.browse(kit, targets))
            }
            Target::Url(url) => Ok(vec![Effect::OpenUrl(url)]),
            Target::File(..) => {
                if let Some(window) = self.files.window() {
                    kit.focus_window(window);
                }
                Ok(Vec::new())
            }
            Target::Place(..) => Ok(Vec::new()),
        }
    }

    /// `<C-l>`, `:e!`: the Inbox fetched again, and what the Preview or the Files screen shows.
    fn reload(&mut self, kit: &mut Kit, _buffer: BufferId) -> Option<Effects> {
        Some(self.refresh(kit))
    }

    fn shown(&mut self, kit: &mut Kit, buffer: BufferId) -> Effects {
        self.view_shown(kit, buffer);
        self.sync(kit)
    }

    /// `:bd` (`<leader>bd`): a View stays, as config.toml says which there are; ghd lists no other buffer.
    fn deleting(&mut self, _kit: &mut Kit, buffer: BufferId) -> Result<Effects, String> {
        if self.views.iter().any(|view| view.buffer == buffer) {
            return Err("A View stays: config.toml says which there are".to_owned());
        }
        Ok(Vec::new())
    }

    fn keyed(&mut self, kit: &mut Kit) -> Effects {
        self.sync(kit)
    }

    /// The list is laid out for the width again, and the Preview with it; below [`PREVIEW_MIN_HEIGHT`] rows, the
    /// Preview and the Files screen's file go until the terminal grows again.
    fn resized(&mut self, kit: &mut Kit, _width: u16, height: u16) -> Effects {
        self.render_views(kit);
        if height < PREVIEW_MIN_HEIGHT {
            self.preview.hide(kit);
        } else if !self.files.is_open() {
            self.preview.unhide(kit, self.list_window);
        }
        self.sync(kit)
    }
}
