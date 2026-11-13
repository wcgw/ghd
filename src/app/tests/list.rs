//! The Views as buffers: their rows and columns, the tabline, the cursor, and what's said in place of rows.

use super::*;
use crate::hl::{self, Group};

#[test]
fn rows_line_up_in_columns() {
    let app = app();

    let rows = rows(&app);
    let polling = &rows[row_of(&app, "Add polling")];
    let news = &rows[row_of(&app, "Old news")];
    assert!(
        polling.starts_with(" ● P wcgw/ghd   Add polling"),
        "{rows:#?}"
    );
    assert!(news.starts_with("   I wcgw/other Old news"), "{rows:#?}");
    assert!(polling.ends_with("review-requested now"), "{rows:#?}");
    assert!(news.ends_with("subscribed        2d"), "{rows:#?}");
    assert_eq!(polling.chars().count(), 80, "laid out to the width");
}

#[test]
fn views_are_buffers_in_the_tabline() {
    let mut app = app();

    assert_eq!(rows(&app)[0], " Inbox (1)  Reviews (1)  Read (0)");

    app.press("gt");
    assert!(shows(&app, "Add polling"));
    assert!(!shows(&app, "Old news"), "Reviews shows review requests");

    app.press("<leader>3");
    assert!(shows(&app, "Old news"));
    assert!(!shows(&app, "Add polling"), "Read shows what's read");

    app.press(":b Inbox<CR>");
    assert!(shows(&app, "Add polling") && shows(&app, "Old news"));
}

#[test]
fn the_cursors_row_is_drawn_as_the_cursorline() {
    let mut app = app();
    let cursorline = style(&app, Group::CURSOR_LINE).bg;

    assert_eq!(style_of(&app, "Add polling").bg, cursorline);
    assert_ne!(style_of(&app, "Old news").bg, cursorline);

    app.press("j");

    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
    assert_eq!(style_of(&app, "Old news").bg, cursorline);
    assert_ne!(style_of(&app, "Add polling").bg, cursorline);
}

#[test]
fn read_and_unread_rows_take_their_groups() {
    let app = app();

    assert_eq!(
        style_of(&app, "wcgw/ghd").fg,
        style(&app, hl::REPOSITORY).fg
    );
    assert_eq!(
        style_of(&app, "wcgw/other").fg,
        style(&app, hl::READ).fg,
        "read rows are muted"
    );
    assert!(
        style_of(&app, "Add polling")
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "unread rows are bold"
    );
}

#[test]
fn icons_follow_the_icons_option() {
    let mut app = app();

    app.press(":set icons=nerd<CR>");
    assert!(shows(&app, "\u{f407} wcgw/ghd"), "{:#?}", rows(&app));
    assert!(shows(&app, "\u{f41b} wcgw/other"), "{:#?}", rows(&app));

    app.press(":set icons=plain<CR>");
    assert!(shows(&app, "P wcgw/ghd"));
}

#[test]
fn a_state_shows_in_the_kind_icon() {
    let mut app = app();

    app.send(Msg::LookedUp {
        asked: vec![crate::state::Lookup {
            thread: ThreadId(1),
            subject_url: "https://github.com/wcgw/ghd/pull/1".to_owned(),
            as_of: notifications()[0].updated_at,
        }],
        found: Ok(HashMap::from([(ThreadId(1), SubjectState::Merged)])),
    });
    app.press(":set icons=nerd<CR>");

    assert!(shows(&app, "\u{f419} wcgw/ghd"), "{:#?}", rows(&app));
    assert_eq!(
        style_of(&app, "\u{f419}").fg,
        style(&app, hl::STATE_MERGED).fg
    );
}

#[test]
fn long_titles_are_cut_to_the_width() {
    let mut app = app_with(views(), false, 50, 24);
    let mut long = notifications();
    long[0].subject_title = "A title much too long to fit on a narrow terminal".to_owned();
    app.send(fetched(long));

    let row = &rows(&app)[row_of(&app, "A title")];
    assert!(row.contains('…'), "{row:?}");
    assert!(row.ends_with("review-requested now"), "{row:?}");
    assert_eq!(row.chars().count(), 50);

    app.update(wcgw_kit::Msg::Resize {
        width: 120,
        height: 24,
    });
    assert!(
        shows(&app, "on a narrow terminal"),
        "laid out again for the width"
    );
}

#[test]
fn the_cursor_stays_on_its_notification_as_the_inbox_changes() {
    let mut app = app();
    app.press("j");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));

    let mut newer = notifications();
    newer.insert(
        0,
        Notification {
            thread: ThreadId(3),
            subject_title: "Brand new".to_owned(),
            ..notifications()[0].clone()
        },
    );
    app.send(fetched(newer));

    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
    assert!(row_of(&app, "Brand new") < row_of(&app, "Add polling"));
}

#[test]
fn each_view_keeps_its_own_cursor() {
    let mut app = app();
    app.press("j");
    app.press("<leader>3");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));

    app.press("<leader>1");

    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
}

#[test]
fn what_shows_in_place_of_rows_says_why() {
    let mut app = app_with(views(), false, 80, 24);
    assert!(shows(&app, "Fetching notifications…"));

    app.send(Msg::Fetched {
        began: Instant::now(),
        listing: Err("timed out".to_owned()),
        poll_after: Duration::from_secs(60),
    });
    assert!(shows(&app, "Couldn't fetch notifications: timed out"));

    app.send(fetched(Vec::new()));
    assert!(shows(&app, "Nothing in your Inbox."));

    app.send(fetched(notifications()));
    app.press("<leader>2");
    app.press(":filter is:issue<CR>");
    assert!(
        shows(&app, "Nothing in this View matches the filter."),
        "{:#?}",
        rows(&app)
    );
}

#[test]
fn a_failed_poll_keeps_the_inbox_and_says_why() {
    let mut app = app();

    app.send(Msg::Fetched {
        began: Instant::now(),
        listing: Err("timed out".to_owned()),
        poll_after: Duration::from_secs(60),
    });

    assert!(shows(&app, "Add polling"));
    assert!(
        cmdline(&app).contains("Couldn't refresh the Inbox, it may be out of date: timed out"),
        "{}",
        cmdline(&app)
    );
}

#[test]
fn polls_come_as_github_asks_and_restart_with_each_fetch() {
    let mut app = app_with(views(), false, 80, 24);

    let effects = app.send(fetched(notifications()));
    assert!(ios(&effects).is_empty() || !ios(&effects).contains(&Io::FetchInbox(Fetch::IfChanged)));

    let effects = app.advance(Duration::from_secs(60));
    assert!(
        ios(&effects).contains(&Io::FetchInbox(Fetch::IfChanged)),
        "{effects:?}"
    );
    assert!(statusline(&app).contains("fetching…"));

    // A poll answered while another was asked for: only the latest polls.
    let effects = app.send(Msg::Fetched {
        began: Instant::now(),
        listing: Ok(None),
        poll_after: Duration::from_secs(30),
    });
    assert_eq!(ios(&effects), []);
    let effects = app.advance(Duration::from_secs(30));
    assert_eq!(
        ios(&effects)
            .iter()
            .filter(|io| **io == Io::FetchInbox(Fetch::IfChanged))
            .count(),
        1
    );
}

#[test]
fn ctrl_l_fetches_the_whole_inbox_now() {
    let mut app = app();

    let effects = app.press("<C-l>");

    assert!(
        ios(&effects).contains(&Io::FetchInbox(Fetch::Whole)),
        "{effects:?}"
    );
    assert!(
        ios(&app.press("<C-l>")).is_empty(),
        "the fetch under way does"
    );
}

#[test]
fn states_are_looked_up_once_the_inbox_arrives() {
    let mut app = app_with(views(), false, 80, 24);

    let effects = app.send(fetched(notifications()));

    let lookups: Vec<_> = ios(&effects)
        .into_iter()
        .filter(|io| matches!(io, Io::LookUp(_)))
        .collect();
    assert_eq!(lookups.len(), 1, "{effects:?}");
    assert!(statusline(&app).contains("fetching…"));
}

#[test]
fn tabs_count_whats_unread_in_each_view_or_whats_done() {
    let mut app = app();

    assert_eq!(rows(&app)[0], " Inbox (1)  Reviews (1)  Read (0)");

    app.press("a");
    app.press("D");
    assert_eq!(
        rows(&app)[0],
        " Inbox (1)  Reviews (1)  Read (0)",
        "Done, in the Done list"
    );
    app.press("D");
    assert_eq!(rows(&app)[0], " Inbox (0)  Reviews (0)  Read (0)");
}

#[test]
fn a_snapshot_shows_until_the_inbox_arrives() {
    let mut app = app();
    app.press("j");
    let snapshot = app.app.snapshot(Utc::now()).expect("an Inbox");
    assert_eq!(snapshot.cursors.get("Inbox"), Some(&ThreadId(2)));

    let (ghd, kit) = Ghd::restored(views(), false, Some(snapshot));
    let mut restored = Harness::new(ghd, kit, 80, 24);

    assert!(shows(&restored, "Add polling"));
    assert_eq!(cursor_title(&mut restored).as_deref(), Some("Old news"));
    assert!(
        cmdline(&restored).contains("Showing the Inbox from just now until the latest arrives"),
        "{}",
        cmdline(&restored)
    );
}

#[test]
fn a_colorscheme_change_redraws_the_list() {
    let mut app = app();

    app.press(":colorscheme default<CR>");

    assert_eq!(style_of(&app, "wcgw/other").fg, style(&app, hl::READ).fg);
    assert_eq!(
        style(&app, hl::READ).fg,
        Some(ratatui::style::Color::DarkGray),
        "ghd's own for it"
    );
}

#[test]
fn segments_go_least_important_first_when_the_line_is_short() {
    let mut app = app_with(views(), false, 40, 24);
    app.send(fetched(notifications()));
    app.press("D");
    let effects = app.press("<C-l>");
    assert!(ios(&effects).contains(&Io::FetchInbox(Fetch::Whole)));

    let line = statusline(&app);

    assert!(line.contains("Done list"), "{line:?}");
    assert!(!line.contains("fetching…"), "{line:?}");
}

#[test]
fn a_view_in_a_vsplit_is_laid_out_for_its_window() {
    let mut app = app();

    app.press(":vsplit<CR>");

    // The window on the left, which the other, a cell narrower, shares the buffer with.
    let left: Vec<String> = rows(&app)
        .iter()
        .map(|row| row.split('│').next().unwrap_or_default().to_owned())
        .collect();
    assert!(left[1].ends_with("review-requested now"), "{left:#?}");
    assert!(
        left[2].ends_with("subscribed        2d"),
        "nothing wraps: {left:#?}"
    );
}

#[test]
fn every_views_cursor_comes_back_from_a_snapshot() {
    let mut app = app();
    app.press("j");
    app.press("<leader>3");
    let snapshot = app.app.snapshot(Utc::now()).expect("an Inbox");
    assert_eq!(snapshot.view, "Read");

    let (ghd, kit) = Ghd::restored(views(), false, Some(snapshot));
    let mut restored = Harness::new(ghd, kit, 80, 24);
    assert_eq!(cursor_title(&mut restored).as_deref(), Some("Old news"));

    restored.press("<leader>1");

    assert_eq!(
        cursor_title(&mut restored).as_deref(),
        Some("Old news"),
        "the Inbox's too"
    );
}

#[test]
fn a_narrow_window_cuts_the_repository_before_the_title() {
    let mut app = app_with(views(), false, 50, 24);
    let mut long = notifications();
    long[0].repository = "an-organization/with-a-long-repository".to_owned();
    app.send(fetched(long));

    let row = &rows(&app)[row_of(&app, "an-orga…")];

    assert!(row.contains("Add polling"), "{row:?}");
    assert!(row.ends_with("review-requested now"), "{row:?}");
}
