//! Markdown as GitHub users write it, drawn as lines of a kit buffer: laid out by `wcgw-kit-text`, in the
//! colorscheme's groups, with GitHub's own rules applied first (see [`as_github_shows`]), its code highlighted, its
//! links made something `:open` follows, and voight's Heatmap shading it.

use std::ops::Range;

use pulldown_cmark::{Event, Options as ParseOptions, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::Line;
use wcgw_kit::doc;
use wcgw_kit::hl::{Group, Theme};
use wcgw_kit_text::{CellSize, Options, Styles};

use crate::assessment::Window;
use crate::drawn::Finder;
use crate::heatmap::{self, Heat};
use crate::syntax::{self, Class};

/// How Markdown is drawn: the styles of its parts, of its code by class, and of the Heatmap, as the colorscheme
/// gives them.
#[derive(Debug, Clone)]
pub struct Look {
    pub styles: Styles,
    /// The style of each class of code, over the code's own.
    pub code: Vec<(Class, Style)>,
    pub heat: Heat,
}

impl Look {
    /// The look `theme` gives, `heat` the Heatmap's styles.
    pub fn of(theme: &Theme, heat: Heat) -> Self {
        Self {
            styles: wcgw_kit_text::kit::styles(theme, None),
            code: Class::all()
                .map(|class| (class, theme.style(class.group())))
                .collect(),
            heat,
        }
    }

    /// The style of code of `class`.
    fn code_style(&self, class: Class) -> Style {
        self.code
            .iter()
            .find(|(of, _)| *of == class)
            .map_or_else(Style::default, |(_, style)| *style)
    }
}

/// Markdown drawn: its lines, and for each the line of the Markdown it draws, if it draws one: not if it only lays
/// out what's drawn, like a blank line between paragraphs, or a table's borders.
#[derive(Debug)]
pub struct Drawing<T> {
    pub lines: Vec<doc::Line<T>>,
    pub sources: Vec<Option<usize>>,
}

/// `markdown` drawn in `look`, `width` cells wide, as the Heatmap of `heat`: the windows of an Assessment of the text
/// [`as_github_shows`] makes of it; none for no Heatmap. Code blocks in a language ghd knows are highlighted, and a
/// link's text and number refer to what `link` makes of its address.
///
/// As github.com shows issues, pull requests and comments, a new line within a paragraph breaks the line. HTML outside
/// code is rewritten, as a terminal can't draw it: comments are dropped, `<br>` breaks the line, `<img>` shows its
/// description, and any other tag is dropped for its text, so a `<details>` block shows open, its `<summary>` on a
/// line of its own.
pub fn draw<T>(
    markdown: &str,
    look: &Look,
    width: u16,
    heat: &[Window],
    link: impl Fn(&str) -> T,
) -> Drawing<T> {
    let rewritten = Rewritten::of(markdown);
    let mut document = wcgw_kit_text::Document::markdown(&rewritten.text, &look.styles);
    document.highlight_code(|language, code| highlighted(language?, code, look));
    let mut lines = document
        .render(&Options {
            width,
            cell: CellSize::default(),
            pixel_ratio: 1.0,
            max_picture_rows: 0,
            pictures: &[],
        })
        .lines;
    let sources = sources(&lines, &rewritten, markdown);
    let urls: Vec<String> = document
        .links()
        .iter()
        .map(|link| link.url.clone())
        .collect();
    // Found by their styles, before the Heatmap changes them.
    let links: Vec<Vec<(Range<usize>, String)>> = lines
        .iter()
        .map(|line| link_marks(line, &urls, &look.styles))
        .collect();
    heatmap::shade(&mut lines, &rewritten.text, heat, &look.heat);
    let lines = lines
        .into_iter()
        .zip(links)
        .map(|(line, links)| kit_line(&line, &links, &link))
        .collect();
    Drawing { lines, sources }
}

/// `code`, in `language`, in pieces, each in its class's style if ghd knows the language; `None` otherwise.
fn highlighted(language: &str, code: &str, look: &Look) -> Option<Vec<(String, Style)>> {
    let classes = syntax::highlight(language, code)?;
    let mut pieces = Vec::new();
    for (index, line) in code.split('\n').enumerate() {
        if index > 0 {
            pieces.push(("\n".to_owned(), Style::default()));
        }
        let line_classes = classes.get(index).map_or(&[][..], Vec::as_slice);
        for (text, class) in syntax::pieces(line, line_classes) {
            let style = class.map_or_else(Style::default, |class| look.code_style(class));
            pieces.push((text.to_owned(), style));
        }
    }
    Some(pieces)
}

/// The line of `markdown` each of `lines`, which draw `rewritten`, draws, if it draws one.
fn sources(lines: &[Line<'static>], rewritten: &Rewritten, markdown: &str) -> Vec<Option<usize>> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(markdown.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let mut finder = Finder::new(&rewritten.text);
    lines
        .iter()
        .map(|line| {
            finder.next_line();
            let mut first = None;
            // Each span looked for, for the finder to keep up.
            for span in &line.spans {
                if let Some(start) = finder.find(&span.content) {
                    first.get_or_insert(start);
                }
            }
            first.map(|start| {
                let at = rewritten.source_of(start);
                line_starts.partition_point(|&line_start| line_start <= at) - 1
            })
        })
        .collect()
}

/// The links on `line`, by their byte ranges in its text: each link's number, `[3]`, and its text just before it,
/// with the address of `links` it numbers.
fn link_marks(
    line: &Line<'static>,
    links: &[String],
    styles: &Styles,
) -> Vec<(Range<usize>, String)> {
    let mut found = Vec::new();
    let mut at = 0;
    let mut previous: Option<Range<usize>> = None;
    for span in &line.spans {
        let range = at..at + span.content.len();
        at = range.end;
        let number = span
            .content
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|number| number.parse::<usize>().ok());
        match number.and_then(|number| links.get(number.checked_sub(1)?)) {
            Some(url) if span.style == styles.link_number => {
                let start = previous.take().map_or(range.start, |text| text.start);
                found.push((start..range.end, url.clone()));
            }
            _ => previous = Some(range),
        }
    }
    found
}

/// `line` as a kit line, in its own colours, its `links` referring to what `link` makes of their address.
fn kit_line<T>(
    line: &Line<'static>,
    links: &[(Range<usize>, String)],
    link: &impl Fn(&str) -> T,
) -> doc::Line<T> {
    let mut cuts: Vec<usize> = links
        .iter()
        .flat_map(|(range, _)| [range.start, range.end])
        .collect();
    cuts.sort_unstable();
    let mut spans = Vec::with_capacity(line.spans.len());
    let mut at = 0;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let text = span.content.as_ref();
        let end = at + text.len();
        let mut from = at;
        for cut in cuts
            .iter()
            .copied()
            .filter(|&cut| at < cut && cut < end)
            .chain([end])
        {
            if cut <= from || !text.is_char_boundary(cut - at) || !text.is_char_boundary(from - at)
            {
                continue;
            }
            let piece = doc::Span::new(&text[from - at..cut - at], Group::NORMAL).with_style(style);
            spans.push(
                match links.iter().find(|(range, _)| range.contains(&from)) {
                    Some((_, url)) => piece.with_target(link(url)),
                    None => piece,
                },
            );
            from = cut;
        }
        at = end;
    }
    doc::Line::Text(spans)
}

/// Whether `markdown` mentions `handle`, a login (`octocat`) or a team (`github/docs`), as GitHub notifies for:
/// `@` and the handle, ignoring case, not within a longer name, and not in code.
pub fn mentions(markdown: &str, handle: &str) -> bool {
    // The text outside code, a space where code was, so that nothing joins across it.
    let mut text = String::with_capacity(markdown.len());
    let mut in_code_block = false;
    for event in Parser::new_ext(markdown, parse_options()) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Text(part) if !in_code_block => text.push_str(&part),
            _ => text.push(' '),
        }
    }
    let text = text.to_lowercase();
    let wanted = format!("@{}", handle.to_lowercase());
    text.match_indices(&wanted).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + wanted.len()..].chars().next();
        // `me@octocat.com` isn't a mention, nor is `@octocat-bot` one of `@octocat`, nor `@octocat/docs` (a team).
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/'))
    })
}

/// A line break within a paragraph, in Markdown: two spaces, then a new line.
const HARD_BREAK: &str = "  \n";

/// `markdown`, with new lines within paragraphs made line breaks and its HTML rewritten (see [`lines`]). Only
/// what the Markdown parser takes for those is touched: never code, where `Vec<T>` is just text.
pub fn as_github_shows(markdown: &str) -> String {
    Rewritten::of(markdown).text
}

/// Markdown as GitHub shows it (see [`as_github_shows`]), and where it was rewritten.
struct Rewritten {
    text: String,
    /// Each rewrite, in order: the range of the Markdown it rewrote, and that of what it wrote in `text`.
    edits: Vec<(Range<usize>, Range<usize>)>,
}

impl Rewritten {
    fn of(markdown: &str) -> Self {
        let mut rewrites: Vec<(Range<usize>, String)> = Vec::new();
        let mut in_html_block = false;
        for (event, range) in Parser::new_ext(markdown, parse_options()).into_offset_iter() {
            match event {
                // The whole block at once: a comment can run across its lines.
                Event::Start(Tag::HtmlBlock) => {
                    in_html_block = true;
                    rewrites.push((range.clone(), as_text(&markdown[range], "\n")));
                }
                Event::End(TagEnd::HtmlBlock) => in_html_block = false,
                Event::InlineHtml(html) if !in_html_block => {
                    rewrites.push((range, as_text(&html, HARD_BREAK)));
                }
                Event::SoftBreak => rewrites.push((range, HARD_BREAK.to_owned())),
                _ => {}
            }
        }
        let mut text = String::with_capacity(markdown.len());
        let mut edits = Vec::with_capacity(rewrites.len());
        let mut copied_to = 0;
        for (range, rewrite) in rewrites {
            text.push_str(&markdown[copied_to..range.start]);
            let start = text.len();
            text.push_str(&rewrite);
            edits.push((range.clone(), start..text.len()));
            copied_to = range.end;
        }
        text.push_str(&markdown[copied_to..]);
        Self { text, edits }
    }

    /// Where in the Markdown what's at `offset` in the text comes from; the start of what was rewritten, for what a
    /// rewrite wrote.
    fn source_of(&self, offset: usize) -> usize {
        let after = self
            .edits
            .partition_point(|(_, written)| written.start <= offset);
        match after.checked_sub(1).map(|last| &self.edits[last]) {
            None => offset,
            Some((rewrote, written)) if offset < written.end => rewrote.start,
            Some((rewrote, written)) => rewrote.end + (offset - written.end),
        }
    }
}

/// The Markdown extensions `wcgw-kit-text` parses with, which decide what's HTML, code or a paragraph: the rewriting
/// must see the same.
fn parse_options() -> ParseOptions {
    ParseOptions::ENABLE_TABLES
        | ParseOptions::ENABLE_STRIKETHROUGH
        | ParseOptions::ENABLE_TASKLISTS
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
        return Some(if let Some(quote @ ('"' | '\'')) = value.chars().next() {
            let inside = &tag[value_start + 1..];
            &inside[..inside.find(quote).unwrap_or(inside.len())]
        } else {
            let unquoted = &tag[value_start..];
            &unquoted[..unquoted
                .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .unwrap_or(unquoted.len())]
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    fn look() -> Look {
        Look::of(&Theme::new(), Heat::default())
    }

    /// The text of each line `markdown` draws, 60 cells wide.
    fn text(markdown: &str) -> Vec<String> {
        lines_of(&draw(markdown, &look(), 60, &[], str::to_owned).lines)
    }

    fn lines_of(lines: &[doc::Line<String>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                doc::Line::Text(spans) => spans.iter().map(|span| span.text.as_str()).collect(),
                doc::Line::Image { .. } => "[picture]".to_owned(),
            })
            .collect()
    }

    /// The spans of `lines`.
    fn spans(lines: &[doc::Line<String>]) -> impl Iterator<Item = &doc::Span<String>> {
        lines.iter().flat_map(|line| match line {
            doc::Line::Text(spans) => spans.as_slice(),
            doc::Line::Image { .. } => &[],
        })
    }

    #[test]
    fn comments_are_dropped() {
        let shown = text("Fixes it.\n\n<!-- Describe your change -->\n\nAnd <!-- this --> that.");

        assert_eq!(shown, ["Fixes it.", "", "And that."]);
        assert_eq!(text("<!--\nThanks for\ncontributing!\n-->\nBody"), ["Body"]);
    }

    #[test]
    fn details_show_open_with_their_summary() {
        let shown = text("<details>\n<summary>Logs</summary>\n\nIt failed.\n\n</details>");

        assert_eq!(shown, ["Logs", "", "It failed."]);
    }

    #[test]
    fn new_line_breaks_the_line_as_on_github() {
        assert_eq!(text("one\ntwo"), ["one", "two"]);
        assert_eq!(text("```\na\nb\n```"), ["a", "b"], "code as it is");
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

        assert_eq!(shown, ["Use <br> here.", "", "Vec<T> <!-- kept -->"]);
    }

    #[test]
    fn markdown_is_drawn_in_the_colorschemes_groups() {
        let theme = Theme::new();
        let drawn = draw("# Title\n\nRun `ghd`.", &look(), 60, &[], str::to_owned);

        let style_of = |text: &str| {
            spans(&drawn.lines)
                .find(|span| span.text.contains(text))
                .and_then(|span| span.style)
        };
        assert_eq!(
            style_of("Title").and_then(|style| style.fg),
            theme.style(Group::MARKUP_HEADING).fg
        );
        assert_eq!(
            style_of("ghd").and_then(|style| style.fg),
            theme.style(Group::MARKUP_RAW).fg
        );
    }

    /// The pieces of `lines` in a colour of one of `groups`, with that group.
    fn coloured<'d>(lines: &'d [doc::Line<String>], groups: &[Group]) -> Vec<(&'d str, Group)> {
        let theme = Theme::new();
        spans(lines)
            .filter_map(|span| {
                let fg = span.style.and_then(|style| style.fg);
                let group = groups
                    .iter()
                    .find(|group| fg.is_some() && theme.style(**group).fg == fg)?;
                Some((span.text.as_str(), *group))
            })
            .collect()
    }

    #[test]
    fn code_in_a_known_language_is_highlighted() {
        let markdown = "Code:\n\n```rust title=x\nlet a = 1; // one\n```\n\n> ```rust\n> fn f() {}\n> ```\n\n```nope\nlet b\n```\n";

        let drawn = draw(markdown, &look(), 60, &[], str::to_owned);

        let groups = [
            Group::KEYWORD,
            Group::OPERATOR,
            Group::CONSTANT,
            Group::FUNCTION,
        ];
        assert_eq!(
            coloured(&drawn.lines, &groups),
            [
                ("let", Group::KEYWORD),
                ("=", Group::OPERATOR),
                ("1", Group::CONSTANT),
                ("fn", Group::KEYWORD),
                ("f", Group::FUNCTION),
            ]
        );
        assert!(
            spans(&drawn.lines).any(|span| span.text == "// one"
                && span.style.and_then(|style| style.fg) == Theme::new().style(Group::COMMENT).fg),
            "comments too"
        );
        assert!(
            lines_of(&drawn.lines).contains(&"let b".to_owned()),
            "a language ghd doesn't know"
        );
    }

    #[test]
    fn code_too_wide_is_cut_and_stays_highlighted() {
        let markdown = format!("```rust\nlet {} = 1;\n```\n", "a".repeat(70));

        let drawn = draw(&markdown, &look(), 40, &[], str::to_owned);

        assert!(drawn.lines.len() >= 2, "cut: {:?}", lines_of(&drawn.lines));
        assert_eq!(
            coloured(&drawn.lines, &[Group::KEYWORD]),
            [("let", Group::KEYWORD)]
        );
        assert!(
            !coloured(&drawn.lines, &[Group::CONSTANT]).is_empty(),
            "past the cut"
        );
    }

    #[test]
    fn diffs_are_highlighted_as_diffs() {
        let drawn = draw(
            "```diff\n-old\n+new\n```\n",
            &look(),
            60,
            &[],
            str::to_owned,
        );

        assert_eq!(
            coloured(&drawn.lines, &[Group::REMOVED, Group::ADDED]),
            [("-old", Group::REMOVED), ("+new", Group::ADDED)]
        );
    }

    #[test]
    fn links_refer_to_their_address() {
        let drawn = draw(
            "See [the docs][docs] and [more](https://more.example).\n\n[docs]: https://example.com\n",
            &look(),
            60,
            &[],
            str::to_owned,
        );

        let targets: Vec<(&str, Option<&str>)> = spans(&drawn.lines)
            .map(|span| (span.text.as_str(), span.target.as_deref()))
            .collect();
        assert_eq!(
            targets,
            [
                ("See ", None),
                ("the docs", Some("https://example.com")),
                ("[1]", Some("https://example.com")),
                (" and ", None),
                ("more", Some("https://more.example")),
                ("[2]", Some("https://more.example")),
                (".", None),
            ]
        );
    }

    #[test]
    fn heatmap_shades_the_text_where_it_is_drawn() {
        let heat = Heat {
            human: ratatui::style::Style::new().bg(Color::Green),
            even: ratatui::style::Style::new().bg(Color::Yellow),
            llm: ratatui::style::Style::new().bg(Color::Red),
        };
        let look = Look { heat, ..look() };
        let markdown = "# Title\n\n- An item, and a [link](https://example.com).<br>Same item.\n\n```\ncode\n```\n\nThe end.\n";
        let assessed = as_github_shows(markdown);
        let window = |part: &str, share| {
            let start = assessed.find(part).expect("part of the text");
            Window {
                range: start..start + part.len(),
                share,
            }
        };
        let windows = [
            window(
                "# Title\n\n- An item, and a [link](https://example.com).",
                1.0,
            ),
            window("Same item.\n\n", 0.0),
            window("The end.", 0.0),
        ];

        let drawn = draw(markdown, &look, 60, &windows, str::to_owned);

        let shaded: Vec<(String, Vec<Option<Color>>)> = drawn
            .lines
            .iter()
            .zip(lines_of(&drawn.lines))
            .map(|(line, text)| {
                let bgs = match line {
                    doc::Line::Text(spans) => spans
                        .iter()
                        .map(|span| span.style.and_then(|style| style.bg))
                        .collect(),
                    doc::Line::Image { .. } => Vec::new(),
                };
                (text, bgs)
            })
            .collect();
        let red = Some(Color::Red);
        let green = Some(Color::Green);
        assert_eq!(
            shaded,
            [
                ("Title".to_owned(), vec![red]),
                (String::new(), vec![]),
                ("• An item, and a link[1].".to_owned(), vec![red; 5]),
                ("  Same item.".to_owned(), vec![green, green]),
                (String::new(), vec![]),
                ("code".to_owned(), vec![None]),
                (String::new(), vec![]),
                ("The end.".to_owned(), vec![green]),
            ]
        );
    }

    #[test]
    fn rewritten_text_leads_back_to_the_markdown() {
        let markdown = "a<!-- x -->b\nc<br>d";
        let rewritten = Rewritten::of(markdown);

        assert_eq!(rewritten.text, "ab  \nc  \nd");
        let source =
            |text: &str| rewritten.source_of(rewritten.text.find(text).expect("in the text"));
        assert_eq!(source("a"), 0);
        assert_eq!(source("b"), markdown.find('b').expect("in the Markdown"));
        assert_eq!(
            source(" "),
            markdown.find('\n').expect("a soft break"),
            "rewritten"
        );
        assert_eq!(source("c"), markdown.find('c').expect("in the Markdown"));
        assert_eq!(source("d"), markdown.find('d').expect("in the Markdown"));
    }

    #[test]
    fn drawn_lines_know_the_lines_they_draw() {
        let markdown = "# Title\n\n<!--\na comment\n-->\nOne,\ntwo.<br>Three.\n\n- item\n\n```rust\nlet a\n```\n";

        let drawn = draw(markdown, &look(), 60, &[], str::to_owned);

        let sources: Vec<(String, Option<usize>)> = lines_of(&drawn.lines)
            .into_iter()
            .zip(drawn.sources)
            .collect();
        let line = |text: &str| markdown.lines().position(|line| line.starts_with(text));
        assert_eq!(
            sources,
            [
                ("Title".to_owned(), line("# Title")),
                (String::new(), None),
                ("One,".to_owned(), line("One,")),
                ("two.".to_owned(), line("two.")),
                ("Three.".to_owned(), line("two.")),
                (String::new(), None),
                ("• item".to_owned(), line("- item")),
                (String::new(), None),
                ("let a".to_owned(), line("let a")),
            ]
        );
    }

    #[test]
    fn mentions_are_of_the_whole_handle() {
        assert!(mentions("cc @alxs, what do you think?", "alxs"));
        assert!(mentions("@ALXS", "alxs"), "ignoring case");
        assert!(mentions("Thanks @wcgw/maintainers!", "wcgw/maintainers"));
        assert!(!mentions("cc @alxs-bot", "alxs"));
        assert!(!mentions("mail me@alxs.dev", "alxs"));
        assert!(
            !mentions("ping @wcgw/maintainers", "wcgw"),
            "a team of wcgw"
        );
        assert!(!mentions("Nobody here", "alxs"));
    }

    #[test]
    fn mentions_in_code_dont_count() {
        assert!(!mentions("Run `ghd --user @alxs`", "alxs"));
        assert!(!mentions("```\n@alxs\n```", "alxs"));
        assert!(mentions("```\ncode\n```\n\n@alxs see above", "alxs"));
    }

    #[test]
    fn alerts_show_as_quotes() {
        assert_eq!(
            text("> [!NOTE]\n> Mind the gap."),
            ["▎ [!NOTE]", "▎ Mind the gap."]
        );
    }
}
