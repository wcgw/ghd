//! The terminal UI: for now, the Inbox View alone.

use std::pin::pin;

use anyhow::Context as _;
use chrono::{DateTime, TimeDelta, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt as _;
use ratatui::layout::Constraint;
use ratatui::style::{Style, Stylize as _};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::github::GitHub;
use crate::inbox::{Inbox, SubjectKind};

/// Widest the repository column gets, in terminal cells; longer names are cut off.
const MAX_REPOSITORY_WIDTH: u16 = 32;

/// Runs the UI on `terminal` until the user quits, fetching the Inbox from `github` meanwhile.
///
/// # Errors
///
/// Fails if the terminal can't be drawn on or read from. Failing to fetch the Inbox isn't an error here: the Inbox
/// View shows it.
pub async fn run(terminal: &mut DefaultTerminal, github: &GitHub) -> anyhow::Result<()> {
    let mut inbox_view = InboxView::Loading;
    let mut events = EventStream::new();
    // Polled alongside terminal events, so the UI keeps drawing and can be quit while pages arrive.
    let mut fetch = pin!(github.inbox());

    loop {
        terminal
            .draw(|frame| inbox_view.render(frame))
            .context("drawing the terminal")?;

        tokio::select! {
            inbox = &mut fetch, if matches!(inbox_view, InboxView::Loading) => {
                inbox_view = InboxView::fetched(inbox);
            }
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    _ => inbox_view.handle_key(key),
                },
                // Anything else, a resize included, just needs the redraw at the top of the loop.
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error).context("reading terminal events"),
                None => return Ok(()),
            },
        }
    }
}

/// The built-in View showing the whole Inbox, and what it has to show so far.
enum InboxView {
    Loading,
    Loaded { inbox: Inbox, selection: TableState },
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

    /// Moves the selection with `j`/`k` and the arrow keys; other keys do nothing.
    fn handle_key(&mut self, key: KeyEvent) {
        let Self::Loaded { selection, .. } = self else {
            return;
        };
        // Moving past either end is corrected when the table is next drawn.
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => selection.select_next(),
            KeyCode::Char('k') | KeyCode::Up => selection.select_previous(),
            _ => {}
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
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
    use crate::inbox::{Notification, Reason};

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

    #[test]
    fn loaded_inbox_shows_unread_count_and_rows() {
        let inbox: Inbox = [
            Notification {
                repository: "wcgw/ghd".to_owned(),
                subject_kind: SubjectKind::PullRequest,
                subject_title: "Add polling".to_owned(),
                reason: Reason::ReviewRequested,
                unread: true,
                updated_at: Utc::now(),
            },
            Notification {
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
        let mut view = InboxView::fetched(Ok(inbox));
        let mut terminal =
            Terminal::new(TestBackend::new(64, 4)).expect("test backend never fails");

        terminal
            .draw(|frame| view.render(frame))
            .expect("test backend never fails");

        let lines: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(64)
            .map(|line| line.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(lines[0].contains("Inbox (1)"), "{lines:#?}");
        assert!(
            lines[1].contains("● PR  wcgw/ghd   Add polling"),
            "{lines:#?}"
        );
        assert!(lines[1].contains("review-requested now"), "{lines:#?}");
        assert!(lines[2].contains("ISS wcgw/other Old news"), "{lines:#?}");
        assert!(lines[2].contains("subscribed       2d"), "{lines:#?}");
    }
}
