//! Markdown as GitHub users write it, drawn for the terminal in a theme's styles.

use std::ops::Range;

use pulldown_cmark::{Event, Options as ParseOptions, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tui_markdown::{AlertKind, Options, StyleSheet};

use crate::theme::Theme;

/// The lines `markdown` draws in `theme`, its tables fitted to `width` cells. As github.com shows issues, pull
/// requests and comments, a new line within a paragraph breaks the line. HTML outside code is rewritten, as a
/// terminal can't draw it: comments are dropped, `<br>` breaks the line, `<img>` shows its description, and any
/// other tag is dropped for its text, so a `<details>` block shows open, its `<summary>` on a line of its own.
pub fn lines(markdown: &str, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let markdown = as_github_shows(markdown);
    let options = Options::new(Styles(*theme)).table_width(width);
    tui_markdown::from_str_with_options(&markdown, &options)
        .lines
        .into_iter()
        .map(|line| Line {
            spans: line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect(),
            ..line
        })
        .collect()
}

/// A line break within a paragraph, in Markdown: two spaces, then a new line.
const HARD_BREAK: &str = "  \n";

/// `markdown`, with new lines within paragraphs made line breaks and its HTML rewritten (see [`lines`]). Only
/// what the Markdown parser takes for those is touched: never code, where `Vec<T>` is just text.
fn as_github_shows(markdown: &str) -> String {
    let mut rewritten: Vec<(Range<usize>, String)> = Vec::new();
    let mut in_html_block = false;
    for (event, range) in Parser::new_ext(markdown, parse_options()).into_offset_iter() {
        match event {
            // The whole block at once: a comment can run across its lines.
            Event::Start(Tag::HtmlBlock) => {
                in_html_block = true;
                rewritten.push((range.clone(), as_text(&markdown[range], "\n")));
            }
            Event::End(TagEnd::HtmlBlock) => in_html_block = false,
            Event::InlineHtml(html) if !in_html_block => {
                rewritten.push((range, as_text(&html, HARD_BREAK)));
            }
            Event::SoftBreak => rewritten.push((range, HARD_BREAK.to_owned())),
            _ => {}
        }
    }
    let mut out = String::with_capacity(markdown.len());
    let mut copied_to = 0;
    for (range, text) in rewritten {
        out.push_str(&markdown[copied_to..range.start]);
        out.push_str(&text);
        copied_to = range.end;
    }
    out.push_str(&markdown[copied_to..]);
    out
}

/// The Markdown extensions `tui-markdown` 0.3 parses with, which decide what's HTML, code or a paragraph: the
/// rewriting must see the same.
fn parse_options() -> ParseOptions {
    ParseOptions::ENABLE_STRIKETHROUGH
        | ParseOptions::ENABLE_TASKLISTS
        | ParseOptions::ENABLE_HEADING_ATTRIBUTES
        | ParseOptions::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | ParseOptions::ENABLE_SUPERSCRIPT
        | ParseOptions::ENABLE_SUBSCRIPT
        | ParseOptions::ENABLE_MATH
        | ParseOptions::ENABLE_FOOTNOTES
        | ParseOptions::ENABLE_DEFINITION_LIST
        | ParseOptions::ENABLE_GFM
        | ParseOptions::ENABLE_TABLES
}

/// `html` as text: comments dropped, `<br>` as `line_break`, `<img>` as `[img]` and its description, the end of a
/// `<summary>` as a new line, and any other tag dropped.
fn as_text(html: &str, line_break: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let Some(end) = tag_end(rest) else {
            // Not a tag: a `<` on its own.
            out.push('<');
            rest = &rest[1..];
            continue;
        };
        let tag = &rest[..end];
        let closing = tag.starts_with("</");
        let name: String = tag
            .trim_start_matches(['<', '/'])
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        match (name.as_str(), closing) {
            ("br", _) => out.push_str(line_break),
            ("img", false) => {
                out.push_str("[img]");
                if let Some(alt) = attribute(tag, "alt").filter(|alt| !alt.is_empty()) {
                    out.push(' ');
                    out.push_str(alt);
                }
            }
            ("summary", true) => out.push('\n'),
            _ => {}
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Where the tag `html` starts with ends, just past its `>`; `None` if it doesn't start with a tag. A `>` in a
/// quoted attribute doesn't end it.
fn tag_end(html: &str) -> Option<usize> {
    let after = html.strip_prefix('<')?;
    let after = after.strip_prefix('/').unwrap_or(after);
    if !after.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut quote = None;
    for (at, c) in html.char_indices() {
        match (c, quote) {
            ('"' | '\'', None) => quote = Some(c),
            (c, Some(open)) if c == open => quote = None,
            ('>', None) => return Some(at + 1),
            _ => {}
        }
    }
    None
}

/// The value of the attribute `name` in the tag `tag`, quoted or not; `None` if it has none.
fn attribute<'t>(tag: &'t str, name: &str) -> Option<&'t str> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let start = from + found;
        from = start + name.len();
        // A whole attribute name, not the end of another (`data-alt`).
        if !lower[..start].ends_with(char::is_whitespace) {
            continue;
        }
        let value = lower[from..].trim_start().strip_prefix('=')?.trim_start();
        let value_start = tag.len() - value.len();
        return Some(match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let inside = &tag[value_start + 1..];
                &inside[..inside.find(quote).unwrap_or(inside.len())]
            }
            _ => {
                let unquoted = &tag[value_start..];
                &unquoted[..unquoted
                    .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .unwrap_or(unquoted.len())]
            }
        });
    }
    None
}

/// A theme, as `tui-markdown` asks for styles. Every style comes from the theme, so that a theme of the terminal's
/// own colors stays that way.
#[derive(Debug, Clone, Copy)]
struct Styles(Theme);

impl StyleSheet for Styles {
    fn heading(&self, _level: u8) -> Style {
        self.0.markdown_heading
    }

    fn code(&self) -> Style {
        self.0.markdown_code
    }

    fn link(&self) -> Style {
        self.0.markdown_link
    }

    fn blockquote(&self) -> Style {
        self.0.markdown_quote
    }

    fn heading_meta(&self) -> Style {
        self.0.markdown_muted
    }

    fn metadata_block(&self) -> Style {
        self.0.markdown_muted
    }

    fn html(&self) -> Style {
        self.0.markdown_muted
    }

    fn math_inline(&self) -> Style {
        self.0.markdown_code
    }

    fn math_display(&self) -> Style {
        self.0.markdown_code
    }

    fn footnote_ref(&self) -> Style {
        self.0.markdown_muted
    }

    fn footnote_def(&self) -> Style {
        self.0.markdown_muted
    }

    fn definition_term(&self) -> Style {
        self.0.markdown_heading
    }

    fn definition_description(&self) -> Style {
        Style::new()
    }

    fn alert(&self, _kind: AlertKind) -> Style {
        self.0.markdown_quote
    }

    /// None: the emoji `tui-markdown` has are two cells wide in some terminals, one in others.
    fn alert_icon(&self, _kind: AlertKind) -> &str {
        ""
    }

    fn table_header(&self) -> Style {
        self.0.markdown_heading
    }

    fn table_cell(&self) -> Style {
        Style::new()
    }

    fn table_border(&self) -> Style {
        self.0.markdown_muted
    }

    fn list_marker(&self) -> Style {
        self.0.markdown_muted
    }

    fn image_alt(&self) -> Style {
        self.0.markdown_muted
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    fn text(markdown: &str) -> Vec<String> {
        lines(markdown, &Theme::default(), 60)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn comments_are_dropped() {
        let shown = text("Fixes it.\n\n<!-- Describe your change -->\n\nAnd <!-- this --> that.");

        assert!(
            !shown.iter().any(|line| line.contains("Describe")),
            "{shown:#?}"
        );
        assert!(shown.contains(&"Fixes it.".to_owned()), "{shown:#?}");
        assert!(shown.contains(&"And  that.".to_owned()), "{shown:#?}");
    }

    #[test]
    fn comment_across_lines_is_dropped() {
        let shown = text("<!--\nThanks for\ncontributing!\n-->\nBody");

        assert!(
            !shown.iter().any(|line| line.contains("contributing")),
            "{shown:#?}"
        );
        assert!(shown.iter().any(|line| line.contains("Body")), "{shown:#?}");
    }

    #[test]
    fn details_show_open_with_their_summary() {
        let shown = text("<details>\n<summary>Logs</summary>\n\nIt failed.\n\n</details>");

        assert!(shown.contains(&"Logs".to_owned()), "{shown:#?}");
        assert!(shown.contains(&"It failed.".to_owned()), "{shown:#?}");
        assert!(!shown.iter().any(|line| line.contains('<')), "{shown:#?}");
    }

    #[test]
    fn new_line_breaks_the_line_as_on_github() {
        assert_eq!(text("one\ntwo"), ["one", "two"]);
        assert_eq!(
            text("```\na\nb\n```"),
            ["```", "a", "b", "```"],
            "code as it is"
        );
    }

    #[test]
    fn br_breaks_the_line() {
        assert_eq!(text("one<br>two<br/>three"), ["one", "two", "three"]);
    }

    #[test]
    fn img_shows_its_description() {
        let shown = text(r#"See <img width="200" alt="the diagram" src="d.png"> here."#);

        assert_eq!(shown, ["See [img] the diagram here."]);
        assert_eq!(text("<img src='d.png'>"), ["[img]"]);
    }

    #[test]
    fn other_tags_leave_their_text() {
        assert_eq!(text("Press <kbd>Ctrl</kbd>-<kbd>C</kbd>"), ["Press Ctrl-C"]);
    }

    #[test]
    fn code_is_never_rewritten() {
        let shown = text("Use `<br>` here.\n\n```\nVec<T> <!-- kept -->\n```");

        assert!(shown.contains(&"Use <br> here.".to_owned()), "{shown:#?}");
        assert!(
            shown
                .iter()
                .any(|line| line.contains("Vec<T> <!-- kept -->")),
            "{shown:#?}"
        );
    }

    #[test]
    fn markdown_is_drawn_in_the_theme() {
        let theme = Theme {
            markdown_heading: Style::new().fg(Color::Red),
            markdown_code: Style::new().fg(Color::Green),
            ..Theme::default()
        };

        let drawn = lines("# Title\n\nRun `ghd`.", &theme, 60);

        // As drawn: the span's style over its line's.
        let style_of = |text: &str| {
            drawn.iter().find_map(|line| {
                let span = line.spans.iter().find(|span| span.content.contains(text))?;
                Some(line.style.patch(span.style).fg)
            })
        };
        assert_eq!(style_of("Title"), Some(Some(Color::Red)));
        assert_eq!(style_of("ghd"), Some(Some(Color::Green)));
    }

    #[test]
    fn alerts_have_no_icon() {
        let shown = text("> [!NOTE]\n> Mind the gap.");

        assert!(shown.iter().any(|line| line.contains("Note")), "{shown:#?}");
        assert!(
            !shown.iter().any(|line| line.contains('\u{2139}')),
            "{shown:#?}"
        );
    }
}
