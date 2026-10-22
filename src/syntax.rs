//! Syntax highlighting: what each piece of some code is, for a theme to color.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::LazyLock;

use ratatui::style::Style;
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, StyleModifier, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::theme;

/// The most code highlighted at once. Highlighting takes about a millisecond for every 6 lines, and is done before the
/// code is drawn.
const MAX_CODE_BYTES: usize = 32 * 1024;

/// The longest line highlighted: past one longer, like a minified script's, a file is left as it is.
const MAX_LINE_BYTES: usize = 4 * 1024;

/// How many pieces of code are kept highlighted, to be drawn again without highlighting them again; past that, they're
/// all forgotten.
const KEPT: usize = 256;

/// What a piece of code is, as far as its color goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Comment,
    /// Keywords, `self` and such.
    Keyword,
    String,
    /// Numbers, `true`, escapes in strings, and such.
    Constant,
    Function,
    /// Types, and keys: in JSON, YAML and such.
    Type,
    Operator,
    /// A line a diff adds.
    Inserted,
    /// A line a diff removes.
    Deleted,
}

impl Class {
    /// Every class, with the `TextMate` scopes that are of it.
    const SCOPES: [(Self, &'static str); 9] = [
        (Self::Comment, "comment"),
        (Self::Keyword, "keyword, storage, variable.language"),
        (Self::String, "string"),
        (Self::Constant, "constant"),
        (
            Self::Function,
            "entity.name.function, support.function, variable.function",
        ),
        (
            Self::Type,
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, \
             entity.name.interface, entity.name.tag, entity.other.inherited-class, support.type, support.class",
        ),
        (Self::Operator, "keyword.operator"),
        (Self::Inserted, "markup.inserted"),
        (Self::Deleted, "markup.deleted"),
    ];

    /// The color [`CLASSES`] gives pieces of `class`: its place in [`Self::SCOPES`], from 1, in red; black for none.
    fn marker(class: Option<Self>) -> Color {
        let red = class.map_or(0, |class| {
            let at = Self::SCOPES
                .iter()
                .position(|(of, _)| *of == class)
                .expect("every class has scopes");
            u8::try_from(at + 1).expect("fewer than 255 classes")
        });
        Color {
            r: red,
            g: 0,
            b: 0,
            a: 0xff,
        }
    }

    /// The class `marker` is the [`Self::marker`] of; `None` for none.
    fn marked(marker: Color) -> Option<Self> {
        let at = usize::from(marker.r).checked_sub(1)?;
        Self::SCOPES.get(at).map(|(class, _)| *class)
    }
}

/// The classes of the pieces of a line of code: the byte ranges in it of those of each [`Class`], in order; what's of
/// none is left out.
pub type Classes = Vec<(Range<usize>, Class)>;

/// Some code highlighted: the [`Classes`] of each of its lines.
pub type Highlighted = Rc<[Classes]>;

/// The syntaxes ghd knows: bat's.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// A syntect theme that marks each piece of code with its class, rather than coloring it: see [`Class::marker`].
static CLASSES: LazyLock<Theme> = LazyLock::new(|| Theme {
    settings: ThemeSettings {
        foreground: Some(Class::marker(None)),
        ..ThemeSettings::default()
    },
    scopes: Class::SCOPES
        .iter()
        .map(|&(class, scopes)| ThemeItem {
            scope: scopes.parse().expect("valid scope selectors"),
            style: StyleModifier {
                foreground: Some(Class::marker(Some(class))),
                background: None,
                font_style: None,
            },
        })
        .collect(),
    ..Theme::default()
});

thread_local! {
    /// The code highlighted so far, with the syntax it was highlighted as; `None` where it couldn't be.
    static KEPT_HIGHLIGHTED: RefCell<HashMap<String, (&'static str, Option<Highlighted>)>> =
        RefCell::new(HashMap::new());
}

/// `code` highlighted as `language`, as a Markdown code block names it (`rust`, `rs`, `toml`); `None` if ghd doesn't
/// know the language, or there's too much code to highlight, or it can't be.
pub fn highlight(language: &str, code: &str) -> Option<Highlighted> {
    if code.len() > MAX_CODE_BYTES {
        return None;
    }
    // GitHub's name for it, which bat doesn't know.
    let language = if language == "shell" {
        "bash"
    } else {
        language
    };
    let syntax = SYNTAXES.find_syntax_by_token(language)?;
    KEPT_HIGHLIGHTED.with_borrow_mut(|kept| {
        if let Some((name, highlighted)) = kept.get(code)
            && *name == syntax.name
        {
            return highlighted.clone();
        }
        if kept.len() >= KEPT {
            kept.clear();
        }
        let highlighted = classes(syntax, code);
        kept.insert(code.to_owned(), (&syntax.name, highlighted.clone()));
        highlighted
    })
}

/// The classes of the pieces of each line of `code`, as `syntax`; `None` if it can't be highlighted.
fn classes(syntax: &SyntaxReference, code: &str) -> Option<Highlighted> {
    let mut highlighter = HighlightLines::new(syntax, &CLASSES);
    LinesWithEndings::from(code)
        .map(|line| line_classes(&mut highlighter, line))
        .collect()
}

/// The classes of the pieces of `line`, the next of the code `highlighter` is at; `None` if it can't be highlighted.
fn line_classes(highlighter: &mut HighlightLines, line: &str) -> Option<Classes> {
    let pieces = highlighter.highlight_line(line, &SYNTAXES).ok()?;
    let end = line.trim_end_matches(['\n', '\r']).len();
    let mut at = 0;
    let mut classes: Classes = Vec::new();
    for (style, piece) in pieces {
        let range = at..(at + piece.len()).min(end);
        at += piece.len();
        let Some(class) = Class::marked(style.foreground) else {
            continue;
        };
        match classes.last_mut() {
            _ if range.is_empty() => {}
            // Of the same class as the piece just before it: one piece.
            Some((last, of)) if *of == class && last.end == range.start => {
                last.end = range.end;
            }
            _ => classes.push((range, class)),
        }
    }
    Some(classes)
}

/// A file's text or diff, highlighted as far down as it's been wanted: highlighting a long one whole would keep the
/// screen waiting.
pub struct Highlighting {
    text: String,
    parsing: Parsing,
    /// The classes of each line highlighted so far.
    done: Vec<Classes>,
    /// Where the next line to highlight starts in `text`, in bytes.
    at: usize,
    /// No more lines are highlighted: one was too long, or couldn't be.
    stopped: bool,
}

/// Where highlighting is at in some code.
enum Parsing {
    /// A file, whole.
    File(HighlightLines<'static>),
    /// A diff.
    Diff(Box<Sides>),
}

/// A diff's file before and after, as far as the hunk at hand shows them. Each hunk starts both again, as it shows
/// nothing of what comes before it.
struct Sides {
    syntax: &'static SyntaxReference,
    before: HighlightLines<'static>,
    after: HighlightLines<'static>,
}

impl Highlighting {
    /// For `text`, the file at `path`, in the language its name says (`Dockerfile`, `main.rs`), or else its first line
    /// (`#!/bin/sh`); `None` if ghd doesn't know it, or it's plain text.
    pub fn of_file(path: &str, text: &str) -> Option<Self> {
        let syntax = syntax_of(path, text.lines().next())?;
        Some(Self::new(
            text,
            Parsing::File(HighlightLines::new(syntax, &CLASSES)),
        ))
    }

    /// For `patch`, the unified diff of the file at `path`, in the language its name says; `None` if ghd doesn't know
    /// it, or it's plain text. Each line's classes are those of its code, past the `+`, `-` or space that starts it;
    /// a hunk's `@@` line has none.
    pub fn of_diff(path: &str, patch: &str) -> Option<Self> {
        let syntax = syntax_of(path, None)?;
        let parsing = Parsing::Diff(Box::new(Sides {
            syntax,
            before: HighlightLines::new(syntax, &CLASSES),
            after: HighlightLines::new(syntax, &CLASSES),
        }));
        Some(Self::new(patch, parsing))
    }

    fn new(text: &str, parsing: Parsing) -> Self {
        Self {
            text: text.to_owned(),
            parsing,
            done: Vec::new(),
            at: 0,
            stopped: false,
        }
    }

    /// Highlights the text down to line `count`, or as far as it can be.
    pub fn down_to(&mut self, count: usize) {
        while !self.stopped && self.done.len() < count && self.at < self.text.len() {
            let rest = &self.text[self.at..];
            let line = &rest[..rest.find('\n').map_or(rest.len(), |end| end + 1)];
            let classes = (line.len() <= MAX_LINE_BYTES)
                .then(|| self.parsing.next(line))
                .flatten();
            match classes {
                Some(classes) => self.done.push(classes),
                None => self.stopped = true,
            }
            self.at += line.len();
        }
    }

    /// The classes of the pieces of each line highlighted so far.
    pub fn lines(&self) -> &[Classes] {
        &self.done
    }
}

impl Parsing {
    /// The classes of the pieces of `line`, the next one; `None` if it can't be highlighted.
    fn next(&mut self, line: &str) -> Option<Classes> {
        match self {
            Self::File(highlighter) => line_classes(highlighter, line),
            Self::Diff(sides) => {
                let Sides {
                    syntax,
                    before,
                    after,
                } = &mut **sides;
                // Past the `+`, `-` or space that says which side it's on.
                let code = line.get(1..).unwrap_or_default();
                match line.as_bytes().first() {
                    Some(b'+') => line_classes(after, code),
                    Some(b'-') => line_classes(before, code),
                    // `@@ -1,3 +1,4 @@`: another hunk.
                    Some(b'@') => {
                        *before = HighlightLines::new(syntax, &CLASSES);
                        *after = HighlightLines::new(syntax, &CLASSES);
                        Some(Vec::new())
                    }
                    // "\ No newline at end of file".
                    Some(b'\\') => Some(Vec::new()),
                    // On both sides.
                    _ => {
                        line_classes(before, code)?;
                        line_classes(after, code)
                    }
                }
            }
        }
    }
}

/// The syntax of the file at `path`, by its name (`Dockerfile`) or extension (`main.rs`), or else by its
/// `first_line` (`#!/bin/sh`); `None` if ghd doesn't know it, or it's plain text.
fn syntax_of(path: &str, first_line: Option<&str>) -> Option<&'static SyntaxReference> {
    let name = path.rsplit('/').next().unwrap_or(path);
    SYNTAXES
        .find_syntax_by_extension(name)
        .or_else(|| {
            let (_, extension) = name.rsplit_once('.')?;
            SYNTAXES.find_syntax_by_extension(extension)
        })
        .or_else(|| SYNTAXES.find_syntax_by_first_line(first_line?))
        .filter(|syntax| !std::ptr::eq(*syntax, SYNTAXES.find_syntax_plain_text()))
}

/// Not the highlighter, which says nothing useful.
impl std::fmt::Debug for Highlighting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighting")
            .field("lines_done", &self.done.len())
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

/// `line` as spans in `style`, those of its pieces of a class in that class's `theme` style over it.
pub fn spans(
    line: &str,
    classes: &[(Range<usize>, Class)],
    style: Style,
    theme: &theme::Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(classes.len() * 2 + 1);
    let mut from = 0;
    for (range, class) in classes {
        if from < range.start {
            spans.push(Span::styled(line[from..range.start].to_owned(), style));
        }
        spans.push(Span::styled(
            line[range.clone()].to_owned(),
            style.patch(theme.syntax(*class)),
        ));
        from = range.end;
    }
    if from < line.len() {
        spans.push(Span::styled(line[from..].to_owned(), style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces of each line of `code`, highlighted as `language`, with their classes.
    fn pieces<'c>(language: &str, code: &'c str) -> Vec<Vec<(&'c str, Class)>> {
        let highlighted = highlight(language, code).expect("highlighted");
        code.lines()
            .zip(highlighted.iter())
            .map(|(line, classes)| {
                classes
                    .iter()
                    .map(|(range, class)| (&line[range.clone()], *class))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn rust_is_highlighted() {
        let code = "fn main() {\n    let s = \"hi\"; // 1\n    s.len() + 1\n}\n";

        let pieces = pieces("rust", code);

        assert!(pieces[0].contains(&("fn", Class::Keyword)), "{pieces:?}");
        assert!(pieces[0].contains(&("main", Class::Function)), "{pieces:?}");
        assert!(pieces[1].contains(&("let", Class::Keyword)), "{pieces:?}");
        assert!(
            pieces[1]
                .iter()
                .any(|(text, class)| text.contains("hi") && *class == Class::String)
        );
        assert!(
            pieces[1]
                .iter()
                .any(|(text, class)| text.contains("// 1") && *class == Class::Comment)
        );
        assert!(pieces[2].contains(&("+", Class::Operator)), "{pieces:?}");
        assert!(pieces[2].contains(&("1", Class::Constant)), "{pieces:?}");
    }

    #[test]
    fn what_highlighting_starts_carries_over_lines() {
        let code = "/* a\n   b */\nfn f() {}\n";

        let pieces = pieces("rs", code);

        assert_eq!(pieces[1], [("   b */", Class::Comment)]);
        assert!(pieces[2].contains(&("fn", Class::Keyword)), "{pieces:?}");
    }

    #[test]
    fn diffs_add_and_remove_lines() {
        let pieces = pieces("diff", "-old\n+new\n");

        assert_eq!(
            pieces,
            [
                vec![("-old", Class::Deleted)],
                vec![("+new", Class::Inserted)]
            ]
        );
    }

    #[test]
    fn languages_go_by_their_names_and_extensions() {
        for language in [
            "toml",
            "ts",
            "typescript",
            "yaml",
            "yml",
            "proto",
            "dockerfile",
            "shell",
            "sh",
        ] {
            assert!(highlight(language, "x\n").is_some(), "{language}");
        }
        assert!(highlight("no-such-language", "x\n").is_none());
        assert!(highlight("", "x\n").is_none());
    }

    #[test]
    fn line_breaks_are_left_out() {
        let highlighted = highlight("rust", "// a\r\n").expect("highlighted");

        assert_eq!(highlighted[0], [(0..4, Class::Comment)]);
    }

    #[test]
    fn too_much_code_is_left_as_it_is() {
        let code = "// a comment\n".repeat(MAX_CODE_BYTES / 13 + 1);

        assert!(highlight("rust", &code).is_none());
        assert!(highlight("rust", &code[..MAX_CODE_BYTES / 13 * 13]).is_some());
    }

    #[test]
    fn files_go_by_their_name_extension_or_first_line() {
        for (path, text) in [
            ("src/main.rs", "fn main() {}\n"),
            ("Dockerfile", "FROM scratch\n"),
            ("deploy/Cargo.lock", "version = 4\n"),
            ("bin/run", "#!/bin/sh\nexit 0\n"),
        ] {
            assert!(Highlighting::of_file(path, text).is_some(), "{path}");
        }
        assert!(
            Highlighting::of_file("LICENSE", "MIT\n").is_none(),
            "unknown"
        );
        assert!(
            Highlighting::of_file("notes.txt", "Plain.\n").is_none(),
            "plain text"
        );
    }

    #[test]
    fn files_are_highlighted_as_far_down_as_wanted() {
        let text = "/* a\n   b */\nfn f() {}\n";
        let mut highlighting = Highlighting::of_file("a.rs", text).expect("Rust");

        highlighting.down_to(1);
        assert_eq!(
            highlighting.lines(),
            [vec![(0..4, Class::Comment)]],
            "no further"
        );

        highlighting.down_to(10);
        let done = highlighting.lines();
        assert_eq!(done.len(), 3, "as far as there are lines");
        assert_eq!(done[1], [(0..7, Class::Comment)], "still in the comment");
        assert_eq!(done[2][0], (0..2, Class::Keyword));
    }

    #[test]
    fn files_stop_at_a_line_too_long() {
        let text = format!(
            "let a = 1;\nlet b = \"{}\";\nlet c = 3;\n",
            "x".repeat(MAX_LINE_BYTES)
        );
        let mut highlighting = Highlighting::of_file("a.rs", &text).expect("Rust");

        highlighting.down_to(3);

        assert_eq!(highlighting.lines().len(), 1);
    }

    /// The pieces of the code of each line of `patch`, past its first character, highlighted as a diff of a Rust
    /// file, with their classes.
    fn diff_pieces(patch: &str) -> Vec<Vec<(&str, Class)>> {
        let mut highlighting = Highlighting::of_diff("src/a.rs", patch).expect("Rust");
        highlighting.down_to(usize::MAX);
        patch
            .lines()
            .zip(highlighting.lines())
            .map(|(line, classes)| {
                classes
                    .iter()
                    .map(|(range, class)| (&line[1..][range.clone()], *class))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn diffs_are_highlighted_side_by_side() {
        let pieces = diff_pieces(
            "@@ -1,4 +1,4 @@ fn main() {\n /* both\n-   before */ 1\n+   after */ fn\n fn f() {}",
        );

        assert_eq!(pieces[0], [], "the hunk's line");
        assert_eq!(pieces[1], [("/* both", Class::Comment)]);
        assert_eq!(
            pieces[2][0],
            ("   before */", Class::Comment),
            "the comment goes on before"
        );
        assert_eq!(pieces[3][0], ("   after */", Class::Comment), "and after");
        assert!(pieces[3].contains(&("fn", Class::Keyword)), "{pieces:?}");
        assert_eq!(pieces[4][0], ("fn", Class::Keyword), "both sides closed it");
    }

    #[test]
    fn diff_sides_and_hunks_start_apart() {
        let pieces = diff_pieces(
            "@@ -1 +1 @@\n-/* before only\n+fn after() {}\n@@ -9 +9 @@\n+/* open\n@@ -20 +20 @@\n fn f() {}",
        );

        assert_eq!(
            pieces[2][0],
            ("fn", Class::Keyword),
            "not in the comment before"
        );
        assert_eq!(
            pieces[6][0],
            ("fn", Class::Keyword),
            "not in the last hunk's comment"
        );
    }

    #[test]
    fn diffs_go_by_the_file_name_only() {
        assert!(Highlighting::of_diff("Dockerfile", "@@ -1 +1 @@\n+FROM scratch").is_some());
        assert!(Highlighting::of_diff("bin/run", "@@ -0,0 +1 @@\n+#!/bin/sh").is_none());
    }

    #[test]
    fn spans_take_their_class_style_over_the_line_style() {
        let theme = theme::Theme {
            syntax_keyword: Style::new().fg(ratatui::style::Color::Magenta),
            ..theme::Theme::default()
        };
        let base = Style::new().italic();

        let spans = spans("let a", &[(0..3, Class::Keyword)], base, &theme);

        assert_eq!(
            spans,
            [
                Span::styled("let", base.fg(ratatui::style::Color::Magenta)),
                Span::styled(" a", base),
            ]
        );
    }

    #[test]
    fn code_highlighted_again_is_the_same() {
        let first = highlight("rust", "let a = 1;\n").expect("highlighted");
        let again = highlight("rust", "let a = 1;\n").expect("highlighted");

        assert!(Rc::ptr_eq(&first, &again), "kept");
        let as_toml = highlight("toml", "let a = 1;\n").expect("highlighted");
        assert_ne!(as_toml, first, "highlighted as another language");
    }
}
