//! Searches: text typed to narrow the View on screen (see `CONTEXT.md`).

use crate::inbox::{Notification, SubjectKind};
use crate::state::SubjectState;

/// What a Search is made of: space-separated terms, each either plain text or `field:text`, with `"` around text
/// that has spaces in it (`title:"add polling"`). A Notification matches when it contains the text of every term,
/// ignoring case. Plain text looks in the title and the repository; the fields are `title:`, `repo:`, `reason:`,
/// `type:` (a Subject kind's name, as in Filters' `is:`, or `pr`) and `state:` (a State, as in Filters' `is:`, of a
/// pull request or an issue whose State is known). The default Search matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    terms: Vec<Term>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    field: Field,
    /// Lowercase, never empty.
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    /// Plain text: the title or the repository.
    TitleOrRepository,
    Title,
    Repository,
    Reason,
    Type,
    State,
}

/// The fields a term can name, in the order completion offers them.
const FIELD_NAMES: [(&str, Field); 5] = [
    ("title", Field::Title),
    ("repo", Field::Repository),
    ("reason", Field::Reason),
    ("type", Field::Type),
    ("state", Field::State),
];

impl Field {
    /// The field `name` stands for in `name:text`, ignoring case.
    fn named(name: &str) -> Option<Self> {
        FIELD_NAMES
            .iter()
            .find(|(field_name, _)| field_name.eq_ignore_ascii_case(name))
            .map(|&(_, field)| field)
    }
}

/// The ways the last word of `input` can be completed, each as the whole input with that word completed, in the
/// order to offer them: the `field:` prefixes the word could be the start of, ignoring case. None when the word
/// can't start a field: it has a `:` or a `"` already (a word inside quotes has the opening one).
pub fn completions(input: &str) -> Vec<String> {
    let mut word_start = 0;
    let mut in_quotes = false;
    for (at, character) in input.char_indices() {
        match character {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => word_start = at + c.len_utf8(),
            _ => {}
        }
    }
    let (before, word) = input.split_at(word_start);
    if word.contains([':', '"']) {
        return Vec::new();
    }
    let word = word.to_ascii_lowercase();
    FIELD_NAMES
        .iter()
        .filter(|(name, _)| name.starts_with(&word))
        .map(|(name, _)| format!("{before}{name}:"))
        .collect()
}

impl Search {
    /// The Search that `input` stands for. Never fails, because a Search is applied while it's being typed: an
    /// unknown `field:` is plain text, a term with no text is left out, and an unclosed `"` runs to the end.
    pub fn parse(input: &str) -> Self {
        let mut terms = Vec::new();
        let mut text = String::new();
        // Where in `text` the first `:` outside quotes is, when no quote came before it.
        let mut colon = None;
        let mut quoted = false;
        let mut in_quotes = false;
        for character in input.chars() {
            match character {
                '"' => {
                    in_quotes = !in_quotes;
                    quoted = true;
                }
                c if c.is_whitespace() && !in_quotes => {
                    terms.extend(Term::new(&text, colon));
                    text.clear();
                    colon = None;
                    quoted = false;
                }
                ':' if !in_quotes && !quoted && colon.is_none() => {
                    colon = Some(text.len());
                    text.push(':');
                }
                c => text.push(c),
            }
        }
        terms.extend(Term::new(&text, colon));
        Self { terms }
    }

    /// Whether `notification` matches every term, its Subject in `state` if that\'s known.
    pub fn matches(&self, notification: &Notification, state: Option<SubjectState>) -> bool {
        self.terms
            .iter()
            .all(|term| term.matches(notification, state))
    }
}

impl Term {
    /// The term written as `text`, quotes taken out, with its first `:` at `colon` if that might end a field name;
    /// `None` if it has no text to look for.
    fn new(text: &str, colon: Option<usize>) -> Option<Self> {
        let (field, text) = colon
            .and_then(|colon| Some((Field::named(&text[..colon])?, &text[colon + 1..])))
            .unwrap_or((Field::TitleOrRepository, text));
        (!text.is_empty()).then(|| Self {
            field,
            text: text.to_lowercase(),
        })
    }

    fn matches(&self, notification: &Notification, state: Option<SubjectState>) -> bool {
        let contains = |haystack: &str| haystack.to_lowercase().contains(&self.text);
        match self.field {
            Field::TitleOrRepository => {
                contains(&notification.subject_title) || contains(&notification.repository)
            }
            Field::Title => contains(&notification.subject_title),
            Field::Repository => contains(&notification.repository),
            Field::Reason => contains(&notification.reason.to_string()),
            Field::Type => {
                contains(notification.subject_kind.name())
                    || (self.text == "pr" && notification.subject_kind == SubjectKind::PullRequest)
            }
            Field::State => state.is_some_and(|state| state.label().contains(&self.text)),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::inbox::{Reason, ThreadId};

    /// "Add polling to the Inbox", an unread review request on a pull request in wcgw/ghd.
    fn notification() -> Notification {
        Notification {
            thread: ThreadId(1),
            subject_url: "https://github.com/wcgw/ghd/pull/1".to_owned(),
            repository: "wcgw/ghd".to_owned(),
            subject_kind: SubjectKind::PullRequest,
            subject_title: "Add polling to the Inbox".to_owned(),
            reason: Reason::ReviewRequested,
            unread: true,
            updated_at: Utc::now(),
        }
    }

    fn matches(input: &str) -> bool {
        Search::parse(input).matches(&notification(), None)
    }

    #[test]
    fn empty_search_matches_everything() {
        assert!(Search::default().matches(&notification(), None));
        for input in ["", "   ", "title:", r#""""#] {
            assert_eq!(Search::parse(input), Search::default(), "{input:?}");
        }
    }

    #[test]
    fn plain_text_looks_in_title_and_repository_ignoring_case() {
        assert!(matches("POLL"));
        assert!(matches("wcgw/gh"));
        assert!(!matches("review"), "not in the reason");
        assert!(!matches("limitador"));
    }

    #[test]
    fn every_term_must_match() {
        assert!(matches("poll ghd"));
        assert!(!matches("poll limitador"));
        assert!(matches("polling inbox"), "words, not a phrase");
        assert!(!matches(r#""polling inbox""#), "a phrase");
        assert!(matches(r#""polling to the""#));
    }

    #[test]
    fn fields_look_in_one_column() {
        assert!(matches("title:inbox"));
        assert!(!matches("title:ghd"));
        assert!(matches("repo:ghd"));
        assert!(!matches("repo:polling"));
        assert!(matches("reason:review-req"));
        assert!(!matches("reason:mention"));
        assert!(matches("type:pull"));
        assert!(matches("type:pr"));
        assert!(matches("TYPE:PR"), "field names ignore case too");
        assert!(!matches("type:issue"));
    }

    #[test]
    fn state_field_looks_in_the_known_state() {
        let merged = Some(SubjectState::Merged);
        let state = |input: &str, state| Search::parse(input).matches(&notification(), state);

        assert!(state("state:merg", merged));
        assert!(!state("state:open", merged));
        assert!(state("state:not-planned", Some(SubjectState::NotPlanned)));
        assert!(state("state:planned", Some(SubjectState::NotPlanned)));
        assert!(!state("state:merged", None), "no State known");
        assert!(
            !state("merged", merged),
            "plain text doesn't look in the State"
        );
    }

    #[test]
    fn quotes_keep_spaces_in_a_field() {
        assert!(matches(r#"title:"polling to""#));
        assert!(!matches(r#"title:"polling inbox""#));
        assert!(matches(r#"title:"add poll"#), "unclosed while typing");
    }

    #[test]
    fn completion_offers_the_fields_a_word_could_start() {
        let fields = ["title:", "repo:", "reason:", "type:", "state:"];
        assert_eq!(completions(""), fields);
        assert_eq!(completions("re"), ["repo:", "reason:"]);
        assert_eq!(completions("TI"), ["title:"]);
        assert_eq!(completions("title"), ["title:"]);
        assert_eq!(completions("poll t"), ["poll title:", "poll type:"]);
        assert_eq!(
            completions("poll "),
            fields.map(|field| format!("poll {field}"))
        );
        assert_eq!(
            completions(r#"title:"add polling" r"#),
            [
                r#"title:"add polling" repo:"#,
                r#"title:"add polling" reason:"#
            ]
        );
    }

    #[test]
    fn completion_offers_nothing_where_no_field_can_start() {
        for input in [
            "poll",
            "title:",
            "title:po",
            "fix:",
            r#""add "#,
            r#"title:"add po"#,
            r#""quoted""#,
        ] {
            assert!(completions(input).is_empty(), "{input:?}");
        }
    }

    #[test]
    fn colons_that_start_no_field_are_text() {
        let fix = Notification {
            subject_title: "fix: polling".to_owned(),
            ..notification()
        };

        assert!(Search::parse("fix:").matches(&fix, None), "unknown field");
        assert!(Search::parse("fix: polling").matches(&fix, None));
        assert!(
            Search::parse(r#""fix: poll""#).matches(&fix, None),
            "colon inside quotes"
        );
        assert!(!Search::parse("fix:").matches(&notification(), None));
    }
}
