//! Finding what lines of text draw in the text they draw: where Markdown's rendered text, say, is in the Markdown.

use std::ops::Range;

/// How far past where it's looked for from the next text drawn is looked for. Further than that, it's taken for
/// something the drawing added, like the address of a link defined at the end of a long text.
pub const LOOKAHEAD: usize = 512;

/// Finds the text lines draw in the text they draw, in order.
pub struct Finder<'t> {
    text: &'t str,
    /// Just past the text found last.
    at: usize,
    /// Where the text found last was looked for from. The next is looked for from there again if it isn't found past
    /// the last: what was found last may have been something the drawing added, found further on, like the address of
    /// a link defined at the end of the text.
    looked_from: usize,
    /// The text found last on the line; empty on a new line.
    last: Range<usize>,
}

impl<'t> Finder<'t> {
    pub fn new(text: &'t str) -> Self {
        Self {
            text,
            at: 0,
            looked_from: 0,
            last: 0..0,
        }
    }

    /// Notes that what's drawn next is on a new line.
    pub fn next_line(&mut self) {
        self.last = 0..0;
    }

    /// Where `drawn`, drawn next, is in the text; `None` if it's taken for something the drawing added: what isn't
    /// found nearby, what has no letters or digits to tell it by, like a list's `- `, and what's the text found last
    /// on its line again, like the address of a link written as that address.
    pub fn find(&mut self, drawn: &str) -> Option<usize> {
        if !drawn.chars().any(char::is_alphanumeric)
            || self.text.get(self.last.clone()) == Some(drawn)
        {
            return None;
        }
        let start = match self.find_from(self.at, drawn) {
            Some(start) => {
                self.looked_from = self.at;
                start
            }
            None => self.find_from(self.looked_from, drawn)?,
        };
        self.at = start + drawn.len();
        self.last = start..self.at;
        Some(start)
    }

    /// Where `drawn` is in the text, from `from` to not far past it.
    fn find_from(&self, from: usize, drawn: &str) -> Option<usize> {
        let end = self
            .text
            .floor_char_boundary(from.saturating_add(LOOKAHEAD + drawn.len()));
        self.text
            .get(from..end)?
            .find(drawn)
            .map(|offset| from + offset)
    }
}
