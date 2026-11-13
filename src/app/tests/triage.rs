//! Acting on Notifications: marking them read or Done, the Selection, filters, opening them, and the keys for it.

use super::*;

/// The thread of `change`.
fn thread_of(change: &Change) -> ThreadId {
    match change {
        Change::MarkRead(n) | Change::MarkDone(n) => n.thread,
    }
}

#[test]
fn x_marks_done_at_once_and_it_comes_back_if_refused() {
    let mut app = app();

    let effects = app.press("a");

    let sent = sent(&effects);
    assert_eq!(sent.len(), 1);
    assert!(matches!(&sent[0].1, Change::MarkDone(n) if n.thread == ThreadId(1)));
    assert!(!listed(&app, "Add polling"), "gone from every View at once");
    assert!(cmdline(&app).contains("Marked Done: Add polling"));
    assert_eq!(
        cursor_title(&mut app).as_deref(),
        Some("Old news"),
        "the one below"
    );

    let (id, change) = sent[0].clone();
    app.send(refused(id, change));

    assert!(shows(&app, "Add polling"));
    assert!(
        cmdline(&app).contains("GitHub refused to mark Done \"Add polling\", undone: Not Found"),
        "{}",
        cmdline(&app)
    );
}

#[test]
fn accepted_done_stays_done_though_github_lists_it() {
    let mut listed_by_github = notifications();
    listed_by_github[0].unread = false;
    let mut app = app_with(views(), false, 80, 24);
    app.send(fetched(listed_by_github.clone()));
    let (id, change) = sent(&app.press("a")).remove(0);
    app.send(accepted(id, change));

    // GitHub keeps listing what's Done, read, with no newer activity.
    app.send(fetched(listed_by_github));

    assert!(!listed(&app, "Add polling"));
}

#[test]
fn shift_i_marks_read_only_what_is_unread() {
    let mut app = app();

    let effects = app.press("I");
    assert_eq!(sent(&effects).len(), 1);
    assert!(
        !style_of(&app, "Add polling")
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );

    app.press("j");
    assert!(sent(&app.press("I")).is_empty(), "read already");
}

#[test]
fn read_takes_three_letters_not_one() {
    let mut app = app();

    assert_eq!(sent(&app.press(":r<CR>")).len(), 0);
    assert!(
        shows(&app, "E492: Not an editor command: r"),
        "{:#?}",
        rows(&app)
    );
    assert_eq!(sent(&app.press(":rea<CR>")).len(), 1);
}

#[test]
fn refused_read_is_undone() {
    let mut app = app();
    let (id, change) = sent(&app.press("I")).remove(0);

    app.send(refused(id, change));

    assert!(rows(&app)[0].contains("Inbox (1)"), "{:?}", rows(&app)[0]);
}

#[test]
fn enter_opens_in_the_browser_and_marks_read() {
    let mut app = app();

    let effects = app.press("<CR>");

    assert_eq!(opened(&effects), ["https://github.com/wcgw/ghd/pull/1"]);
    assert_eq!(sent(&effects).len(), 1, "marked read");
    assert!(rows(&app)[0].contains("Inbox (0)"), "{:?}", rows(&app)[0]);
}

#[test]
fn gx_on_a_notification_is_no_link_and_gshift_x_opens_it() {
    let mut app = app();
    app.press("j");

    let effects = app.press("gx");
    assert_eq!(opened(&effects), Vec::<String>::new());
    assert!(cmdline(&app).contains("No link under the cursor"));

    let effects = app.press("gX");
    assert_eq!(opened(&effects), ["https://github.com/wcgw/other/issues/2"]);
    assert!(sent(&effects).is_empty(), "read already");
}

#[test]
fn gy_yanks_the_subjects_link() {
    let mut app = app();

    let effects = app.press("gy");

    assert!(effects.contains(&Effect::Copy(
        "https://github.com/wcgw/ghd/pull/1".to_owned()
    )));
}

#[test]
fn a_double_click_opens_what_it_lands_on() {
    let mut app = app();
    let row = u16::try_from(row_of(&app, "Old news")).expect("on screen");

    let effects = app.update(wcgw_kit::Msg::Mouse {
        mouse: wcgw_kit::key::Mouse::Left,
        column: 20,
        row,
    });
    assert!(opened(&effects).is_empty(), "a click moves the cursor");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));

    let effects = app.update(wcgw_kit::Msg::Mouse {
        mouse: wcgw_kit::key::Mouse::DoubleLeft,
        column: 20,
        row,
    });
    assert_eq!(opened(&effects), ["https://github.com/wcgw/other/issues/2"]);
}

#[test]
fn s_selects_and_moves_down_and_actions_act_on_the_selection() {
    let mut app = app();

    app.press("x");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
    assert!(
        statusline(&app).contains("1 selected"),
        "{}",
        statusline(&app)
    );
    assert!(shows(&app, "▌● P wcgw/ghd"), "{:#?}", rows(&app));
    app.press("x");
    assert!(statusline(&app).contains("2 selected"));

    let effects = app.press("a");

    let threads: Vec<ThreadId> = sent(&effects)
        .iter()
        .map(|(_, change)| thread_of(change))
        .collect();
    assert_eq!(threads, [ThreadId(1), ThreadId(2)]);
    assert!(cmdline(&app).contains("Marked 2 Done"));
    assert!(!statusline(&app).contains("selected"), "acting empties it");
}

#[test]
fn shift_a_selects_all_shown_or_none() {
    let mut app = app();

    app.press("A");
    assert!(statusline(&app).contains("2 selected"));
    app.press("A");
    assert!(!statusline(&app).contains("selected"));
}

#[test]
fn select_and_unselect_take_a_filter() {
    let mut app = app();

    app.press(":select is:pr<CR>");
    assert!(statusline(&app).contains("1 selected"));
    assert!(
        cmdline(&app).contains("Selected 1 Notification"),
        "{}",
        cmdline(&app)
    );

    app.press(":select<CR>");
    assert!(
        cmdline(&app).contains("Selected 1 Notification"),
        "only one more"
    );
    assert!(statusline(&app).contains("2 selected"));

    app.press(":unselect is:read<CR>");
    assert!(statusline(&app).contains("1 selected"));

    app.press(":select reason:nonsense<CR>");
    assert!(
        cmdline(&app).starts_with("Not a filter"),
        "{}",
        cmdline(&app)
    );
}

#[test]
fn filters_complete_from_what_is_shown() {
    let app = app();
    let complete = |line: &str| app.kit.ex_completions(&app.app, line).candidates;

    assert_eq!(complete("select rea"), ["reason:"]);
    assert_eq!(complete("filter -rea"), ["-reason:"]);
    assert_eq!(
        complete("unselect reason:"),
        ["reason:review-requested", "reason:subscribed"]
    );
    assert_eq!(complete("filter repo:wcgw/g"), ["repo:wcgw/ghd"]);
    assert_eq!(complete("filt"), ["filter"]);
}

#[test]
fn tab_completes_on_the_command_line() {
    let mut app = app();

    app.press(":select reason:rev<Tab>");

    assert!(
        cmdline(&app).contains(":select reason:review-requested"),
        "{}",
        cmdline(&app)
    );
}

#[test]
fn filter_narrows_every_view_until_cleared() {
    let mut app = app();

    app.press(":filter is:issue<CR>");
    assert!(!shows(&app, "Add polling") && shows(&app, "Old news"));
    assert!(
        statusline(&app).contains("filter is:issue"),
        "{}",
        statusline(&app)
    );

    app.press("<leader>3");
    assert!(shows(&app, "Old news"), "it follows from View to View");

    app.press("<Esc>");
    app.press("<leader>1");
    assert!(shows(&app, "Add polling"));
    assert!(!statusline(&app).contains("filter"));
}

#[test]
fn esc_clears_the_selection_before_the_filter() {
    let mut app = app();
    app.press(":filter is:read<CR>");
    app.press("A");

    app.press("<Esc>");
    assert!(!statusline(&app).contains("selected"));
    assert!(statusline(&app).contains("filter"));

    app.press("<Esc>");
    assert!(!statusline(&app).contains("filter"));
}

#[test]
fn u_shows_only_the_unread_in_the_view_on_screen() {
    let mut app = app();

    app.press("u");
    assert!(shows(&app, "Add polling") && !shows(&app, "Old news"));
    assert!(
        statusline(&app).contains("unread only"),
        "{}",
        statusline(&app)
    );

    app.press("<leader>3");
    assert!(shows(&app, "Old news"), "each View has its own");
}

#[test]
fn shift_d_shows_the_done_list_where_x_brings_back() {
    let mut app = app();
    let (id, change) = sent(&app.press("a")).remove(0);
    app.send(accepted(id, change));

    app.press("D");
    assert!(
        shows(&app, "Add polling") && !shows(&app, "Old news"),
        "{:#?}",
        rows(&app)
    );
    assert!(statusline(&app).contains("Done list"));
    assert!(
        rows(&app)[0].contains("Inbox (1)"),
        "Done, counted: {:?}",
        rows(&app)[0]
    );

    let effects = app.press("a");
    assert!(sent(&effects).is_empty(), "GitHub can't move it back");
    assert!(cmdline(&app).contains("Back in ghd's Inbox (GitHub keeps it Done): Add polling"));
    assert!(shows(&app, "Nothing in the Done list."));

    app.press("D");
    assert!(shows(&app, "Add polling"));
}

#[test]
fn unread_only_is_for_the_inbox() {
    let mut app = app();
    app.press("D");

    app.press("u");

    assert!(cmdline(&app).contains("Unread only is for the Inbox, not the Done list"));
}

#[test]
fn quitting_leaves_what_to_keep() {
    let mut app = app();
    let (id, _) = sent(&app.press("I")).remove(0);

    let effects = app.press(":q<CR>");

    assert!(effects.contains(&Effect::Quit));
    assert_eq!(app.app.unsettled(), [id], "what GitHub hasn't answered");
    let snapshot = app.app.snapshot(Utc::now()).expect("the Inbox");
    assert!(
        !snapshot
            .listing
            .inbox
            .get(ThreadId(1))
            .expect("kept")
            .unread
    );
}

#[test]
fn keys_are_vim_motions_and_ghds_commands() {
    let mut app = app();

    // `e` is a motion, not Done; `g` a prefix, `gg` the top.
    assert_eq!(sent(&app.press("e")), []);
    app.press("j");
    app.press("gg");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Add polling"));
    app.press("G");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
    // `?` searches backwards, `/` forwards: the kit's, in the buffer.
    app.press("?polling<CR>");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Add polling"));
    app.press("/news<CR>");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
    // `]]` goes from Notification to Notification.
    app.press("gg]]");
    assert_eq!(cursor_title(&mut app).as_deref(), Some("Old news"));
}

#[test]
fn help_lists_ghds_keys_and_commands() {
    let mut app = app();

    for (keys, says) in [
        ("a", "mark Done, or bring back"),
        ("D", "toggle the Done list"),
        ("gf", "pull request's files"),
        ("x", "select or not, and down"),
    ] {
        app.press(&format!(":help {keys}<CR>"));
        assert!(shows(&app, says), "{keys}: {:#?}", rows(&app));
    }
}

#[test]
fn leader_f_starts_typing_a_filter() {
    let mut app = app();

    app.press("<leader>f");

    assert_eq!(cmdline(&app), ":filter");
    app.press("is:pr<CR>");
    assert!(statusline(&app).contains("filter is:pr"));
}
