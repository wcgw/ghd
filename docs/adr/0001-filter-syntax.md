# Filters use GitHub's inbox filter syntax, extended

A Filter is a single line written in GitHub's notification [inbox
filter](https://docs.github.com/en/subscriptions-and-notifications/reference/inbox-filters) syntax (`repo:`, `org:`,
`is:`, `reason:` with GitHub's hyphenated values, e.g. `reason:review-requested`), and ghd evaluates it locally because
the notifications API offers no filtering. ghd extends that syntax: `-` negates a qualifier, `repo:` accepts `*`
wildcards, and `is:pr` / `is:issue` are distinct (GitHub's inbox can't tell them apart, the API's subject type can). A
repeated qualifier is OR, different qualifiers are AND; an unknown qualifier is a configuration error, never a Filter
that silently matches nothing.

`reason:` accepts every reason in the [REST API's
list](https://docs.github.com/en/rest/activity/notifications#about-notification-reasons), hyphenated
(`reason:ci-activity`, `reason:subscribed`), which is wider than the inbox filter's own list; any other value is a
configuration error. `reason:participating` is rejected: GitHub doesn't define which reasons it groups, and any set we
picked would silently disagree with github.com.

## Considered Options

- **Structured TOML** (`reason = [...]`, `not_type = [...]`): parsing comes for free, but OR across fields and negation
  get awkward quickly.
- **A full boolean expression language** (`(a or b) and not c`): more power than the current fields need, and a bigger
  parser to own.

## Consequences

- GitHub's syntax doesn't say how a repeated qualifier combines, and GitHub *search* treats `label:a label:b` as AND. We
  chose OR because every field a Filter can match today has a single value, so a repeated qualifier as AND could never
  match. When fields with several values (labels, assignees) are added, their repeat rule must be decided explicitly.
- GitHub adds reasons over time (`agent_session_finished` is recent). A Notification with a reason ghd doesn't know is
  still shown, with the reason as GitHub sent it, but no `reason:` value selects it until ghd learns that reason;
  negated `-reason:` Filters still let it through.
- `author:` and subject state (e.g. merged) are valid GitHub qualifiers that ghd can't evaluate yet: they need the
  Subject's details, which aren't fetched yet.
