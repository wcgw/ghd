//! voight's statusline segment, its command and its Heatmap.

use super::*;
use crate::assessment::{Assessment, Event, Key, Part, Window};
use crate::hl;
use crate::preview::{Body, Comment, Place, Preview};

/// A Preview whose body is `body`, with one comment, opening on the body.
fn preview_of(body: &str) -> Preview {
    Preview {
        reference: "#1".to_owned(),
        title: "Add polling".to_owned(),
        state: None,
        author: Some("octocat".to_owned()),
        labels: Vec::new(),
        closed: None,
        pull_request: None,
        discussion: None,
        body: Body::Markdown(body.to_owned()),
        comments: vec![Comment {
            author: Some("hubot".to_owned()),
            published_at: Utc::now(),
            path: None,
            body: "A comment.".to_owned(),
            mark: None,
        }],
        opens_on: Place::Body,
        note: None,
    }
}

/// ghd with voight on, its model `loaded` or not, the Preview of "Add polling" showing `body`.
fn assessing(body: &str, loaded: bool) -> (App, Effects) {
    let mut app = app_with(views(), true, 80, 24);
    app.send(fetched(notifications()));
    app.press("p");
    app.advance(PREVIEW_REST);
    let as_of = app
        .app
        .notification(ThreadId(1))
        .expect("listed")
        .updated_at;
    let mut effects = app.send(Msg::PreviewFetched {
        thread: ThreadId(1),
        as_of,
        preview: Ok(preview_of(body)),
    });
    if loaded {
        effects = app.send(Msg::Voight(Event::Loaded { fallback: None }));
    }
    (app, effects)
}

/// The Assessment asked for in `effects`.
fn asked(effects: &Effects) -> Option<(Key, String)> {
    ios(effects).into_iter().find_map(|io| match io {
        Io::Assess { key, text } => Some((key, text)),
        _ => None,
    })
}

/// The Assessment of `text` under `key`, which an LLM wrote `share` of, all one window.
fn assessed(key: Key, text: &str, share: f64) -> Msg {
    Msg::Voight(Event::Assessed {
        key,
        assessment: Ok(Some(Assessment {
            share,
            windows: vec![Window {
                range: 0..text.len(),
                share,
            }],
        })),
    })
}

#[test]
fn voight_assesses_what_the_preview_shows_once_loaded() {
    let (app, effects) = assessing("Some prose to assess.", false);
    assert_eq!(asked(&effects), None, "still loading");
    assert!(
        statusline(&app).contains("voight loading…"),
        "{}",
        statusline(&app)
    );

    let (app, effects) = assessing("Some prose to assess.", true);
    let (key, text) = asked(&effects).expect("asked once loaded");
    assert_eq!(key.thread, ThreadId(1));
    assert_eq!(key.part, Part::Preview(Place::Body));
    assert_eq!(text, "Some prose to assess.");
    assert!(
        statusline(&app).contains("voight …"),
        "{}",
        statusline(&app)
    );
}

#[test]
fn the_assessment_shows_in_the_statusline() {
    let (mut app, effects) = assessing("Some prose to assess.", true);
    let (key, text) = asked(&effects).expect("asked");

    app.send(assessed(key, &text, 0.73));

    assert!(
        statusline(&app).contains("voight 73%"),
        "{}",
        statusline(&app)
    );
}

#[test]
fn too_little_prose_says_nothing() {
    let (mut app, effects) = assessing("LGTM", true);
    let (key, _) = asked(&effects).expect("asked");

    app.send(Msg::Voight(Event::Assessed {
        key,
        assessment: Ok(None),
    }));

    assert!(!statusline(&app).contains("voight"), "{}", statusline(&app));
}

#[test]
fn leader_h_shades_the_preview_once_assessed() {
    let (mut app, effects) = assessing("Some prose to assess.", true);
    let (key, text) = asked(&effects).expect("asked");
    app.send(assessed(key, &text, 1.0));
    let plain = style_of(&app, "Some prose").bg;

    app.press("<leader>h");

    assert_eq!(
        style_of(&app, "Some prose").bg,
        style(&app, hl::HEAT_LLM).bg
    );
    assert_ne!(style_of(&app, "Some prose").bg, plain);
    app.press(":voight heatmap<CR>");
    assert_eq!(style_of(&app, "Some prose").bg, plain, "and back");
}

#[test]
fn the_heatmap_says_when_theres_nothing_to_shade() {
    let (mut app, _) = assessing("Some prose to assess.", true);

    app.press("<leader>h");

    assert!(
        cmdline(&app).contains("The Heatmap shows on text voight has assessed"),
        "{}",
        cmdline(&app)
    );
}

#[test]
fn the_heatmap_needs_voight() {
    let mut app = app();

    app.press("<leader>h");

    assert!(
        cmdline(&app).contains("No Heatmap: voight is off"),
        "{}",
        cmdline(&app)
    );
}

#[test]
fn moving_through_the_preview_assesses_each_comment() {
    let (mut app, effects) = assessing("Some prose to assess.", true);
    let (key, text) = asked(&effects).expect("asked");
    app.send(assessed(key, &text, 0.2));

    app.press("<C-w>j");
    let effects = app.press("G");

    let (key, text) = asked(&effects).expect("the comment asked for");
    assert_eq!(key.part, Part::Preview(Place::Comment(0)));
    assert_eq!(text, "A comment.");
}

#[test]
fn voight_failing_to_load_says_why() {
    let (mut app, _) = assessing("Some prose to assess.", false);

    app.send(Msg::Voight(Event::Failed("out of memory".to_owned())));

    assert!(cmdline(&app).contains("voight couldn't load its model: out of memory"));
    assert!(!statusline(&app).contains("voight"));
}

#[test]
fn falling_back_to_the_cpu_says_so() {
    let (mut app, _) = assessing("Some prose to assess.", false);

    app.send(Msg::Voight(Event::Loaded {
        fallback: Some("couldn't use CUDA, so voight runs on the CPU".to_owned()),
    }));

    assert!(cmdline(&app).contains("couldn't use CUDA"));
}

#[test]
fn voight_off_and_on_again() {
    let (mut app, _) = assessing("Some prose to assess.", true);

    app.press(":voight off<CR>");
    assert!(!statusline(&app).contains("voight"));

    let effects = app.press(":voight on<CR>");
    assert!(statusline(&app).contains("voight"), "{}", statusline(&app));
    assert!(asked(&effects).is_some(), "assessing again");

    app.press(":voight<CR>");
    assert!(cmdline(&app).contains("voight …"), "{}", cmdline(&app));
}

#[test]
fn voight_on_needs_voight_started() {
    let mut app = app();

    app.press(":voight on<CR>");

    assert!(cmdline(&app).contains("voight is off"), "{}", cmdline(&app));
}

#[test]
fn voight_completes_its_arguments() {
    let app = app();

    let completions = app.kit.ex_completions(&app.app, "voight h").candidates;

    assert_eq!(completions, ["heatmap"]);
}
