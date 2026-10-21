//! Filters: which Notifications a View shows, written in GitHub's inbox filter syntax with ghd's extensions (see
//! `docs/adr/0001-filter-syntax.md`).

use std::fmt;
use std::mem::discriminant;
use std::ops::Range;
use std::str::FromStr;

use crate::inbox::{Notification, Reason, SubjectKind};

/// A rule that a Notification either matches or doesn't. The default Filter matches every Notification.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// One group per qualifier written without `-`: a Notification must match at least one condition of every group.
    /// Groups are never empty, and each holds one kind of [`Condition`].
    any_of: Vec<Vec<Condition>>,
    /// The qualifiers written with `-`: a Notification matching any of them is left out.
    none_of: Vec<Condition>,
}

impl Filter {
    /// Whether `notification` matches.
    pub fn matches(&self, notification: &Notification) -> bool {
        self.any_of.iter().all(|group| {
            group
                .iter()
                .any(|condition| condition.matches(notification))
        }) && !self
            .none_of
            .iter()
            .any(|condition| condition.matches(notification))
    }
}

/// Parses a filter: space-separated `qualifier:value` terms, each optionally negated with a leading `-`.
impl FromStr for Filter {
    type Err = FilterError;

    fn from_str(filter: &str) -> Result<Self, Self::Err> {
        let mut parsed = Self::default();
        for token in token_spans(filter) {
            let reject = |span: Range<usize>, problem: String| FilterError {
                filter: filter.to_owned(),
                span,
                problem,
            };
            let text = &filter[token.clone()];
            let (negated, term_start) = match text.strip_prefix('-') {
                Some(_) => (true, token.start + 1),
                None => (false, token.start),
            };
            let Some((qualifier, value)) = filter[term_start..token.end].split_once(':') else {
                return Err(reject(
                    token,
                    format!("expected qualifier:value, like `reason:mention`, not `{text}`"),
                ));
            };
            // The qualifier's span takes in the `:`, so that a missing value has something to point at.
            let qualifier_span = term_start..term_start + qualifier.len() + 1;
            let value_span = qualifier_span.end..token.end;
            if value.is_empty() {
                return Err(reject(
                    qualifier_span,
                    format!("`{qualifier}:` needs a value"),
                ));
            }

            let condition =
                Condition::parse(qualifier, value).map_err(|rejection| match rejection {
                    Rejection::Qualifier(problem) => reject(qualifier_span, problem),
                    Rejection::Value(problem) => reject(value_span, problem),
                })?;
            if negated {
                parsed.none_of.push(condition);
            } else {
                match parsed
                    .any_of
                    .iter_mut()
                    .find(|group| discriminant(&group[0]) == discriminant(&condition))
                {
                    Some(group) => group.push(condition),
                    None => parsed.any_of.push(vec![condition]),
                }
            }
        }
        Ok(parsed)
    }
}

/// What a single `qualifier:value` term asks of a Notification. Each variant is its own qualifier as far as
/// repeating goes; `is:` is two of them, because a Subject's kind and its read state are unrelated.
#[derive(Debug, Clone)]
enum Condition {
    /// `repo:`: the repository's `owner/name` fits this lowercase pattern, `*` standing for any run of characters.
    Repository(String),
    /// `org:`: the repository's owner is this one, lowercase.
    Owner(String),
    /// `is:` with a kind of Subject: the Subject is any of these kinds.
    Kind(Vec<SubjectKind>),
    /// `is:unread` (`true`) or `is:read` (`false`).
    Unread(bool),
    /// `reason:`.
    Reason(Reason),
}

/// Which part of a term is wrong, and why.
enum Rejection {
    Qualifier(String),
    Value(String),
}

impl Condition {
    /// The condition `qualifier:value` stands for; `value` isn't empty.
    fn parse(qualifier: &str, value: &str) -> Result<Self, Rejection> {
        match qualifier {
            "repo" if value.contains('/') => Ok(Self::Repository(value.to_ascii_lowercase())),
            "repo" => Err(Rejection::Value(
                "`repo:` needs owner/name; use `org:` for all of an owner's repositories".to_owned(),
            )),
            "org" => Ok(Self::Owner(value.to_ascii_lowercase())),
            "is" => Self::parse_is(value),
            "reason" => Self::parse_reason(value),
            "author" => Err(Rejection::Qualifier(
                "`author:` isn't supported yet: it needs details of the Subject that ghd doesn't fetch".to_owned(),
            )),
            _ => Err(Rejection::Qualifier(format!(
                "unknown qualifier `{qualifier}:`; ghd understands `repo:`, `org:`, `is:` and `reason:`"
            ))),
        }
    }

    fn parse_is(value: &str) -> Result<Self, Rejection> {
        use SubjectKind::{CheckSuite, Commit, Discussion, Issue, PullRequest, Release};
        let kinds = match value {
            "pr" | "pull-request" => vec![PullRequest],
            "issue" => vec![Issue],
            "issue-or-pull-request" => vec![Issue, PullRequest],
            "release" => vec![Release],
            "discussion" => vec![Discussion],
            "commit" => vec![Commit],
            "check-suite" => vec![CheckSuite],
            "unread" => return Ok(Self::Unread(true)),
            "read" => return Ok(Self::Unread(false)),
            "done" => {
                return Err(Rejection::Value(
                    "the Inbox never holds Done notifications, so `is:done` would match nothing"
                        .to_owned(),
                ));
            }
            "saved" => {
                return Err(Rejection::Value(
                    "GitHub's API doesn't say which notifications are saved".to_owned(),
                ));
            }
            "gist"
            | "repository-invitation"
            | "repository-vulnerability-alert"
            | "repository-advisory" => {
                return Err(Rejection::Value(format!(
                    "`is:{value}` isn't supported yet"
                )));
            }
            _ => return Err(Rejection::Value(format!("unknown `is:` value `{value}`"))),
        };
        Ok(Self::Kind(kinds))
    }

    fn parse_reason(value: &str) -> Result<Self, Rejection> {
        if let Some(reason) = Reason::from_name(value) {
            return Ok(Self::Reason(reason));
        }
        let hyphenated = value.replace('_', "-");
        let problem = if value == "participating" {
            "GitHub doesn't define which reasons `participating` covers; list the reasons instead, like \
             `reason:mention reason:review-requested`"
                .to_owned()
        } else if Reason::from_name(&hyphenated).is_some() {
            format!("reasons are written with hyphens: `{hyphenated}`")
        } else {
            format!("unknown reason `{value}`")
        };
        Err(Rejection::Value(problem))
    }

    fn matches(&self, notification: &Notification) -> bool {
        match self {
            Self::Repository(pattern) => {
                fits(pattern, &notification.repository.to_ascii_lowercase())
            }
            Self::Owner(owner) => notification
                .repository
                .split_once('/')
                .is_some_and(|(repository_owner, _)| repository_owner.eq_ignore_ascii_case(owner)),
            Self::Kind(kinds) => kinds.contains(&notification.subject_kind),
            Self::Unread(unread) => notification.unread == *unread,
            Self::Reason(reason) => notification.reason == *reason,
        }
    }
}

/// Whether all of `text` fits `pattern`, each `*` in `pattern` standing for any run of characters, the empty one
/// included.
fn fits(pattern: &str, text: &str) -> bool {
    let mut literals = pattern.split('*');
    let first = literals.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let mut literals: Vec<&str> = literals.collect();
    let Some(last) = literals.pop() else {
        // No `*` at all: the text must be the pattern.
        return rest.is_empty();
    };
    for literal in literals {
        match rest.find(literal) {
            Some(at) => rest = &rest[at + literal.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// The byte ranges of the whitespace-separated tokens in `text`.
fn token_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    for (at, character) in text.char_indices().chain([(text.len(), ' ')]) {
        match (start, character.is_whitespace()) {
            (None, false) => start = Some(at),
            (Some(token_start), true) => {
                spans.push(token_start..at);
                start = None;
            }
            _ => {}
        }
    }
    spans
}

/// Why a filter doesn't parse, and which part of it is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError {
    /// The whole filter, as written.
    filter: String,
    /// The bytes of `filter` that are wrong.
    span: Range<usize>,
    /// What's wrong with them, as one line.
    problem: String,
}

/// The problem, then the filter with the wrong part underlined on the next line.
impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let column = self.filter[..self.span.start].chars().count();
        let width = self.filter[self.span.clone()].chars().count().max(1);
        write!(
            f,
            "{}\n  {}\n  {}{}",
            self.problem,
            self.filter,
            " ".repeat(column),
            "^".repeat(width)
        )
    }
}

impl std::error::Error for FilterError {}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::inbox::{Reason, SubjectKind, ThreadId};

    /// An unread pull request in `repository`, there because of `reason`.
    fn notification(repository: &str, reason: Reason) -> Notification {
        Notification {
            thread: ThreadId(1),
            subject_url: format!("https://github.com/{repository}/pull/1"),
            repository: repository.to_owned(),
            subject_kind: SubjectKind::PullRequest,
            subject_title: "A pull request".to_owned(),
            reason,
            unread: true,
            updated_at: Utc::now(),
        }
    }

    fn filter(text: &str) -> Filter {
        text.parse()
            .unwrap_or_else(|error| panic!("{text:?} should parse:\n{error}"))
    }

    fn problem(text: &str) -> FilterError {
        match text.parse::<Filter>() {
            Ok(_) => panic!("{text:?} should be rejected"),
            Err(error) => error,
        }
    }

    /// The part of the filter the error points at.
    fn culprit(error: &FilterError) -> &str {
        &error.filter[error.span.clone()]
    }

    #[test]
    fn empty_filter_matches_everything() {
        let review = notification("wcgw/ghd", Reason::ReviewRequested);
        assert!(Filter::default().matches(&review));
        assert!(filter("").matches(&review));
        assert!(filter("   ").matches(&review));
    }

    #[test]
    fn repeated_qualifier_is_any_of() {
        let reviews = filter("reason:review-requested reason:team-mention");

        assert!(reviews.matches(&notification("wcgw/ghd", Reason::ReviewRequested)));
        assert!(reviews.matches(&notification("wcgw/ghd", Reason::TeamMention)));
        assert!(!reviews.matches(&notification("wcgw/ghd", Reason::Mention)));
    }

    #[test]
    fn different_qualifiers_must_all_match() {
        let reviews_at_wcgw = filter("reason:review-requested org:wcgw");

        assert!(reviews_at_wcgw.matches(&notification("wcgw/ghd", Reason::ReviewRequested)));
        assert!(
            !reviews_at_wcgw.matches(&notification("kuadrant/limitador", Reason::ReviewRequested))
        );
        assert!(!reviews_at_wcgw.matches(&notification("wcgw/ghd", Reason::Mention)));
    }

    #[test]
    fn negated_qualifiers_each_exclude() {
        let quiet = filter("-reason:subscribed -reason:ci-activity");

        assert!(quiet.matches(&notification("wcgw/ghd", Reason::Mention)));
        assert!(!quiet.matches(&notification("wcgw/ghd", Reason::Subscribed)));
        assert!(!quiet.matches(&notification("wcgw/ghd", Reason::CiActivity)));
    }

    #[test]
    fn negation_narrows_what_the_rest_selects() {
        let wcgw_but_ghd = filter("org:wcgw -repo:wcgw/ghd");

        assert!(wcgw_but_ghd.matches(&notification("wcgw/other", Reason::Mention)));
        assert!(!wcgw_but_ghd.matches(&notification("wcgw/ghd", Reason::Mention)));
        assert!(!wcgw_but_ghd.matches(&notification("kuadrant/limitador", Reason::Mention)));
    }

    #[test]
    fn repo_matches_owner_and_name_ignoring_case() {
        let ghd = filter("repo:WCGW/ghd");

        assert!(ghd.matches(&notification("wcgw/ghd", Reason::Mention)));
        assert!(!ghd.matches(&notification("wcgw/ghd-extras", Reason::Mention)));
        assert!(!ghd.matches(&notification("other/ghd", Reason::Mention)));
    }

    #[test]
    fn repo_wildcard_matches_any_run_of_characters() {
        let cases = [
            ("repo:wcgw/*", "wcgw/ghd", true),
            ("repo:wcgw/*", "kuadrant/ghd", false),
            ("repo:*/ghd", "wcgw/ghd", true),
            ("repo:*/ghd", "wcgw/ghd-extras", false),
            ("repo:kuadrant/lim*or", "kuadrant/limitador", true),
            ("repo:kuadrant/lim*or", "kuadrant/limitador-server", false),
            ("repo:kuadrant/*-*", "kuadrant/authorino-operator", true),
            ("repo:kuadrant/*-*", "kuadrant/authorino", false),
            ("repo:*/*", "anyone/anything", true),
        ];
        for (text, repository, expected) in cases {
            assert_eq!(
                filter(text).matches(&notification(repository, Reason::Mention)),
                expected,
                "{text} on {repository}"
            );
        }
    }

    #[test]
    fn org_matches_the_owner_ignoring_case() {
        let wcgw = filter("org:WCGW");

        assert!(wcgw.matches(&notification("wcgw/ghd", Reason::Mention)));
        assert!(!wcgw.matches(&notification("wcgw-labs/ghd", Reason::Mention)));
    }

    #[test]
    fn is_tells_pull_requests_and_issues_apart() {
        let pr = notification("wcgw/ghd", Reason::Mention);
        let issue = Notification {
            subject_kind: SubjectKind::Issue,
            ..pr.clone()
        };
        let release = Notification {
            subject_kind: SubjectKind::Release,
            ..pr.clone()
        };

        for text in ["is:pr", "is:pull-request"] {
            assert!(filter(text).matches(&pr), "{text}");
            assert!(!filter(text).matches(&issue), "{text}");
        }
        assert!(filter("is:issue").matches(&issue));
        assert!(!filter("is:issue").matches(&pr));
        let either = filter("is:issue-or-pull-request");
        assert!(either.matches(&pr) && either.matches(&issue) && !either.matches(&release));
        assert!(filter("is:release").matches(&release));
    }

    #[test]
    fn read_state_and_kind_are_separate_qualifiers() {
        let unread_pr = notification("wcgw/ghd", Reason::Mention);
        let read_pr = Notification {
            unread: false,
            ..unread_pr.clone()
        };
        let unread_issue = Notification {
            subject_kind: SubjectKind::Issue,
            ..unread_pr.clone()
        };

        assert!(filter("is:unread").matches(&unread_pr));
        assert!(!filter("is:unread").matches(&read_pr));
        assert!(filter("is:read").matches(&read_pr));

        // Kind and read state each get their own any-of, and both must match.
        let unread_prs = filter("is:pr is:unread");
        assert!(unread_prs.matches(&unread_pr));
        assert!(!unread_prs.matches(&read_pr));
        assert!(!unread_prs.matches(&unread_issue));
    }

    #[test]
    fn unknown_reason_is_only_reachable_by_negation() {
        let new_reason = notification("wcgw/ghd", Reason::Other("brand_new_reason".to_owned()));

        assert!(!filter("reason:mention").matches(&new_reason));
        assert!(filter("-reason:mention").matches(&new_reason));
    }

    #[test]
    fn every_api_reason_has_a_hyphenated_name() {
        let names = [
            "agent-session-finished",
            "approval-requested",
            "assign",
            "author",
            "ci-activity",
            "comment",
            "invitation",
            "manual",
            "member-feature-requested",
            "mention",
            "review-requested",
            "security-advisory-credit",
            "security-alert",
            "state-change",
            "subscribed",
            "team-mention",
        ];
        for name in names {
            let reason = Reason::from_api(&name.replace('-', "_"));
            assert!(
                filter(&format!("reason:{name}")).matches(&notification("wcgw/ghd", reason)),
                "{name}"
            );
        }
    }

    #[test]
    fn rejections_point_at_the_culprit() {
        let cases = [
            (
                "reason:review-requestd org:wcgw",
                "review-requestd",
                "unknown reason",
            ),
            (
                "reason:review_requested",
                "review_requested",
                "review-requested",
            ),
            ("reason:participating", "participating", "list the reasons"),
            (
                "org:wcgw author:alexsnaps",
                "author:",
                "isn't supported yet",
            ),
            ("org:wcgw title:polling", "title:", "unknown qualifier"),
            ("repo:ghd", "ghd", "owner/name"),
            ("is:done", "done", "never holds"),
            ("is:saved", "saved", "saved"),
            ("is:gist", "gist", "isn't supported yet"),
            ("is:everything", "everything", "unknown"),
            ("reason: org:wcgw", "reason:", "needs a value"),
            ("org:wcgw polling", "polling", "qualifier:value"),
            ("org:wcgw -", "-", "qualifier:value"),
        ];
        for (text, culprit_text, hint) in cases {
            let error = problem(text);
            assert_eq!(culprit(&error), culprit_text, "{text}");
            assert!(error.problem.contains(hint), "{text}: {}", error.problem);
        }
    }

    #[test]
    fn error_underlines_the_culprit() {
        let error = problem("org:wcgw reason:review-requestd");

        assert_eq!(
            error.to_string(),
            "unknown reason `review-requestd`\n  \
             org:wcgw reason:review-requestd\n  \
             \x20               ^^^^^^^^^^^^^^^"
        );
    }
}
