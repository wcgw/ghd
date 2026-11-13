//! The Preview: a window below the list (the kit's D11) showing what the Notification at the list's cursor is about.
//!
//! Its buffer shows the Preview, its body and each comment a block, so that `]]` and `[[` step through the comments,
//! and the window opens on the one the Notification is about; until it's fetched, or when there's none, it says why.

use chrono::{DateTime, Utc};
use ratatui::text::Line as Drawn;
use wcgw_kit::BufferId;
use wcgw_kit::doc::{BlockKey, Content, Cursor, Line, Span};
use wcgw_kit::shell::{Placement, Size, WindowId};

use super::{
    Effect, Effects, Ghd, Kit, Msg, PREVIEW_MIN_HEIGHT, PREVIEW_REST, Target, WantedPreview,
    counted, list,
};
use crate::assessment::{self, Assessed, Window};
use crate::fetched::Fetched;
use crate::github::PreviewRequest;
use crate::heatmap::{self, Heat};
use crate::hl::{self, Group, Theme};
use crate::inbox::{Notification, ThreadId};
use crate::markdown::{self, Look};
use crate::preview::{Body, Ci, Mark, Note, Place, Preview, PullRequestSummary, ReviewDecision};
use crate::state::SubjectState;

/// The Preview's window, and its buffer.
#[derive(Debug)]
pub(super) struct Pane {
    /// The window, while it's open.
    window: Option<WindowId>,
    /// The user wants it open: it is, unless the terminal is too short.
    wanted: bool,
    buffer: BufferId,
    /// What the buffer was drawn for last.
    drawn: Option<DrawnFor>,
    /// The thread at the list's cursor, as last seen, and whether the cursor has rested on it.
    at: Option<(ThreadId, bool)>,
    /// Counts the cursor's moves: a rest asked for before the latest is stale.
    generation: u64,
    /// A Preview's fetch is under way; there's never more than one.
    fetching: bool,
}

/// What the Preview's buffer was drawn for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DrawnFor {
    /// The thread, with the activity its Preview is of, while its Preview shows; `None` for what's said in its
    /// place.
    preview: Option<(ThreadId, DateTime<Utc>)>,
    /// The width, and the Heatmap's version.
    width: u16,
    heat: u64,
}

impl Pane {
    pub(super) fn new(buffer: BufferId) -> Self {
        Self {
            window: None,
            wanted: false,
            buffer,
            drawn: None,
            at: None,
            generation: 0,
            fetching: false,
        }
    }

    /// The window, while it's open.
    pub(super) fn window(&self) -> Option<WindowId> {
        self.window
    }

    pub(super) fn is_open(&self) -> bool {
        self.window.is_some()
    }

    /// The window was closed by the user.
    pub(super) fn closed(&mut self) {
        self.window = None;
        self.wanted = false;
        self.at = None;
    }

    /// Opens the window below `list`.
    fn open(&mut self, kit: &mut Kit, list: WindowId) {
        if self.window.is_some() {
            return;
        }
        let current = kit.current_window();
        kit.focus_window(list);
        self.window = Some(kit.open_window(Placement::Below, Size::Percent(75), self.buffer));
        kit.focus_window(current);
        self.at = None;
    }

    /// Closes the window, the user still wanting it, as when the terminal is too short.
    pub(super) fn hide(&mut self, kit: &mut Kit) {
        if let Some(window) = self.window.take() {
            kit.close_window_id(window);
        }
    }

    /// Opens the window again once the terminal is tall enough, if the user wants it.
    pub(super) fn unhide(&mut self, kit: &mut Kit, list: WindowId) {
        if self.wanted && kit.size().1 >= PREVIEW_MIN_HEIGHT {
            self.open(kit, list);
        }
    }

    /// Hands the window over, to the Files screen; closing the Files screen gives it back.
    pub(super) fn lend(&mut self) -> Option<WindowId> {
        self.at = None;
        self.window.take()
    }

    /// Takes the window back from the Files screen.
    pub(super) fn take_back(&mut self, window: WindowId) {
        self.window = Some(window);
        self.at = None;
    }

    /// The cursor rested on `thread`, if it's still the rest asked for last.
    pub(super) fn rested(&mut self, thread: ThreadId, generation: u64) {
        if generation == self.generation
            && let Some((at, rested)) = &mut self.at
            && *at == thread
        {
            *rested = true;
        }
    }

    /// A Preview's fetch ended.
    pub(super) fn fetched(&mut self) {
        self.fetching = false;
    }

    /// Forgets what's drawn of `thread`, so that its Preview opens where it opens once fetched again.
    pub(super) fn forget(&mut self, thread: ThreadId) {
        if let Some((at, _)) = &mut self.at
            && *at == thread
        {
            self.at = None;
        }
        self.drawn = None;
    }
}

impl Ghd {
    /// `:preview` (`p`): opens the Preview below the list, or closes it; says why not on a terminal too short.
    pub(super) fn toggle_preview(&mut self, kit: &mut Kit) -> Result<Effects, String> {
        if self.files.is_open() {
            return Err("The Preview is for the list, not the Files screen".to_owned());
        }
        if self.preview.is_open() {
            self.preview.hide(kit);
            self.preview.wanted = false;
            return Ok(Vec::new());
        }
        if kit.size().1 < PREVIEW_MIN_HEIGHT {
            kit.info(format!(
                "The Preview needs a terminal at least {PREVIEW_MIN_HEIGHT} lines tall"
            ));
            return Ok(Vec::new());
        }
        self.preview.wanted = true;
        self.preview.open(kit, self.list_window);
        Ok(Vec::new())
    }

    /// Shows the Preview of the Notification at the list's cursor, fetching it once the cursor rests there.
    pub(super) fn sync_preview(&mut self, kit: &mut Kit) -> Effects {
        let Some(window) = self.preview.window else {
            return Vec::new();
        };
        if kit.window_buffer(window) != Some(self.preview.buffer) {
            // Given back by the Files screen.
            kit.show_buffer_in(window, self.preview.buffer);
        }
        let thread = self.views[self.current].cursor;
        let mut effects = Vec::new();
        if self.preview.at.map(|(at, _)| at) != thread {
            self.preview.generation += 1;
            self.preview.at = thread.map(|thread| (thread, false));
            if let Some(thread) = thread {
                self.previews.forget_failed(thread);
                effects.push(Effect::After {
                    after: PREVIEW_REST,
                    msg: Msg::Rested {
                        thread,
                        generation: self.preview.generation,
                    },
                });
            }
        }
        let notification = thread.and_then(|thread| self.notification(thread)).cloned();
        let Some(notification) = notification else {
            self.show_placeholder(kit, window, None);
            return effects;
        };
        if let Some(Fetched::Done(preview)) = self.previews.get(&notification) {
            let preview = preview.clone();
            self.show_preview(kit, window, &notification, &preview);
            return effects;
        }
        self.show_placeholder(kit, window, Some(&notification));
        effects.extend(self.preview_to_fetch(&notification));
        effects
    }

    /// The Preview of `notification` to fetch now: once the cursor has rested on it, unless it's fetched, being
    /// fetched, can't be, or another fetch is under way.
    fn preview_to_fetch(&mut self, notification: &Notification) -> Effects {
        let rested = self.preview.at == Some((notification.thread, true));
        if !rested || self.preview.fetching || self.previews.get(notification).is_some() {
            return Vec::new();
        }
        let Some(request) = PreviewRequest::for_notification(notification) else {
            return Vec::new();
        };
        self.previews
            .fetching(notification.thread, notification.updated_at);
        self.preview.fetching = true;
        vec![Effect::App(super::Io::FetchPreview(WantedPreview {
            thread: notification.thread,
            as_of: notification.updated_at,
            request,
        }))]
    }

    /// Shows in `window` what there is of `notification` until its Preview is fetched, or why there's none.
    fn show_placeholder(
        &mut self,
        kit: &mut Kit,
        window: WindowId,
        notification: Option<&Notification>,
    ) {
        let blocks = match notification {
            None => vec![message("Nothing to preview.", hl::MESSAGE)],
            Some(n) => {
                let (says, group) =
                    match (PreviewRequest::for_notification(n), self.previews.get(n)) {
                        (None, _) => (
                            "No Preview for this kind of Notification yet: gX opens it.".to_owned(),
                            hl::MESSAGE,
                        ),
                        (Some(_), Some(Fetched::Failed(why))) => {
                            (format!("Couldn't fetch the Preview: {why}"), hl::ERROR)
                        }
                        (Some(_), _) => ("Fetching…".to_owned(), hl::MESSAGE),
                    };
                let now = Utc::now();
                let meta = separated(
                    [
                        Some(Span::new(n.repository.clone(), hl::REPOSITORY)),
                        self.states
                            .get(n)
                            .map(|state| Span::new(state_name(state), list::state_group(state))),
                        Some(Span::new(n.reason.to_string(), hl::REASON)),
                        Some(Span::new(list::age(n.updated_at, now), hl::AGE)),
                    ]
                    .into_iter()
                    .flatten(),
                );
                let header = vec![
                    Line::Text(vec![Span::new(n.subject_title.clone(), hl::PREVIEW_TITLE)]),
                    meta,
                    Line::text(""),
                ];
                vec![
                    (BlockKey::new("header"), Content::from(header)),
                    message(&says, group),
                ]
            }
        };
        let buffer = self.preview.buffer;
        let shown = kit.buffer_mut(buffer);
        shown.set_winbar(Some(vec![(" Preview ".to_owned(), Group::WIN_BAR)]));
        shown.doc_mut().set_blocks(blocks);
        if self
            .preview
            .drawn
            .as_ref()
            .is_none_or(|drawn| drawn.preview.is_some())
        {
            kit.set_cursor(window, Cursor::new());
        }
        self.preview.drawn = Some(DrawnFor {
            preview: None,
            width: 0,
            heat: 0,
        });
    }

    /// Shows `preview`, of `notification`, in `window`: drawn again when the width or the Heatmap changed, and
    /// opened where it opens when it's another.
    fn show_preview(
        &mut self,
        kit: &mut Kit,
        window: WindowId,
        notification: &Notification,
        preview: &Preview,
    ) {
        let drawn = DrawnFor {
            preview: Some((notification.thread, notification.updated_at)),
            width: kit
                .window_text_area(window)
                .map_or(kit.size().0, |area| area.width),
            heat: self.heat_version,
        };
        if self.preview.drawn.as_ref() == Some(&drawn) {
            return;
        }
        let another = self
            .preview
            .drawn
            .as_ref()
            .is_none_or(|before| before.preview != drawn.preview);
        let blocks = self.preview_blocks(kit, notification, preview, drawn.width);
        let count = preview.comments.len() as u64;
        let winbar = vec![
            (" Preview ".to_owned(), Group::WIN_BAR),
            (
                if count == 0 {
                    String::new()
                } else {
                    format!("· {} ", counted(count, "comment"))
                },
                hl::AGE,
            ),
        ];
        let shown = kit.buffer_mut(self.preview.buffer);
        shown.set_winbar(Some(winbar));
        shown.doc_mut().set_blocks(blocks);
        if another {
            kit.set_cursor(window, Cursor::on_block(place_key(preview.opens_on)));
        }
        self.preview.drawn = Some(drawn);
    }

    /// The blocks of `preview`, of `notification`: its header, its body and each comment, each shaded by its
    /// Assessment while the Heatmap shows.
    fn preview_blocks(
        &self,
        kit: &Kit,
        notification: &Notification,
        preview: &Preview,
        width: u16,
    ) -> Vec<(BlockKey, Content<Target>)> {
        let theme = kit.theme();
        let look = Look::of(theme, heat(theme));
        let thread = notification.thread;
        let now = Utc::now();
        let mut header = header(preview, &notification.repository, now);
        header.push(Line::text(""));
        let mut blocks = vec![(
            BlockKey::new("header"),
            Content {
                lines: header,
                folds: Vec::new(),
                target: Some(Target::Notification(thread)),
            },
        )];
        let heat_at = |place| {
            self.heat_of(&assessment::Key {
                thread,
                as_of: notification.updated_at,
                part: assessment::Part::Preview(place),
            })
        };
        let mut body = Vec::new();
        if let Some(Note::BodyMentionsYou) = preview.note {
            body.push(Line::Text(vec![Span::new(
                format!(
                    "{} mentioned you in the description",
                    who(preview.author.as_ref())
                ),
                hl::AGE,
            )]));
        }
        body.extend(match &preview.body {
            Body::Markdown(text) | Body::Plain(text) if text.trim().is_empty() => {
                vec![Line::Text(vec![Span::new("No description.", hl::MESSAGE)])]
            }
            Body::Markdown(text) => {
                markdown::draw(text, &look, width, heat_at(Place::Body), link).lines
            }
            Body::Plain(text) => plain(text, heat_at(Place::Body), &look.heat),
        });
        body.push(Line::text(""));
        blocks.push((
            place_key(Place::Body),
            Content {
                lines: body,
                folds: Vec::new(),
                target: Some(Target::Place(thread, Place::Body)),
            },
        ));
        for (index, comment) in preview.comments.iter().enumerate() {
            let place = Place::Comment(index);
            let did = match comment.mark {
                None => "commented",
                Some(Mark::MentionsYou) => "mentioned you",
                Some(Mark::Answer) => "answered",
            };
            let on = comment
                .path
                .as_ref()
                .map_or_else(String::new, |path| format!(" on {path}"));
            let mut lines = vec![
                Line::Text(vec![Span::new(
                    format!(
                        "{} {did}{on} {}",
                        who(comment.author.as_ref()),
                        ago(comment.published_at, now)
                    ),
                    hl::AGE,
                )]),
                Line::text(""),
            ];
            lines.extend(markdown::draw(&comment.body, &look, width, heat_at(place), link).lines);
            lines.push(Line::text(""));
            blocks.push((
                place_key(place),
                Content {
                    lines,
                    folds: Vec::new(),
                    target: Some(Target::Place(thread, place)),
                },
            ));
        }
        blocks
    }

    /// The windows of the Assessment under `key`, while the Heatmap shows and there's one; none otherwise.
    pub(super) fn heat_of(&self, key: &assessment::Key) -> &[Window] {
        if !self.heatmap {
            return &[];
        }
        match self
            .assessments
            .as_ref()
            .and_then(|assessments| assessments.get(key))
        {
            Some(Assessed::Done(Some(assessment))) => &assessment.windows,
            _ => &[],
        }
    }

    /// What the Preview shows that voight would assess: the part of it the Preview's cursor is in.
    pub(super) fn preview_assessable(&self, kit: &Kit) -> Option<(assessment::Key, String)> {
        let window = self.preview.window?;
        let thread = self.views[self.current].cursor?;
        let notification = self.notification(thread)?;
        let Some(Fetched::Done(preview)) = self.previews.get(notification) else {
            return None;
        };
        let place = match kit.targets_in(window)[1] {
            Some(Target::Place(at, place)) if *at == thread => Some(*place),
            _ => None,
        }
        .unwrap_or(preview.opens_on);
        let text = match preview.at(place) {
            crate::preview::Shown::Comment(comment) => markdown::as_github_shows(&comment.body),
            crate::preview::Shown::Body(Body::Markdown(body)) => markdown::as_github_shows(body),
            crate::preview::Shown::Body(Body::Plain(body)) => body.clone(),
        };
        Some((
            assessment::Key {
                thread,
                as_of: notification.updated_at,
                part: assessment::Part::Preview(place),
            },
            text,
        ))
    }
}

/// The Heatmap's styles, as the colorscheme gives them.
pub(super) fn heat(theme: &Theme) -> Heat {
    Heat {
        human: theme.style(hl::HEAT_HUMAN),
        even: theme.style(hl::HEAT_EVEN),
        llm: theme.style(hl::HEAT_LLM),
    }
}

/// The key of the block showing `place`.
fn place_key(place: Place) -> BlockKey {
    match place {
        Place::Body => BlockKey::new("body"),
        Place::Comment(index) => BlockKey::new(format!("comment {index}")),
    }
}

/// A link's address, as a target.
fn link(url: &str) -> Target {
    Target::Url(url.to_owned())
}

/// A block saying `text` in `group`.
fn message(text: &str, group: Group) -> (BlockKey, Content<Target>) {
    (
        BlockKey::new("message"),
        Content::from(vec![Line::Text(vec![Span::new(text, group)])]),
    )
}

/// `login` as `@login`, or "Someone" when there's none.
fn who(login: Option<&String>) -> String {
    login.map_or_else(|| "Someone".to_owned(), |login| format!("@{login}"))
}

/// How long before `now` `then` was, as a Preview says it: "just now", "3h ago".
fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match list::age(then, now).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    }
}

/// `state`'s name, as a Preview says it.
fn state_name(state: SubjectState) -> String {
    state.name().replace('_', " ")
}

/// A Preview's header, of a Subject in `repository`, as of `now`: its number and title, where it stands, how it
/// closed, a pull request's changes and checks.
fn header(preview: &Preview, repository: &str, now: DateTime<Utc>) -> Vec<Line<Target>> {
    let discussion = preview.discussion.as_ref();
    let mut lines = vec![
        Line::Text(vec![
            Span::new(format!("{} ", preview.reference), hl::REFERENCE),
            Span::new(preview.title.clone(), hl::PREVIEW_TITLE),
        ]),
        separated(
            [
                Some(Span::new(repository.to_owned(), hl::REPOSITORY)),
                discussion.map(|discussion| Span::new(discussion.category.clone(), Group::NORMAL)),
                preview
                    .state
                    .map(|state| Span::new(state_name(state), list::state_group(state))),
                discussion
                    .filter(|discussion| discussion.answered)
                    .map(|_| Span::new("answered", Group::NORMAL)),
                preview
                    .author
                    .as_ref()
                    .map(|author| Span::new(format!("@{author}"), Group::NORMAL)),
                (!preview.labels.is_empty())
                    .then(|| Span::new(preview.labels.join(", "), hl::LABELS)),
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
            .map_or_else(String::new, |by| format!(" by @{by}"));
        let reason = match preview.state {
            Some(SubjectState::Completed) => " as completed",
            Some(SubjectState::NotPlanned) => " as not planned",
            _ => "",
        };
        lines.push(Line::Text(vec![Span::new(
            format!("{how}{by}{reason} {}", ago(closed.at, now)),
            hl::AGE,
        )]));
    }
    if let Some(summary) = &preview.pull_request {
        lines.push(pull_request_line(summary));
    }
    if let Some(Note::MentionNotLookedFor(why)) = &preview.note {
        lines.push(Line::text(""));
        lines.push(Line::Text(vec![Span::new(
            format!("Couldn't look for the mention: {why}"),
            hl::HINT,
        )]));
    }
    lines
}

/// A pull request's line in its Preview: its changes, then its checks and review decision, when known.
fn pull_request_line(summary: &PullRequestSummary) -> Line<Target> {
    let mut spans = vec![
        Span::new(format!("+{}", summary.additions), Group::ADDED),
        Span::new(" ", Group::NORMAL),
        Span::new(format!("\u{2212}{}", summary.deletions), Group::REMOVED),
    ];
    let rest = [
        Some(Span::new(
            counted(summary.changed_files, "file"),
            Group::NORMAL,
        )),
        Some(Span::new(counted(summary.commits, "commit"), Group::NORMAL)),
        summary.ci.map(|ci| match ci {
            Ci::Passing => Span::new("CI passing", hl::CI_PASSING),
            Ci::Failing => Span::new("CI failing", hl::CI_FAILING),
            Ci::Pending => Span::new("CI pending", hl::CI_PENDING),
        }),
        summary.review.map(|review| {
            Span::new(
                match review {
                    ReviewDecision::Approved => "approved",
                    ReviewDecision::ChangesRequested => "changes requested",
                    ReviewDecision::ReviewRequired => "review required",
                },
                Group::NORMAL,
            )
        }),
    ];
    for span in rest.into_iter().flatten() {
        spans.push(Span::new(" · ", Group::NORMAL));
        spans.push(span);
    }
    Line::Text(spans)
}

/// One line of `spans`, with ` · ` between them.
fn separated(spans: impl IntoIterator<Item = Span<Target>>) -> Line<Target> {
    let mut line = Vec::new();
    for (index, span) in spans.into_iter().enumerate() {
        if index > 0 {
            line.push(Span::new(" · ", Group::NORMAL));
        }
        line.push(span);
    }
    Line::Text(line)
}

/// A plain text body's lines, shaded by `windows` in `heat`'s styles.
pub(super) fn plain(text: &str, windows: &[Window], heat: &Heat) -> Vec<Line<Target>> {
    let mut lines: Vec<Drawn<'static>> = text
        .lines()
        .map(|line| Drawn::raw(line.replace('\t', "    ")))
        .collect();
    heatmap::shade(&mut lines, text, windows, heat);
    lines
        .into_iter()
        .map(|line| {
            Line::Text(
                line.spans
                    .into_iter()
                    .map(|span| {
                        Span::new(span.content.into_owned(), Group::NORMAL).with_style(span.style)
                    })
                    .collect(),
            )
        })
        .collect()
}
