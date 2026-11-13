//! The Preview window below the list, and its Markdown.

use super::*;
use crate::preview::{Body, Comment, Mark, Note, Place, Preview};

/// The Preview of "Add polling": a description, then two comments, opening on the second.
fn polling() -> Preview {
    let comment = |author: &str, body: &str, mark| Comment {
        author: Some(author.to_owned()),
        published_at: Utc::now() - TimeDelta::hours(3),
        path: None,
        body: body.to_owned(),
        mark,
    };
    Preview {
        reference: "#1".to_owned(),
        title: "Add polling".to_owned(),
        state: Some(SubjectState::Open),
        author: Some("octocat".to_owned()),
        labels: vec!["enhancement".to_owned()],
        closed: None,
        pull_request: None,
        discussion: None,
        body: Body::Markdown(
            "# Why\n\nPoll the Inbox, see [the docs](https://docs.example.com).".to_owned(),
        ),
        comments: vec![
            comment("hubot", "First, a thought.", None),
            comment(
                "alxs",
                "Looks good, @octocat.\n\n```rust\nlet a = 1;\n```",
                Some(Mark::MentionsYou),
            ),
        ],
        opens_on: Place::Comment(1),
        note: None,
    }
}

/// [`app`] with the Preview open, its fetch asked for once the cursor rested.
fn previewing() -> App {
    let mut app = app();
    app.press("p");
    let effects = app.advance(PREVIEW_REST);
    assert!(
        ios(&effects)
            .iter()
            .any(|io| matches!(io, Io::FetchPreview(wanted) if wanted.thread == ThreadId(1))),
        "{effects:?}"
    );
    app
}

/// The Preview of thread 1, `preview`, arriving for its activity in `app`.
fn arrives(app: &mut App, preview: Result<Preview, String>) -> Effects {
    let as_of = app
        .app
        .notification(ThreadId(1))
        .expect("in the Inbox")
        .updated_at;
    app.send(Msg::PreviewFetched {
        thread: ThreadId(1),
        as_of,
        preview,
    })
}

/// Whether the Preview's window is open: its winbar shows.
fn preview_open(app: &App) -> bool {
    rows(app).iter().any(|row| row.starts_with(" Preview"))
}

/// The Preview of thread 1 arriving for its activity in `app`.
fn arrives_for(app: &mut App, preview: Preview) {
    arrives(app, Ok(preview));
}

#[test]
fn p_opens_the_preview_below_the_list() {
    let mut app = app();

    app.press("p");

    assert!(preview_open(&app));
    assert!(
        row_of(&app, " Preview") > row_of(&app, "Old news"),
        "{:#?}",
        rows(&app)
    );
    assert!(shows(&app, "Fetching…"));
    assert_eq!(
        cursor_title(&mut app).as_deref(),
        Some("Add polling"),
        "the list keeps the cursor"
    );

    app.press("p");
    assert!(!preview_open(&app));
}

#[test]
fn the_preview_waits_for_the_cursor_to_rest() {
    let mut app = app();
    app.press("p");

    let effects = app.press("j");
    assert!(ios(&effects).is_empty(), "nothing fetched while moving");
    app.press("k");
    let effects = app.advance(PREVIEW_REST);

    let fetched: Vec<ThreadId> = ios(&effects)
        .into_iter()
        .filter_map(|io| match io {
            Io::FetchPreview(wanted) => Some(wanted.thread),
            _ => None,
        })
        .collect();
    assert_eq!(fetched, [ThreadId(1)], "only where it rested");
}

#[test]
fn the_preview_shows_its_header_body_and_comments_and_opens_on_its_comment() {
    let mut app = previewing();
    arrives_for(&mut app, polling());

    assert!(shows(&app, " Preview · 2 comments"), "{:#?}", rows(&app));
    assert!(shows(&app, "#1 Add polling"));
    assert!(shows(&app, "wcgw/ghd · open · @octocat · enhancement"));
    assert!(
        shows(&app, "@alxs mentioned you 3h ago"),
        "{:#?}",
        rows(&app)
    );

    app.press("<C-w>j");
    let cursor = usize::from(app.screen().cursor.1);
    assert_eq!(
        cursor,
        row_of(&app, "@alxs mentioned you"),
        "opens on the comment it's about"
    );
}

#[test]
fn brackets_step_through_the_comments() {
    let mut app = previewing();
    arrives_for(&mut app, polling());

    app.press("<C-w>j");
    app.press("gg");
    assert!(shows(&app, "#1 Add polling"));
    app.press("]]]]");

    assert!(shows(&app, "@hubot commented 3h ago"), "{:#?}", rows(&app));
}

#[test]
fn markdown_is_rendered_its_code_highlighted_and_its_links_followed() {
    let mut app = previewing();
    arrives_for(&mut app, polling());

    assert!(shows(&app, "Why"), "{:#?}", rows(&app));
    assert!(!shows(&app, "# Why"), "a heading, not its marker");
    app.press("<C-w>j");
    app.press("G");
    assert_eq!(
        style_of(&app, "let").fg,
        style(&app, crate::hl::Group::KEYWORD).fg
    );

    app.press("gg/the docs<CR>");
    let effects = app.press("gx");
    assert_eq!(opened(&effects), ["https://docs.example.com"]);
    let effects = app.press("gy");
    assert!(
        effects.contains(&Effect::Copy("https://docs.example.com".to_owned())),
        "gy yanks what gx opens: {effects:?}"
    );
    let effects = app.press("<CR>");
    assert_eq!(
        opened(&effects),
        ["https://docs.example.com"],
        "<CR> follows it too"
    );
}

#[test]
fn a_description_mentioning_you_says_so() {
    let mut app = previewing();
    arrives_for(
        &mut app,
        Preview {
            comments: Vec::new(),
            opens_on: Place::Body,
            note: Some(Note::BodyMentionsYou),
            ..polling()
        },
    );

    assert!(
        shows(&app, "@octocat mentioned you in the description"),
        "{:#?}",
        rows(&app)
    );
    assert!(preview_open(&app));
    assert!(!shows(&app, "comments"));
}

#[test]
fn a_failed_preview_says_why_and_is_tried_again_when_the_cursor_comes_back() {
    let mut app = previewing();

    arrives(&mut app, Err("Not Found".to_owned()));
    assert!(
        shows(&app, "Couldn't fetch the Preview: Not Found"),
        "{:#?}",
        rows(&app)
    );

    app.press("j");
    app.press("k");
    let effects = app.advance(PREVIEW_REST);
    assert!(
        ios(&effects)
            .iter()
            .any(|io| matches!(io, Io::FetchPreview(_))),
        "{effects:?}"
    );
}

#[test]
fn kinds_without_a_preview_say_so() {
    let mut app = app_with(views(), false, 80, 24);
    let mut release = notifications();
    release[0].subject_kind = SubjectKind::CheckSuite;
    release[0].api_paths.subject = None;
    app.send(fetched(release));

    app.press("p");
    let effects = app.advance(PREVIEW_REST);

    assert!(
        ios(&effects)
            .iter()
            .all(|io| !matches!(io, Io::FetchPreview(_)))
    );
    assert!(shows(
        &app,
        "No Preview for this kind of Notification yet: gX opens it."
    ));
}

#[test]
fn ctrl_l_fetches_the_preview_again() {
    let mut app = previewing();
    arrives_for(&mut app, polling());

    app.press("<C-l>");
    let effects = app.advance(PREVIEW_REST);

    assert!(
        ios(&effects)
            .iter()
            .any(|io| matches!(io, Io::FetchPreview(_))),
        "{effects:?}"
    );
    assert!(shows(&app, "Fetching…"));
}

#[test]
fn newer_activity_fetches_the_preview_again() {
    let mut app = previewing();
    arrives_for(&mut app, polling());

    let mut newer = notifications();
    newer[0].updated_at += TimeDelta::minutes(5);
    app.send(fetched(newer));
    let effects = app.advance(PREVIEW_REST);

    assert!(shows(&app, "Fetching…"), "{:#?}", rows(&app));
    assert!(
        ios(&effects)
            .iter()
            .any(|io| matches!(io, Io::FetchPreview(_)))
            || app.app.preview.is_open()
    );
}

#[test]
fn a_short_terminal_has_no_preview() {
    let mut app = app_with(views(), false, 80, 11);
    app.send(fetched(notifications()));

    app.press("p");
    assert!(cmdline(&app).contains("The Preview needs a terminal at least 12 lines tall"));
    assert!(!preview_open(&app));

    let mut tall = super::app();
    tall.press("p");
    tall.update(wcgw_kit::Msg::Resize {
        width: 80,
        height: 10,
    });
    assert!(!preview_open(&tall), "hidden while too short");
    tall.update(wcgw_kit::Msg::Resize {
        width: 80,
        height: 24,
    });
    assert!(preview_open(&tall), "back once tall enough");
}

#[test]
fn q_in_the_preview_closes_it() {
    let mut app = app();
    app.press("p");

    app.press("<C-w>j");
    let effects = app.press("q");

    assert!(!effects.contains(&Effect::Quit));
    assert!(!preview_open(&app));
    assert!(shows(&app, "Add polling"));
}

#[test]
fn q_in_the_list_leaves_the_list_on_screen() {
    let mut app = app();
    app.press("p");

    app.press("q");

    assert!(shows(&app, "Add polling"));
    assert!(!preview_open(&app));
    assert!(app.press("q").contains(&Effect::Quit), "then quits");
}

#[test]
fn moving_the_list_shows_the_other_preview() {
    let mut app = previewing();
    arrives_for(&mut app, polling());

    app.press("j");

    assert!(shows(&app, "Fetching…"), "{:#?}", rows(&app));
    assert!(!shows(&app, "#1 Add polling"));
    app.press("k");
    assert!(shows(&app, "#1 Add polling"), "kept");
}
