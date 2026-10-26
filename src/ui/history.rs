//! What the user entered in a prompt before, to go back through with Up and Down: the searches, or the commands.

use super::Step;

/// How many entries a History keeps: the newest ones.
const KEPT: usize = 100;

/// What the user entered in a prompt before, for as long as ghd runs: oldest first, each once.
#[derive(Debug, Default)]
pub(super) struct History {
    entries: Vec<String>,
}

/// Where the user is in a History while going through it in an open prompt, and what the prompt held before.
#[derive(Debug)]
pub(super) struct Recall {
    /// What was in the prompt when the user started going through the History: given back past the newest entry.
    typed: String,
    /// Index into the History's entries.
    at: usize,
}

impl History {
    /// Keeps `entry` as the newest, or moves it there if it's kept already. Nothing is kept of an empty entry, and
    /// only the newest [`KEPT`] are.
    pub(super) fn add(&mut self, entry: &str) {
        let entry = entry.trim();
        if entry.is_empty() {
            return;
        }
        self.entries.retain(|kept| kept != entry);
        self.entries.push(entry.to_owned());
        if self.entries.len() > KEPT {
            self.entries.remove(0);
        }
    }

    /// Goes back to the previous (older) entry or on to the next (newer) one from where `recall` is, or, if it's
    /// `None`, starts going back from `typed`, what's in the prompt now. Entries the same as what the prompt held when
    /// that started are passed over. Returns what the prompt should hold now: on past the newest entry, what it held
    /// then; `None` if that doesn't change, as when there's no older entry.
    pub(super) fn recall(
        &self,
        recall: &mut Option<Recall>,
        typed: &str,
        step: Step,
    ) -> Option<String> {
        let (held, from) = match recall {
            Some(recall) => (recall.typed.as_str(), recall.at),
            None => (typed, self.entries.len()),
        };
        let other = |at: &usize| self.entries[*at] != held;
        let found = match step {
            Step::Previous => (0..from).rev().find(other),
            Step::Next => (from + 1..self.entries.len()).find(other),
        };
        match (found, step) {
            (Some(at), _) => {
                match recall {
                    Some(recall) => recall.at = at,
                    None => {
                        *recall = Some(Recall {
                            typed: typed.to_owned(),
                            at,
                        });
                    }
                }
                Some(self.entries[at].clone())
            }
            (None, Step::Next) => recall.take().map(|recall| recall.typed),
            (None, Step::Previous) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(entries: &[&str]) -> History {
        let mut history = History::default();
        for entry in entries {
            history.add(entry);
        }
        history
    }

    #[test]
    fn keeps_each_entry_once_the_newest_last() {
        let history = history(&["old", "", "  ", "new", " old "]);

        assert_eq!(history.entries, ["new", "old"]);
    }

    #[test]
    fn keeps_only_the_newest() {
        let entries: Vec<String> = (0..=KEPT).map(|i| i.to_string()).collect();
        let history = history(&entries.iter().map(String::as_str).collect::<Vec<_>>());

        assert_eq!(history.entries.len(), KEPT);
        assert_eq!(history.entries.first().map(String::as_str), Some("1"));
        assert_eq!(
            history.entries.last().map(String::as_str),
            Some(KEPT.to_string().as_str())
        );
    }

    #[test]
    fn goes_back_and_on_then_gives_back_what_was_typed() {
        let history = history(&["first", "second"]);
        let mut recall = None;
        let mut step = |step| history.recall(&mut recall, "typed", step);

        assert_eq!(step(Step::Previous).as_deref(), Some("second"));
        assert_eq!(step(Step::Previous).as_deref(), Some("first"));
        assert_eq!(step(Step::Previous), None, "nothing older");
        assert_eq!(step(Step::Next).as_deref(), Some("second"));
        assert_eq!(
            step(Step::Next).as_deref(),
            Some("typed"),
            "past the newest"
        );
        assert_eq!(step(Step::Next), None, "not going through it");
        assert_eq!(
            step(Step::Previous).as_deref(),
            Some("second"),
            "from the newest again"
        );
    }

    #[test]
    fn passes_over_what_the_prompt_holds_already() {
        let history = history(&["other", "kept"]);
        let mut recall = None;

        assert_eq!(
            history
                .recall(&mut recall, "kept", Step::Previous)
                .as_deref(),
            Some("other")
        );
        assert_eq!(
            history.recall(&mut recall, "other", Step::Next).as_deref(),
            Some("kept"),
            "what it held, past the newest"
        );
    }

    #[test]
    fn an_empty_history_changes_nothing() {
        let history = History::default();
        let mut recall = None;

        assert_eq!(history.recall(&mut recall, "typed", Step::Previous), None);
        assert_eq!(history.recall(&mut recall, "typed", Step::Next), None);
        assert!(recall.is_none());
    }
}
