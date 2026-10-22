//! Syntax highlighting: what each piece of some code is, for a theme to color.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::LazyLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, StyleModifier, Theme, ThemeItem, ThemeSettings};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// The most code highlighted at once. Highlighting takes about a millisecond for every 6 lines, and is done before the
/// code is drawn.
const MAX_CODE_BYTES: usize = 32 * 1024;

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

/// Each line of some code, as the byte ranges in it of the pieces of each [`Class`], in order; what's of none is left
/// out.
pub type Highlighted = Rc<[Vec<(Range<usize>, Class)>]>;

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
        .map(|line| {
            let pieces = highlighter.highlight_line(line, &SYNTAXES).ok()?;
            let end = line.trim_end_matches(['\n', '\r']).len();
            let mut at = 0;
            let mut classes: Vec<(Range<usize>, Class)> = Vec::new();
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
        })
        .collect()
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
    fn code_highlighted_again_is_the_same() {
        let first = highlight("rust", "let a = 1;\n").expect("highlighted");
        let again = highlight("rust", "let a = 1;\n").expect("highlighted");

        assert!(Rc::ptr_eq(&first, &again), "kept");
        let as_toml = highlight("toml", "let a = 1;\n").expect("highlighted");
        assert_ne!(as_toml, first, "highlighted as another language");
    }
}
