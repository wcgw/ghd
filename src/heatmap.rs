//! The Heatmap: the text of an Assessment, each window voight scored shaded by the share of it an LLM wrote (see
//! `CONTEXT.md`).

use ratatui::text::{Line, Span};

use crate::assessment::Window;
use crate::drawn::Finder;
use crate::theme::Theme;

/// Shades `lines`, which draw `text`, by `windows` of it, in `theme`'s heatmap styles; text outside them, like code,
/// stays as it is. Nothing is shaded if `windows` aren't of `text`.
///
/// What each line draws is found in `text` in order, span by span. What isn't found nearby, like a list's markers or
/// a table's borders, is shaded as the text before it on its line, or else as the text after it.
pub fn shade(lines: &mut [Line<'static>], text: &str, windows: &[Window], theme: &Theme) {
    if windows.is_empty()
        || windows
            .iter()
            .any(|window| text.get(window.range.clone()).is_none())
    {
        return;
    }
    let mut finder = Finder::new(text);
    for line in lines {
        finder.next_line();
        let mut pieces = Vec::with_capacity(line.spans.len());
        for span in line.spans.drain(..) {
            match finder.find(&span.content) {
                Some(start) => cut(span, start, windows, &mut pieces),
                None => pieces.push((span, Place::Added)),
            }
        }
        place_added(&mut pieces);
        line.spans = pieces
            .into_iter()
            .map(|(span, place)| match place {
                Place::Found(Some(share)) => span.patch_style(theme.heat(share)),
                Place::Found(None) | Place::Added => span,
            })
            .collect();
    }
}

/// Where a piece of a line is in the text it draws.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Place {
    /// Found: in a window an LLM wrote this share of, or outside any.
    Found(Option<f64>),
    /// Not found: something the drawing added.
    Added,
}

/// Pushes `span`, found at `start` in the text, onto `pieces`: cut where `windows` start or end in it, each piece placed
/// in its window, if any.
fn cut(
    span: Span<'static>,
    start: usize,
    windows: &[Window],
    pieces: &mut Vec<(Span<'static>, Place)>,
) {
    let end = start + span.content.len();
    let mut from = start;
    while from < end {
        let next = windows.partition_point(|window| window.range.end <= from);
        let (to, share) = match windows.get(next) {
            Some(window) if window.range.start <= from => {
                (window.range.end.min(end), Some(window.share))
            }
            Some(window) => (window.range.start.min(end), None),
            None => (end, None),
        };
        if from == start && to == end {
            pieces.push((span, Place::Found(share)));
            return;
        }
        // The windows are of the text, so they start and end between characters.
        let piece = span.content[from - start..to - start].to_owned();
        pieces.push((Span::styled(piece, span.style), Place::Found(share)));
        from = to;
    }
}

/// Places what the drawing added to a line as the piece found before it, or else after it; outside any window if
/// nothing on the line was found.
fn place_added(pieces: &mut [(Span<'static>, Place)]) {
    let mut before = None;
    for (_, place) in pieces.iter_mut() {
        match *place {
            Place::Found(share) => before = Some(share),
            Place::Added => {
                if let Some(share) = before {
                    *place = Place::Found(share);
                }
            }
        }
    }
    // Only what comes before anything found is left.
    let mut after = None;
    for (_, place) in pieces.iter_mut().rev() {
        match *place {
            Place::Found(share) => after = Some(share),
            Place::Added => *place = Place::Found(after.flatten()),
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    /// A theme whose heatmap tells apart text an LLM wrote none, half and all of.
    fn theme() -> Theme {
        Theme {
            heatmap_human: Style::new().bg(Color::Green),
            heatmap_even: Style::new().bg(Color::Yellow),
            heatmap_llm: Style::new().bg(Color::Red),
            ..Theme::default()
        }
    }

    /// The window over `part` of `text`, which an LLM wrote `share` of.
    fn window(text: &str, part: &str, share: f64) -> Window {
        let start = text.find(part).expect("part of the text");
        Window {
            range: start..start + part.len(),
            share,
        }
    }

    /// Each line's pieces, with the background each is shaded with.
    fn shades(lines: &[Line]) -> Vec<Vec<(String, Option<Color>)>> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| (span.content.to_string(), span.style.bg))
                    .collect()
            })
            .collect()
    }

    fn piece(text: &str, bg: Option<Color>) -> (String, Option<Color>) {
        (text.to_owned(), bg)
    }

    #[test]
    fn windows_are_shaded_and_cut_where_they_meet() {
        let text = "One sentence. Another one.\n\nA last paragraph.";
        let windows = [
            window(text, "One sentence.", 0.0),
            window(text, "Another one.", 1.0),
            window(text, "A last paragraph.", 0.5),
        ];
        let mut lines = vec![
            Line::from("One sentence. Another one."),
            Line::default(),
            Line::from("A last paragraph."),
        ];

        shade(&mut lines, text, &windows, &theme());

        assert_eq!(
            shades(&lines),
            [
                vec![
                    piece("One sentence.", Some(Color::Green)),
                    piece(" ", None),
                    piece("Another one.", Some(Color::Red)),
                ],
                vec![],
                vec![piece("A last paragraph.", Some(Color::Yellow))],
            ]
        );
    }

    #[test]
    fn shading_keeps_the_text_style_but_its_colors() {
        let text = "Bold words.";
        let mut lines = vec![Line::from(Span::styled(text, Style::new().blue().bold()))];
        let theme = Theme {
            heatmap_llm: Style::new().black().on_red(),
            ..theme()
        };

        shade(&mut lines, text, &[window(text, text, 1.0)], &theme);

        assert_eq!(
            lines[0].spans[0].style,
            Style::new().black().on_red().bold()
        );
    }

    /// A link drawn as `tui-markdown` does: its text, then its address in brackets.
    fn link(text: &str, address: &str) -> [Span<'static>; 4] {
        [
            Span::raw(text.to_owned()),
            Span::raw(" ("),
            Span::raw(address.to_owned()),
            Span::raw(")"),
        ]
    }

    #[test]
    fn repeated_text_is_found_where_it_comes_next() {
        let text = "Said once.\n\nSaid once.";
        let windows = [
            Window {
                range: 0..10,
                share: 1.0,
            },
            Window {
                range: 12..22,
                share: 0.0,
            },
        ];
        let mut lines = vec![
            Line::from("Said once."),
            Line::default(),
            Line::from("Said once."),
        ];

        shade(&mut lines, text, &windows, &theme());

        assert_eq!(shades(&lines)[2], [piece("Said once.", Some(Color::Green))]);
    }

    #[test]
    fn link_written_as_its_address_is_shaded_as_its_text() {
        let text = "See https://x.y/a for it.\n\nMore at https://x.y/a below.";
        let windows = [
            window(text, "See https://x.y/a for it.", 1.0),
            window(text, "More at https://x.y/a below.", 0.0),
        ];
        let line = |before: &str, after: &str| {
            let mut spans = vec![Span::raw(before.to_owned())];
            spans.extend(link("https://x.y/a", "https://x.y/a"));
            spans.push(Span::raw(after.to_owned()));
            Line::from(spans)
        };
        let mut lines = vec![
            line("See ", " for it."),
            Line::default(),
            line("More at ", " below."),
        ];

        shade(&mut lines, text, &windows, &theme());

        let shades = shades(&lines);
        assert!(
            shades[0].iter().all(|(_, bg)| *bg == Some(Color::Red)),
            "{shades:?}"
        );
        assert!(
            shades[2].iter().all(|(_, bg)| *bg == Some(Color::Green)),
            "{shades:?}"
        );
    }

    #[test]
    fn text_after_an_address_found_further_on_is_found_all_the_same() {
        // A reference link, drawn with its address, which is defined at the end.
        let text = "A [link][ref] here.\n\nNext (with an aside).\n\n[ref]: https://x.y";
        let windows = [
            window(text, "A [link][ref] here.", 1.0),
            window(text, "Next (with an aside).", 0.0),
        ];
        let mut spans = vec![Span::raw("A ")];
        spans.extend(link("link", "https://x.y"));
        spans.push(Span::raw(" here."));
        let mut lines = vec![
            Line::from(spans),
            Line::default(),
            Line::from("Next (with an aside)."),
        ];

        shade(&mut lines, text, &windows, &theme());

        let shades = shades(&lines);
        assert_eq!(
            shades[0].last(),
            Some(&piece(" here.", Some(Color::Red))),
            "{shades:?}"
        );
        assert_eq!(
            shades[2],
            [piece("Next (with an aside).", Some(Color::Green))]
        );
    }

    #[test]
    fn address_defined_far_off_is_shaded_as_its_link() {
        let words = "word ".repeat(crate::drawn::LOOKAHEAD);
        let text = format!("A [link][ref]\n\n{words}\n\n[ref]: https://x.y");
        let windows = [
            window(&text, "A [link][ref]", 1.0),
            window(&text, words.trim_end(), 0.0),
        ];
        let mut spans = vec![Span::raw("A ")];
        spans.extend(link("link", "https://x.y"));
        let mut lines = vec![Line::from(spans), Line::default(), Line::from("word word")];

        shade(&mut lines, &text, &windows, &theme());

        let shades = shades(&lines);
        assert!(
            shades[0].iter().all(|(_, bg)| *bg == Some(Color::Red)),
            "{shades:?}"
        );
        assert_eq!(shades[2], [piece("word word", Some(Color::Green))]);
    }

    #[test]
    fn windows_of_another_text_shade_nothing() {
        let text = "Short.";
        let mut lines = vec![Line::from(text)];

        shade(
            &mut lines,
            text,
            &[window("A longer text.", "longer text.", 1.0)],
            &theme(),
        );

        assert_eq!(shades(&lines), [vec![piece("Short.", None)]]);
    }

    #[test]
    fn added_pieces_take_their_neighbours_shade() {
        let mut pieces = vec![
            (Span::raw("- "), Place::Added),
            (Span::raw("item"), Place::Found(Some(1.0))),
            (Span::raw(" ("), Place::Added),
            (Span::raw("url"), Place::Found(Some(0.0))),
            (Span::raw(")"), Place::Added),
        ];
        place_added(&mut pieces);
        let places: Vec<Place> = pieces.iter().map(|(_, place)| *place).collect();
        assert_eq!(
            places,
            [
                Place::Found(Some(1.0)),
                Place::Found(Some(1.0)),
                Place::Found(Some(1.0)),
                Place::Found(Some(0.0)),
                Place::Found(Some(0.0)),
            ]
        );

        let mut alone = vec![(Span::raw("───"), Place::Added)];
        place_added(&mut alone);
        assert_eq!(alone[0].1, Place::Found(None), "a table's border line");
    }
}
