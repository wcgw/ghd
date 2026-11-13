//! ghd as the user meets it, through the kit's test harness: keys in, screen and effects out.

mod files;
mod list;
mod preview;
mod triage;
mod voight;

use std::time::Duration;

use chrono::{TimeDelta, Utc};
use ratatui::style::Style;
use tokio::time::Instant;
use wcgw_kit::testing::Harness;

use super::*;
use crate::inbox::{ApiPaths, Reason, SubjectKind};

/// ghd in the kit, as the tests drive it.
pub(super) type App = Harness<Ghd>;

/// "Add polling" (thread 1, an unread review request on a pull request, just now), then "Old news" (thread 2, a
/// read issue one is subscribed to, two days ago).
pub(super) fn notifications() -> Vec<Notification> {
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
            api_paths: ApiPaths {
                subject: Some("/repos/wcgw/ghd/pulls/1".to_owned()),
                latest_comment: None,
            },
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
            api_paths: ApiPaths {
                subject: Some("/repos/wcgw/other/issues/2".to_owned()),
                latest_comment: None,
            },
        },
    ]
}

/// A View called `name`, showing what `filter` matches.
pub(super) fn view(name: &str, filter: &str) -> View {
    View {
        name: name.to_owned(),
        filter: filter.parse().expect("a valid filter"),
    }
}

/// The Inbox View, "Reviews" (`reason:review-requested`) and "Read" (`is:read`).
pub(super) fn views() -> Vec<View> {
    vec![
        view("Inbox", ""),
        view("Reviews", "reason:review-requested"),
        view("Read", "is:read"),
    ]
}

/// ghd with `views`, voight on or not, in a `width` × `height` terminal, before the Inbox arrives; icons plain, as
/// any font draws them.
pub(super) fn app_with(views: Vec<View>, voight: bool, width: u16, height: u16) -> App {
    let (ghd, kit) = Ghd::session(views, voight);
    let mut app = Harness::new(ghd, kit, width, height);
    app.press(":set icons=plain<CR>");
    app
}

/// ghd with [`views`] in an 80 × 24 terminal, [`notifications`] fetched.
pub(super) fn app() -> App {
    let mut app = app_with(views(), false, 80, 24);
    app.send(fetched(notifications()));
    app
}

/// A fetch that has just brought `notifications`.
pub(super) fn fetched(notifications: Vec<Notification>) -> Msg {
    Msg::Fetched {
        began: Instant::now(),
        listing: Ok(Some(Listing {
            inbox: notifications.into_iter().collect(),
            done: Inbox::default(),
        })),
        poll_after: Duration::from_secs(60),
    }
}

/// The rows the terminal shows.
pub(super) fn rows(app: &App) -> Vec<String> {
    app.screen().rows
}

/// Whether some row shows `text`.
pub(super) fn shows(app: &App, text: &str) -> bool {
    rows(app).iter().any(|row| row.contains(text))
}

/// Whether a row of the windows shows `text`: not the tabline, the statusline nor the command line.
pub(super) fn listed(app: &App, text: &str) -> bool {
    let rows = rows(app);
    rows[1..rows.len() - 2].iter().any(|row| row.contains(text))
}

/// The row showing `text`, by its index; panics with the screen when there's none.
pub(super) fn row_of(app: &App, text: &str) -> usize {
    let rows = rows(app);
    rows.iter()
        .position(|row| row.contains(text))
        .unwrap_or_else(|| panic!("no row shows {text:?}: {rows:#?}"))
}

/// The command line: the bottom row.
pub(super) fn cmdline(app: &App) -> String {
    rows(app).last().cloned().unwrap_or_default()
}

/// The statusline: the row above the command line.
pub(super) fn statusline(app: &App) -> String {
    let rows = rows(app);
    rows[rows.len() - 2].clone()
}

/// ghd's own effects among `effects`.
pub(super) fn ios(effects: &Effects) -> Vec<Io> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::App(io) => Some(io.clone()),
            _ => None,
        })
        .collect()
}

/// The links `effects` open in the browser.
pub(super) fn opened(effects: &Effects) -> Vec<String> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::OpenUrl(url) => Some(url.clone()),
            _ => None,
        })
        .collect()
}

/// The changes `effects` send to GitHub.
pub(super) fn sent(effects: &Effects) -> Vec<(u64, Change)> {
    ios(effects)
        .into_iter()
        .filter_map(|io| match io {
            Io::Send { id, change } => Some((id, change)),
            _ => None,
        })
        .collect()
}

/// The style the cell under the first `text` on screen is drawn in.
pub(super) fn style_of(app: &App, text: &str) -> Style {
    let rows = rows(app);
    for (y, row) in rows.iter().enumerate() {
        if let Some(at) = row.find(text) {
            let x = unicode_width::UnicodeWidthStr::width(&row[..at]);
            return app.style_at(
                u16::try_from(x).expect("on screen"),
                u16::try_from(y).expect("on screen"),
            );
        }
    }
    panic!("{text:?} isn't on screen: {rows:#?}");
}

/// How `group` is drawn now.
pub(super) fn style(app: &App, group: crate::hl::Group) -> Style {
    app.kit.style(group)
}

/// The Notification the list's cursor is on.
pub(super) fn cursor_title(app: &mut App) -> Option<String> {
    let Harness { app: ghd, kit, .. } = app;
    let thread = ghd.at_cursor(kit)?;
    ghd.notification(thread).map(|n| n.subject_title.clone())
}

/// GitHub's answer accepting change `id`.
pub(super) fn accepted(id: u64, change: Change) -> Msg {
    Msg::Settled {
        id,
        change,
        accepted: Ok(()),
    }
}

/// GitHub's answer refusing change `id`.
pub(super) fn refused(id: u64, change: Change) -> Msg {
    Msg::Settled {
        id,
        change,
        accepted: Err("Not Found".to_owned()),
    }
}

#[test]
fn ghd_starts_by_fetching_the_inbox() {
    let mut app = app_with(views(), false, 80, 24);

    let effects = {
        let Harness { app: ghd, kit, .. } = &mut app;
        kit.start(ghd)
    };

    assert_eq!(ios(&effects), [Io::FetchInbox(Fetch::Whole)]);
    assert!(shows(&app, "Fetching notifications…"));
    assert!(
        statusline(&app).contains("fetching…"),
        "{}",
        statusline(&app)
    );
}
