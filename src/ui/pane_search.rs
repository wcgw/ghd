//! Searching a pane of text: what the user types to look for, every place it occurs highlighted, and jumps from one to
//! the next, each to the middle of the pane.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::{Scrolled, Step};
use crate::theme::Theme;

/// What's looked for in a pane of text, and where the search is at. Text is looked for in the lines as the pane draws
/// them, ignoring case.
#[derive(Debug, Default)]
pub(super) struct PaneSearch {
    /// What's looked for; empty while nothing is.
    text: String,
    /// The occurrence jumped to last; `None` until a jump finds one.
    current: Option<Occurrence>,
    /// The jump to make when the pane is next drawn: only drawing knows its lines, and where they wrap.
    jump: Option<Jump>,
    /// The prompt in the status line, while the user types what to look for.
    prompt: Option<Prompt>,
}

/// Where what's looked for occurs: in which of the pane's lines, and at which bytes of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Occurrence {
    line: usize,
    bytes: Range<usize>,
}

/// Where a jump goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    /// To the first occurrence on this row of the text or below it, or else the first of all.
    From(u16),
    /// On to the next occurrence or back to the previous one, from the one jumped to last while it shows, or else from
    /// what the pane shows; around from the last to the first, and the first to the last.
    Step(Step),
}

/// The prompt the user types what to look for into.
#[derive(Debug)]
struct Prompt {
    /// What was looked for when the prompt opened, and the occurrence jumped to then: put back if the user cancels.
    text_before: String,
    current_before: Option<Occurrence>,
    /// How far down the pane was scrolled when the prompt opened: what's typed is looked for from there, and the pane
    /// goes back there if the user cancels.
    scrolled_by: u16,
}

impl PaneSearch {
    /// What's looked for, as typed so far, while the user types it; `None` while they don't.
    pub(super) fn typed(&self) -> Option<&str> {
        self.prompt.as_ref().map(|_| self.text.as_str())
    }

    /// Opens the prompt, what's looked for already in it, on a pane scrolled as `scroll` is.
    pub(super) fn start_typing(&mut self, scroll: &Scrolled) {
        self.prompt = Some(Prompt {
            text_before: self.text.clone(),
            current_before: self.current.clone(),
            scrolled_by: scroll.by,
        });
    }

    /// Edits what's looked for with `key`, on the pane `scroll` scrolls. A character adds to it and Backspace takes
    /// the last one off, each time looking for it again from where the pane was when the prompt opened; Enter closes
    /// the prompt, and Esc closes it, putting back what was looked for before and where the pane was.
    pub(super) fn type_key(&mut self, key: KeyEvent, scroll: &mut Scrolled) {
        let Some(scrolled_by) = self.prompt.as_ref().map(|prompt| prompt.scrolled_by) else {
            return;
        };
        match key.code {
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.text.push(character);
            }
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Enter => {
                self.prompt = None;
                return;
            }
            KeyCode::Esc => {
                if let Some(prompt) = self.prompt.take() {
                    self.text = prompt.text_before;
                    self.current = prompt.current_before;
                    self.jump = None;
                    scroll.by = scrolled_by;
                }
                return;
            }
            _ => return,
        }
        scroll.by = scrolled_by;
        self.jump = Some(Jump::From(scrolled_by));
    }

    /// Jumps on to the next occurrence of what's looked for, or back to the previous one, when the pane is next drawn:
    /// from the one jumped to last while it shows, or else from what the pane shows. Does nothing while nothing is
    /// looked for.
    pub(super) fn step(&mut self, step: Step) {
        self.jump = Some(Jump::Step(step));
    }

    /// Stops looking for anything; returns whether anything was looked for.
    pub(super) fn clear(&mut self) -> bool {
        let looked_for = !self.text.is_empty();
        *self = Self::default();
        looked_for
    }

    /// Forgets the occurrence jumped to, and any jump still to make, for the pane shows other text now; what's looked
    /// for is looked for in it.
    pub(super) fn lose_place(&mut self) {
        self.current = None;
        self.jump = None;
    }

    /// Makes the jump due, if there's one, in `lines` as a pane on `area` draws them, its border included: scrolls
    /// `scroll` to bring the occurrence it goes to to the middle of the pane, or as near as the text's top and end let
    /// it. A jump that finds nothing leaves the pane where it is.
    pub(super) fn jump(&mut self, lines: &[Line], area: Rect, scroll: &mut Scrolled) {
        let Some(jump) = self.jump.take() else {
            return;
        };
        let found = occurrences(lines, &self.text);
        // Inside the border.
        let (width, rows) = (area.width.saturating_sub(2), area.height.saturating_sub(2));
        let found_on = rows_of(lines, &found, width);
        let (top, bottom) = (scroll.by, scroll.by.saturating_add(rows));
        let current = self
            .current
            .as_ref()
            .and_then(|current| found.iter().position(|occurrence| occurrence == current))
            .filter(|&at| (top..bottom).contains(&found_on[at]));
        let count = found.len();
        let first_from = |row: u16| {
            found_on
                .iter()
                .position(|&on| on >= row)
                .or((count > 0).then_some(0))
        };
        let to = match (jump, current) {
            (Jump::From(row), _) => first_from(row),
            (Jump::Step(Step::Next), Some(at)) => Some((at + 1) % count),
            (Jump::Step(Step::Previous), Some(at)) => Some((at + count - 1) % count),
            (Jump::Step(Step::Next), None) => first_from(top),
            (Jump::Step(Step::Previous), None) => found_on
                .iter()
                .rposition(|&on| on < bottom)
                .or(count.checked_sub(1)),
        };
        if let Some(to) = to {
            scroll.by = found_on[to].saturating_sub(rows / 2);
            self.current = found.into_iter().nth(to);
        }
    }

    /// Highlights where what's looked for occurs in `lines`, in `theme`, the occurrence jumped to apart; returns what
    /// to say of it in the pane's border, and `None` while nothing is looked for.
    pub(super) fn highlight(
        &self,
        lines: &mut [Line<'static>],
        theme: &Theme,
    ) -> Option<Line<'static>> {
        if self.text.is_empty() {
            return None;
        }
        let found = occurrences(lines, &self.text);
        for on_line in found.chunk_by(|one, other| one.line == other.line) {
            highlight_line(
                &mut lines[on_line[0].line],
                on_line,
                self.current.as_ref(),
                theme,
            );
        }
        let at = self
            .current
            .as_ref()
            .and_then(|current| found.iter().position(|occurrence| occurrence == current));
        let count = match (found.len(), at) {
            (0, _) => "no match".to_owned(),
            (count, Some(at)) => format!("{} of {count}", at + 1),
            (1, None) => "1 match".to_owned(),
            (count, None) => format!("{count} matches"),
        };
        // While it's typed, what's looked for is in the prompt.
        let said = match self.typed() {
            Some(_) => format!(" {count} "),
            None => format!(" /{} · {count} ", self.text),
        };
        Some(Line::styled(said, theme.search))
    }
}

/// Where `text` occurs in `lines`, ignoring case, in order: line by line, left to right, none overlapping.
fn occurrences(lines: &[Line], text: &str) -> Vec<Occurrence> {
    let wanted: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut found = Vec::new();
    if wanted.is_empty() {
        return found;
    }
    for (index, line) in lines.iter().enumerate() {
        let drawn: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        // Each character lowercased, with the bytes of the character it's of: one can lowercase to several.
        let lowered: Vec<(Range<usize>, char)> = drawn
            .char_indices()
            .flat_map(|(at, character)| {
                let bytes = at..at + character.len_utf8();
                character
                    .to_lowercase()
                    .map(move |lower| (bytes.clone(), lower))
            })
            .collect();
        let mut at = 0;
        while at + wanted.len() <= lowered.len() {
            let here = &lowered[at..at + wanted.len()];
            if !here
                .iter()
                .map(|(_, lower)| *lower)
                .eq(wanted.iter().copied())
            {
                at += 1;
                continue;
            }
            // Whole characters, even where only part of what one lowercases to is looked for.
            found.push(Occurrence {
                line: index,
                bytes: here[0].0.start..here[here.len() - 1].0.end,
            });
            at += wanted.len();
        }
    }
    found
}

/// The row of the text each of `found` ends on, with `lines` wrapped to `width` as the pane wraps them.
fn rows_of(lines: &[Line], found: &[Occurrence], width: u16) -> Vec<u16> {
    let wrapped = |line: Line| {
        Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .line_count(width)
    };
    let mut rows = Vec::with_capacity(found.len());
    let mut found = found.iter().peekable();
    // The rows the lines before take.
    let mut above = 0;
    for (index, line) in lines.iter().enumerate() {
        while let Some(occurrence) = found.next_if(|occurrence| occurrence.line == index) {
            let up_to_its_end = wrapped(cut_at(line, occurrence.bytes.end));
            rows.push(above + up_to_its_end.saturating_sub(1));
        }
        above += wrapped(line.clone());
    }
    rows.into_iter()
        .map(|row| u16::try_from(row).unwrap_or(u16::MAX))
        .collect()
}

/// `line` as far as byte `end` of its text, which is between characters.
fn cut_at<'a>(line: &Line<'a>, end: usize) -> Line<'a> {
    let mut spans = Vec::new();
    let mut start = 0;
    for span in &line.spans {
        if start >= end {
            break;
        }
        let length = span.content.len();
        let kept = &span.content[..length.min(end - start)];
        spans.push(Span::styled(kept.to_owned(), span.style));
        start += length;
    }
    Line::from(spans)
}

/// Highlights `found`, occurrences on `line`, in `theme`'s style for them, or for the one jumped to, `current`: over
/// the style of the text they cover, cutting its spans where they start and end.
fn highlight_line(
    line: &mut Line<'static>,
    found: &[Occurrence],
    current: Option<&Occurrence>,
    theme: &Theme,
) {
    let mut pieces = Vec::with_capacity(line.spans.len() + 2 * found.len());
    // Where the span is in the line's text.
    let mut start = 0;
    for span in line.spans.drain(..) {
        let end = start + span.content.len();
        let mut from = start;
        while from < end {
            let (to, over) = match found.iter().find(|occurrence| occurrence.bytes.end > from) {
                Some(occurrence) if occurrence.bytes.start <= from => {
                    let over = if current == Some(occurrence) {
                        theme.found_current
                    } else {
                        theme.found
                    };
                    (occurrence.bytes.end.min(end), Some(over))
                }
                Some(occurrence) => (occurrence.bytes.start.min(end), None),
                None => (end, None),
            };
            let piece = Span::styled(
                span.content[from - start..to - start].to_owned(),
                span.style,
            );
            pieces.push(match over {
                Some(over) => piece.patch_style(over),
                None => piece,
            });
            from = to;
        }
        start = end;
    }
    line.spans = pieces;
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    /// A theme that tells occurrences, and the one jumped to, apart.
    fn theme() -> Theme {
        Theme {
            found: Style::new().bg(Color::Yellow),
            found_current: Style::new().bg(Color::Red),
            ..Theme::default()
        }
    }

    fn at(line: usize, bytes: Range<usize>) -> Occurrence {
        Occurrence { line, bytes }
    }

    #[test]
    fn text_is_found_ignoring_case_across_spans() {
        let lines = [
            Line::from(vec![Span::raw("let Need"), Span::raw("le = needle;")]),
            Line::raw("nothing here"),
            Line::raw("NEEDLE"),
        ];

        assert_eq!(
            occurrences(&lines, "needle"),
            [at(0, 4..10), at(0, 13..19), at(2, 0..6)]
        );
        assert_eq!(occurrences(&lines, "NeEdLe"), occurrences(&lines, "needle"));
        assert_eq!(occurrences(&lines, "haystack"), []);
    }

    #[test]
    fn occurrences_never_overlap() {
        assert_eq!(
            occurrences(&[Line::raw("aaaaa")], "aa"),
            [at(0, 0..2), at(0, 2..4)]
        );
    }

    #[test]
    fn occurrences_are_where_their_characters_are() {
        // "É" and "é" are two bytes each.
        let lines = [Line::raw("café, CAFÉ")];

        assert_eq!(occurrences(&lines, "é"), [at(0, 3..5), at(0, 10..12)]);
        assert_eq!(occurrences(&lines, "café"), [at(0, 0..5), at(0, 7..12)]);
        // "İ" lowercases to "i" and a combining dot: all of it is found.
        assert_eq!(occurrences(&[Line::raw("İf")], "i"), [at(0, 0..2)]);
    }

    #[test]
    fn highlighting_cuts_spans_over_their_own_style() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let mut lines = [Line::from(vec![
            Span::styled("foo", red),
            Span::styled("bar foo", blue),
        ])];
        let search = PaneSearch {
            text: "oob".to_owned(),
            ..PaneSearch::default()
        };

        search.highlight(&mut lines, &theme());

        assert_eq!(
            lines[0].spans,
            [
                Span::styled("f", red),
                Span::styled("oo", red.bg(Color::Yellow)),
                Span::styled("b", blue.bg(Color::Yellow)),
                Span::styled("ar foo", blue),
            ]
        );
    }

    #[test]
    fn the_occurrence_jumped_to_is_highlighted_apart() {
        let mut lines = [Line::raw("one needle, two needles")];
        let search = PaneSearch {
            text: "needle".to_owned(),
            current: Some(at(0, 16..22)),
            ..PaneSearch::default()
        };

        let said = search.highlight(&mut lines, &theme());

        assert_eq!(
            lines[0].spans,
            [
                Span::raw("one "),
                Span::styled("needle", Style::new().bg(Color::Yellow)),
                Span::raw(", two "),
                Span::styled("needle", Style::new().bg(Color::Red)),
                Span::raw("s"),
            ]
        );
        assert_eq!(said, Some(Line::raw(" /needle · 2 of 2 ")));
    }

    #[test]
    fn rows_count_where_lines_wrap() {
        let lines = [
            Line::raw("short"),
            // At 10 cells: "aaaa bbbb", "cccc", "needle".
            Line::raw("aaaa bbbb cccc needle"),
            // "needle", "aaaa bbbb", "cccc".
            Line::raw("needle aaaa bbbb cccc"),
            Line::raw("needle"),
        ];

        let found = occurrences(&lines, "needle");

        assert_eq!(rows_of(&lines, &found, 10), [3, 4, 7]);
        assert_eq!(rows_of(&lines, &found, 80), [1, 2, 3]);
    }

    /// A search for "needle" jumped to its occurrence in line 20 of `lines`, 30 lines with "needle" in lines 5 and 20.
    fn on_the_second_needle() -> (PaneSearch, Vec<Line<'static>>) {
        let lines = (0..30)
            .map(|i| match i {
                5 | 20 => Line::raw(format!("{i} needle")),
                _ => Line::raw(i.to_string()),
            })
            .collect();
        let search = PaneSearch {
            text: "needle".to_owned(),
            current: Some(at(20, 3..9)),
            ..PaneSearch::default()
        };
        (search, lines)
    }

    #[test]
    fn steps_go_on_from_the_occurrence_jumped_to_only_while_it_shows() {
        // 10 rows inside the border.
        let area = Rect::new(0, 0, 40, 12);
        let (first, second) = (at(5, 2..8), at(20, 3..9));
        for (by, current, to) in [
            // The second just below the pane: the first from the pane's top, the second again.
            (10, &second, 15),
            // The second on the pane's last row, then its first: on to the next, around to the first.
            (11, &first, 0),
            (20, &first, 0),
        ] {
            let (mut search, lines) = on_the_second_needle();
            let mut scroll = Scrolled {
                by,
                ..Scrolled::default()
            };

            search.step(Step::Next);
            search.jump(&lines, area, &mut scroll);

            assert_eq!(search.current.as_ref(), Some(current), "scrolled by {by}");
            assert_eq!(scroll.by, to, "scrolled by {by}");
        }
    }

    #[test]
    fn nothing_looked_for_says_nothing() {
        let mut lines = [Line::raw("needle")];

        assert_eq!(PaneSearch::default().highlight(&mut lines, &theme()), None);
        assert_eq!(lines[0], Line::raw("needle"));
    }
}
