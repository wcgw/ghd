//! Searches: text typed to narrow the View on screen (see `CONTEXT.md`).

use crate::inbox::{Notification, SubjectKind};

/// What a Search is made of: space-separated terms, each either plain text or `field:text`, with `"` around text
/// that has spaces in it (`title:"add polling"`). A Notification matches when it contains the text of every term,
/// ignoring case. Plain text looks in the title and the repository; the fields are `title:`, `repo:`, `reason:`
/// and `type:` (a Subject kind's name, as in Filters' `is:`, or `pr`). The default Search matches everything.
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
}

impl Field {
    /// The field `name` stands for in `name:text`, ignoring case.
    fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "title" => Some(Self::Title),
            "repo" => Some(Self::Repository),
            "reason" => Some(Self::Reason),
            "type" => Some(Self::Type),
            _ => None,
        }
    }
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

    /// Whether `notification` matches every term.
    pub fn matches(&self, notification: &Notification) -> bool {
        self.terms.iter().all(|term| term.matches(notification))
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

    fn matches(&self, notification: &Notification) -> bool {
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
        Search::parse(input).matches(&notification())
    }

    #[test]
    fn empty_search_matches_everything() {
        assert!(Search::default().matches(&notification()));
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
    fn quotes_keep_spaces_in_a_field() {
        assert!(matches(r#"title:"polling to""#));
        assert!(!matches(r#"title:"polling inbox""#));
        assert!(matches(r#"title:"add poll"#), "unclosed while typing");
    }

    #[test]
    fn colons_that_start_no_field_are_text() {
        let fix = Notification {
            subject_title: "fix: polling".to_owned(),
            ..notification()
        };

        assert!(Search::parse("fix:").matches(&fix), "unknown field");
        assert!(Search::parse("fix: polling").matches(&fix));
        assert!(
            Search::parse(r#""fix: poll""#).matches(&fix),
            "colon inside quotes"
        );
        assert!(!Search::parse("fix:").matches(&notification()));
    }
}
