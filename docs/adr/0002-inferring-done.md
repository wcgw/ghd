# ghd infers which notifications are Done

GitHub's REST `GET /notifications?all=true`, which ghd builds the Inbox from, keeps listing threads marked Done, and
nothing in a listed thread says it is Done. (Seen on 2026-10-21: `DELETE /notifications/threads/{id}` answered `204`,
the thread left github.com's inbox, and every later list still had it, read, with no `last_read_at`.) So ghd hides,
on top of what GitHub lists:

- the threads ghd itself marked Done, kept in its cache, until they get newer activity or GitHub lists them unread
  again (marked unread on github.com, say), as GitHub brings such threads back to the inbox;
- read threads with no `last_read_at`, as the thread marked Done had.

## Considered Options

- **Unread only** (`all=false`): exact for unread notifications wherever they were read or marked Done, but read
  notifications that aren't Done disappear too, against the Inbox model chosen at the start.
- **ghd's own Done only:** reliable for what ghd marks Done, but everything marked Done elsewhere stays listed (273
  where github.com showed 83, on 2026-10-21).

## Consequences

- The `last_read_at` guess is not GitHub's word. On 2026-10-21 ghd listed 112 where github.com showed 83: at least 29
  threads marked Done elsewhere have a `last_read_at`, and stay. A read notification that isn't Done but has no
  `last_read_at` would be hidden; none was known to.
- Should GitHub's API ever say which threads are Done, both inferences go.
