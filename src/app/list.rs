//! A View's buffer: one block per Notification, its columns aligned and laid out for the window's width (`nowrap`,
//! the plan's D3), so that the cursor stays on its Notification as the list changes around it.

use chrono::{DateTime, TimeDelta, Utc};
use ratatui::style::{Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use wcgw_kit::doc::{BlockKey, Content, Line, Span};

use super::Target;
use crate::hl::{self, Group, Theme};
use crate::inbox::{Notification, SubjectKind, ThreadId};
use crate::state::SubjectState;

/// Widest the repository column gets, in cells; longer names are cut off.
const MAX_REPOSITORY_WIDTH: usize = 32;

/// The icons for kinds of Subject and their States (the kit's D9): Octicons as Nerd Fonts (version 3) have them,
/// else a letter, which any font draws in one cell. The State's colour tells a pull request's or an issue's State
/// apart where the letter can't.
pub(super) const ICONS: [(&str, &str, &str); 12] = [
    // oct-git_pull_request, _draft, oct-git_merge, oct-git_pull_request_closed.
    ("GhdPullRequest", "\u{f407}", "P"),
    ("GhdPullRequestDraft", "\u{f4dd}", "P"),
    ("GhdPullRequestMerged", "\u{f419}", "P"),
    ("GhdPullRequestClosed", "\u{f4dc}", "P"),
    // oct-issue_opened, oct-issue_closed, oct-skip.
    ("GhdIssue", "\u{f41b}", "I"),
    ("GhdIssueCompleted", "\u{f41d}", "I"),
    ("GhdIssueNotPlanned", "\u{f517}", "I"),
    // oct-tag, oct-comment_discussion, oct-git_commit, oct-checklist, oct-question.
    ("GhdRelease", "\u{f412}", "R"),
    ("GhdDiscussion", "\u{f442}", "D"),
    ("GhdCommit", "\u{f417}", "C"),
    ("GhdCheckSuite", "\u{f45e}", "S"),
    ("GhdOther", "\u{f420}", "?"),
];

/// The icon for a Subject of `kind` in `state`.
pub(super) fn icon(kind: &SubjectKind, state: Option<SubjectState>) -> &'static str {
    match kind {
        SubjectKind::PullRequest => match state {
            Some(SubjectState::Draft) => "GhdPullRequestDraft",
            Some(SubjectState::Merged) => "GhdPullRequestMerged",
            Some(SubjectState::Closed) => "GhdPullRequestClosed",
            _ => "GhdPullRequest",
        },
        SubjectKind::Issue => match state {
            Some(SubjectState::Completed) => "GhdIssueCompleted",
            Some(SubjectState::NotPlanned) => "GhdIssueNotPlanned",
            _ => "GhdIssue",
        },
        SubjectKind::Release => "GhdRelease",
        SubjectKind::Discussion => "GhdDiscussion",
        SubjectKind::Commit => "GhdCommit",
        SubjectKind::CheckSuite => "GhdCheckSuite",
        SubjectKind::Other(_) => "GhdOther",
    }
}

/// The group of Subjects of `kind`.
pub(super) fn kind_group(kind: &SubjectKind) -> Group {
    match kind {
        SubjectKind::PullRequest => hl::KIND_PULL_REQUEST,
        SubjectKind::Issue => hl::KIND_ISSUE,
        SubjectKind::Release => hl::KIND_RELEASE,
        SubjectKind::Discussion => hl::KIND_DISCUSSION,
        SubjectKind::Commit => hl::KIND_COMMIT,
        SubjectKind::CheckSuite => hl::KIND_CHECK_SUITE,
        SubjectKind::Other(_) => hl::KIND_OTHER,
    }
}

/// The group of Subjects in `state`.
pub(super) fn state_group(state: SubjectState) -> Group {
    match state {
        SubjectState::Open => hl::STATE_OPEN,
        SubjectState::Draft => hl::STATE_DRAFT,
        SubjectState::Merged => hl::STATE_MERGED,
        SubjectState::Closed => hl::STATE_CLOSED,
        SubjectState::Completed => hl::STATE_COMPLETED,
        SubjectState::NotPlanned => hl::STATE_NOT_PLANNED,
    }
}

/// The key of the block showing `thread`.
pub(super) fn key(thread: ThreadId) -> BlockKey {
    BlockKey::new(format!("n{}", thread.0))
}

/// A Notification as its row shows it.
pub(super) struct Row<'a> {
    pub(super) notification: &'a Notification,
    pub(super) state: Option<SubjectState>,
    pub(super) selected: bool,
    /// The cursor is on it: its row is drawn as `CursorLine`.
    pub(super) cursor: bool,
}

/// What draws the rows: the colorscheme's styles, the icons as `:set icons` draws them, and the time it is.
pub(super) struct Look<'a> {
    pub(super) theme: &'a Theme,
    pub(super) icon: &'a dyn Fn(&str) -> String,
    pub(super) now: DateTime<Utc>,
}

/// One block per row of `rows`, each a line `width` cells wide: the selection's `▌`, the unread `●`, the kind's icon
/// in its State's colour, the repository, the title, the Reason and the age, the columns aligned across rows.
pub(super) fn blocks(
    rows: &[Row<'_>],
    width: u16,
    look: &Look<'_>,
) -> Vec<(BlockKey, Content<Target>)> {
    let repository_width = rows
        .iter()
        .map(|row| row.notification.repository.width())
        .max()
        .unwrap_or(0)
        .min(MAX_REPOSITORY_WIDTH);
    let reason_width = rows
        .iter()
        .map(|row| row.notification.reason.to_string().width())
        .max()
        .unwrap_or(0);
    // `▌● ` and the icon, then a space after the repository, the title and the Reason, then the age.
    let fixed = 3 + 2 + repository_width + 1 + 1 + reason_width + 1 + 3;
    let title_width = usize::from(width).saturating_sub(fixed);
    rows.iter()
        .map(|row| {
            let line = row_line(row, repository_width, title_width, reason_width, look);
            let content = Content {
                lines: vec![clip(line, usize::from(width))],
                folds: Vec::new(),
                target: Some(Target::Notification(row.notification.thread)),
            };
            (key(row.notification.thread), content)
        })
        .collect()
}

/// The line of `row`, its columns these widths.
fn row_line(
    row: &Row<'_>,
    repository_width: usize,
    title_width: usize,
    reason_width: usize,
    look: &Look<'_>,
) -> Vec<Span<Target>> {
    let theme = look.theme;
    let n = row.notification;
    // Whether it's read goes over each column's own group, so that a colorscheme can, say, grey out read rows;
    // whether it's selected goes over that, and the cursor over all.
    let mut row_style = theme.style(if n.unread { hl::UNREAD } else { hl::READ });
    if row.selected {
        row_style = row_style.patch(theme.style(hl::SELECTED));
    }
    if row.cursor {
        row_style = row_style.patch(theme.style(Group::CURSOR_LINE));
    }
    let span = |text: String, group: Group| Span::new(text, group).with_style(row_style);
    // A State's colour goes over the row's, so that a read Subject still shows it, dimmed.
    let (kind_group, mut kind_style) = match row.state {
        Some(state) => (
            state_group(state),
            row_style.patch(theme.style(state_group(state))),
        ),
        None => (self::kind_group(&n.subject_kind), row_style),
    };
    if !n.unread && row.state.is_some() {
        kind_style = kind_style.add_modifier(Modifier::DIM);
    }
    vec![
        span(
            if row.selected { "▌" } else { " " }.to_owned(),
            hl::SELECTED,
        ),
        span(
            if n.unread { "●" } else { " " }.to_owned(),
            hl::UNREAD_MARKER,
        ),
        span(" ".to_owned(), Group::NORMAL),
        Span::new((look.icon)(icon(&n.subject_kind, row.state)), kind_group).with_style(kind_style),
        span(fit(&n.repository, repository_width), hl::REPOSITORY),
        span(" ".to_owned(), Group::NORMAL),
        span(fit(&n.subject_title, title_width), hl::TITLE),
        span(" ".to_owned(), Group::NORMAL),
        span(fit(&n.reason.to_string(), reason_width), hl::REASON),
        span(" ".to_owned(), Group::NORMAL),
        span(format!("{:>3}", age(n.updated_at, look.now)), hl::AGE),
    ]
}

/// A block saying `text` in `group`, in place of the rows.
pub(super) fn message(text: &str, group: Group) -> Vec<(BlockKey, Content<Target>)> {
    vec![(
        BlockKey::new("message"),
        Content::from(vec![Line::Text(vec![Span::new(text, group)])]),
    )]
}

/// `text` in exactly `width` cells: cut with `…` if it's wider, padded with spaces if it's narrower.
pub(super) fn fit(text: &str, width: usize) -> String {
    let mut fitted = String::with_capacity(width);
    let mut used = 0;
    if text.width() > width {
        for c in text.chars() {
            let cells = c.width().unwrap_or(0);
            if used + cells + 1 > width {
                break;
            }
            fitted.push(c);
            used += cells;
        }
        if width > 0 {
            fitted.push('…');
            used += 1;
        }
    } else {
        fitted.push_str(text);
        used = text.width();
    }
    fitted.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    fitted
}

/// `spans` as one line no wider than `width` cells, so that it never wraps.
pub(super) fn clip(spans: Vec<Span<Target>>, width: usize) -> Line<Target> {
    let mut left = width;
    let mut clipped = Vec::with_capacity(spans.len());
    for mut span in spans {
        if left == 0 {
            break;
        }
        let cells = span.text.width();
        if cells > left {
            let mut kept = String::new();
            let mut used = 0;
            for c in span.text.chars() {
                let cells = c.width().unwrap_or(0);
                if used + cells > left {
                    break;
                }
                kept.push(c);
                used += cells;
            }
            span.text = kept;
            left = 0;
        } else {
            left -= cells;
        }
        clipped.push(span);
    }
    Line::Text(clipped)
}

/// How long before `now` the moment `then` was, in at most three characters: `now`, `5m`, `3h`, `2d`, `6w`, `2y`.
/// Each unit is truncated, not rounded. A `then` after `now` (clock skew) counts as `now`.
pub(super) fn age(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
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

/// `style` with nothing but its background, to lay under text in groups of its own.
pub(super) fn background(style: Style) -> Style {
    Style {
        bg: style.bg,
        ..Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_truncates_to_the_largest_whole_unit() {
        let now: DateTime<Utc> = "2026-10-22T12:00:00Z".parse().expect("valid RFC 3339");
        let ago = |delta| age(now - delta, now);

        assert_eq!(ago(TimeDelta::seconds(59)), "now");
        assert_eq!(ago(TimeDelta::seconds(-30)), "now", "clock skew");
        assert_eq!(ago(TimeDelta::minutes(5)), "5m");
        assert_eq!(ago(TimeDelta::minutes(119)), "1h");
        assert_eq!(ago(TimeDelta::hours(47)), "1d");
        assert_eq!(ago(TimeDelta::days(13)), "1w");
        assert_eq!(ago(TimeDelta::days(800)), "2y");
    }

    #[test]
    fn fit_cuts_and_pads_to_the_width() {
        assert_eq!(fit("wcgw/ghd", 10), "wcgw/ghd  ");
        assert_eq!(fit("a long title", 6), "a lon…");
        assert_eq!(fit("anything", 0), "");
        assert_eq!(fit("日本語", 4), "日… ");
    }
}
