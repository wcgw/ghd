//! The Files screen: a pull request's Changed files, and each one whole or as its diff.

use super::*;
use crate::files::{ChangedFile, FileStatus, FileText};
use crate::hl::{self, Group};

fn changed(path: &str, status: FileStatus, patch: Option<&str>) -> ChangedFile {
    ChangedFile {
        path: path.to_owned(),
        status,
        previous_path: None,
        additions: 3,
        deletions: 1,
        patch: patch.map(str::to_owned),
        contents_path: format!("/repos/wcgw/ghd/contents/{path}?ref=abc"),
    }
}

/// A new Markdown file, a modified Rust one, a removed text file, and a Rust one renamed as it was.
fn changed_files() -> Vec<ChangedFile> {
    vec![
        changed(
            "docs/guide.md",
            FileStatus::Added,
            Some("@@ -0,0 +1,3 @@\n+# Guide\n+\n+Read me."),
        ),
        changed(
            "src/ui.rs",
            FileStatus::Modified,
            Some("@@ -1,2 +1,2 @@\n fn a() {}\n-old\n+new"),
        ),
        ChangedFile {
            deletions: 5,
            ..changed("old.txt", FileStatus::Removed, None)
        },
        ChangedFile {
            previous_path: Some("src/old.rs".to_owned()),
            additions: 0,
            deletions: 0,
            ..changed("src/new.rs", FileStatus::Renamed, None)
        },
    ]
}

/// [`app`] on its Files screen, for "Add polling", its Changed files fetched as `files`.
fn app_with_files(files: Vec<ChangedFile>) -> App {
    let mut app = app();
    let effects = app.press("gf");
    let (thread, as_of) = ios(&effects)
        .into_iter()
        .find_map(|io| match io {
            Io::FetchFiles { thread, as_of, .. } => Some((thread, as_of)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fetches the files: {effects:?}"));
    app.send(Msg::FilesFetched {
        thread,
        as_of,
        files: Ok(files),
    });
    app
}

/// The whole texts `effects` fetch, by their contents path.
fn texts_fetched(effects: &Effects) -> Vec<String> {
    ios(effects)
        .into_iter()
        .filter_map(|io| match io {
            Io::FetchText { contents_path, .. } => Some(contents_path),
            _ => None,
        })
        .collect()
}

#[test]
fn gf_shows_a_pull_requests_files_and_a_new_one_whole() {
    let mut app = app();
    app.press("gf");
    assert!(shows(&app, "Fetching files…"), "{:#?}", rows(&app));

    let app = app_with_files(changed_files());

    assert!(
        shows(&app, "#1 Add polling · 4 files · +9 \u{2212}7"),
        "{:#?}",
        rows(&app)
    );
    assert!(shows(&app, "A docs/guide.md"), "{:#?}", rows(&app));
    assert!(shows(&app, "M src/ui.rs"));
    assert!(shows(&app, "D old.txt"));
    assert!(shows(&app, "R src/old.rs \u{2192} src/new.rs"));
    assert!(shows(&app, " docs/guide.md"), "the file's winbar");
    assert!(shows(&app, "Read me."), "{:#?}", rows(&app));
    assert!(!shows(&app, "+Read me."), "not as a diff");
}

#[test]
fn a_new_file_shows_what_it_adds_in_its_gutter() {
    let app = app_with_files(changed_files());

    assert!(shows(&app, "\u{258e} Read me."), "{:#?}", rows(&app));
    assert_eq!(
        style_of(&app, "\u{258e} Read").fg,
        style(&app, Group::ADDED).fg
    );
}

#[test]
fn a_changed_file_shows_its_diff_and_whole_shows_it_whole() {
    let mut app = app_with_files(changed_files());

    let effects = app.press("j");
    assert!(
        shows(&app, "-old") && shows(&app, "+new"),
        "{:#?}",
        rows(&app)
    );
    assert_eq!(
        texts_fetched(&effects),
        Vec::<String>::new(),
        "the diff needs nothing more"
    );

    let effects = app.press("<leader>w");
    assert_eq!(
        texts_fetched(&effects),
        ["/repos/wcgw/ghd/contents/src/ui.rs?ref=abc"]
    );
    assert!(shows(&app, "Fetching…"));
    app.send(Msg::TextFetched {
        thread: ThreadId(1),
        contents_path: "/repos/wcgw/ghd/contents/src/ui.rs?ref=abc".to_owned(),
        text: Ok(FileText::of("fn a() {}\nnew\n".to_owned())),
    });
    assert!(shows(&app, "  fn a() {}"), "{:#?}", rows(&app));
    assert!(
        shows(&app, "\u{258e} new"),
        "changed, in the gutter: {:#?}",
        rows(&app)
    );
    assert!(!shows(&app, "-old"));

    app.press("<leader>w");
    assert!(shows(&app, "-old"));
}

#[test]
fn diffs_are_highlighted_on_their_lines_background() {
    let mut app = app_with_files(changed_files());
    app.press("j");

    assert_eq!(style_of(&app, "@@ -1,2").fg, style(&app, hl::HUNK).fg);
    assert_eq!(
        style_of(&app, "fn a()").fg,
        style(&app, Group::KEYWORD).fg,
        "context"
    );
    let added = style_of(&app, "+new");
    assert_eq!(added.fg, style(&app, Group::ADDED).fg);
    assert_eq!(added.bg, style(&app, Group::DIFF_ADD).bg);
    let removed = style_of(&app, "-old");
    assert_eq!(removed.bg, style(&app, Group::DIFF_DELETE).bg);
    let row = u16::try_from(row_of(&app, "-old")).expect("on screen");
    assert_eq!(
        app.style_at(79, row).bg,
        style(&app, Group::DIFF_DELETE).bg,
        "across the window"
    );
}

#[test]
fn removed_and_renamed_files_say_so() {
    let mut app = app_with_files(changed_files());

    app.press("jj");
    assert!(shows(&app, "Deleted: 5 lines"), "{:#?}", rows(&app));

    app.press("j");
    assert!(shows(&app, "Renamed from src/old.rs"), "{:#?}", rows(&app));
}

#[test]
fn a_file_without_a_patch_says_so() {
    let mut app = app_with_files(vec![changed("logo.png", FileStatus::Modified, None)]);

    assert!(
        shows(
            &app,
            "No diff from GitHub (binary, or too large): gX opens it."
        ),
        "{:#?}",
        rows(&app)
    );
    app.press("<leader>w");
    app.send(Msg::TextFetched {
        thread: ThreadId(1),
        contents_path: "/repos/wcgw/ghd/contents/logo.png?ref=abc".to_owned(),
        text: Ok(FileText::Binary),
    });
    assert!(shows(&app, "Binary: gX opens it."), "{:#?}", rows(&app));
}

#[test]
fn a_pull_request_without_files_says_so() {
    let app = app_with_files(Vec::new());

    assert!(shows(&app, "No files."), "{:#?}", rows(&app));
}

#[test]
fn markdown_shown_whole_is_rendered() {
    let app = app_with_files(changed_files());

    assert!(shows(&app, "Guide"), "{:#?}", rows(&app));
    assert!(!shows(&app, "# Guide"));
}

#[test]
fn gshift_x_opens_the_file_on_github_and_marks_read() {
    let mut app = app_with_files(changed_files());
    app.press("j");

    let effects = app.press("gX");

    let anchor = changed_files()[1].anchor();
    assert_eq!(
        opened(&effects),
        [format!("https://github.com/wcgw/ghd/pull/1/files#{anchor}")]
    );
    assert_eq!(sent(&effects).len(), 1, "the pull request read");
}

#[test]
fn esc_goes_back_to_the_list_and_the_preview() {
    let mut app = app();
    app.press("p");
    app.press("gf");
    assert!(shows(&app, "Fetching files…"));

    app.press("<Esc>");

    assert!(shows(&app, "Old news"));
    assert!(
        rows(&app).iter().any(|row| row.starts_with(" Preview")),
        "{:#?}",
        rows(&app)
    );
}

#[test]
fn back_from_the_files_with_ctrl_t_or_gf() {
    let mut app = app_with_files(changed_files());

    app.press("<C-t>");
    assert!(shows(&app, "Old news"), "{:#?}", rows(&app));
    assert!(
        !shows(&app, "docs/guide.md"),
        "the window below goes: {:#?}",
        rows(&app)
    );

    app.press("gf");
    assert!(shows(&app, "docs/guide.md"), "fetched already");
    app.press("gf");
    assert!(shows(&app, "Old news"));
}

#[test]
fn only_pull_requests_have_files() {
    let mut app = app();
    app.press("j");

    let effects = app.press("gf");

    assert!(
        ios(&effects)
            .iter()
            .all(|io| !matches!(io, Io::FetchFiles { .. }))
    );
    assert!(cmdline(&app).contains("Only pull requests have files"));
}

#[test]
fn files_need_a_tall_enough_terminal() {
    let mut app = app_with(views(), false, 80, 10);
    app.send(fetched(notifications()));

    app.press("gf");

    assert!(cmdline(&app).contains("Files need a terminal at least 12 lines tall"));
}

#[test]
fn the_list_takes_no_more_than_its_files() {
    let app = app_with_files(changed_files());

    let rows = rows(&app);
    let last_file = row_of(&app, "R src/old.rs");
    assert!(
        rows[last_file + 1].starts_with('─'),
        "the line between the windows: {rows:#?}"
    );
}

#[test]
fn ctrl_l_fetches_the_files_again() {
    let mut app = app_with_files(changed_files());

    let effects = app.press("<C-l>");

    assert!(
        ios(&effects)
            .iter()
            .any(|io| matches!(io, Io::FetchFiles { .. })),
        "{effects:?}"
    );
}

#[test]
fn slash_searches_the_file_shown() {
    let mut app = app_with_files(changed_files());
    app.press("j<C-w>j");

    app.press("/new<CR>");

    let cursor = usize::from(app.screen().cursor.1);
    assert_eq!(cursor, row_of(&app, "+new"));
}
