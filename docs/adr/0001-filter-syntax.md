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

Negated qualifiers are exclusions: a Notification matching any of them is left out, so `-reason:a -reason:b` drops both
(read as OR, it would let nearly everything through). `is:` counts as three qualifiers for the repeat rule, because it
covers three unrelated things: the Subject's kind (`is:pr`, `is:issue`, …), its read state (`is:read`, `is:unread`) and
its State (`is:open`, `is:draft`, `is:merged`, `is:closed`, `is:completed`, `is:not-planned`). So `is:pr is:unread`
means unread pull requests, not "pull requests or anything unread". The States follow GitHub's search: `is:open`
includes drafts, and `is:closed` includes merged pull requests and issues closed for any reason; `is:completed` and
`is:not-planned` are ghd's, where GitHub's search uses `reason:` (taken here by the notification's Reason). A
Notification whose State isn't known (not looked up yet, or a kind with none) matches no `is:` State, only negated
ones, as with unknown reasons. `is:` values ghd can't
evaluate are rejected rather than matching nothing: `is:done` (the Inbox never holds Done notifications), `is:saved`
(the API doesn't expose it) and the kinds whose API names ghd doesn't know yet (`gist`, `repository-invitation`,
`repository-vulnerability-alert`, `repository-advisory`).

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
- `author:` is a valid GitHub qualifier that ghd can't evaluate yet: it needs the Subject's details, which ghd doesn't
  look up. A Subject's State is looked up apart from the Inbox, so a View on a State fills in a moment after the Inbox
  arrives.
