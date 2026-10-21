# ghd

A terminal UI to GitHub. It starts with notifications: retrieving them and organizing them into user-defined views.

## Language

**Account**:
The GitHub identity ghd acts as, a user on a host (github.com or a GitHub Enterprise Server); exactly one per session.
_Avoid_: login, profile

**Notification**:
An entry in the user's GitHub notifications inbox, about a single **Subject**.

**Subject**:
The GitHub item a **Notification** is about: an issue, pull request, release, discussion, commit or check suite.

**Reason**:
Why GitHub notified the user about a **Subject** (e.g. review requested, mentioned, author, subscribed).

**Inbox**:
The user's **Notifications** that haven't been marked **Done**, read or not; everything ghd shows comes from it.
_Avoid_: feed, list

**Unread**:
A **Notification** with activity the user hasn't seen yet; reading it doesn't remove it from the **Inbox**.

**Done**:
A **Notification** the user has finished with; it leaves the **Inbox**.
_Avoid_: archived, dismissed, read

**Filter**:
A rule that a **Notification** either matches or doesn't; it has no name and is never shown on its own.
_Avoid_: query, rule, search

**View**:
A named tab, defined in configuration, that shows the **Inbox** **Notifications** matching its **Filter**.
_Avoid_: folder, bucket, tab, filter

## Relationships

- An **Inbox** belongs to exactly one **Account**
- A **Notification** is about exactly one **Subject** and carries one **Reason**
- Opening a **Notification** clears **Unread**; marking it **Done** removes it from the **Inbox** and so from every **View**
- A **View** has exactly one **Filter**
- There is always one built-in **View**, named "Inbox", that shows the whole **Inbox**; it comes first and can't be removed or redefined, and configured **Views** follow in config order
- A **Notification** can appear in zero, one or many configured **Views** (and always in the Inbox **View**); Views do not claim Notifications from each other

## Example dialogue

> **Dev:** "I clicked a review request in the browser, so it's not **Unread** any more — does it drop out of my 'Reviews' **View**?"
> **Domain expert:** "No. Reading only clears **Unread**; it stays in the **Inbox**, and so in every **View** whose **Filter** matches it, until you mark it **Done**."
> **Dev:** "And since it's a review request on `wcgw/ghd`, it also shows in my 'ghd' **View**?"
> **Domain expert:** "If both **Filters** match, yes — **Views** never take a **Notification** away from each other. And the Inbox **View** always shows it regardless."

## Flagged ambiguities

- "Inbox" names both the set of not-Done **Notifications** and the built-in **View** that shows it. Resolved: **Inbox** is the set; say "the Inbox View" for the tab.
- "read" and "done" are easy to conflate. Resolved: reading a **Notification** only clears **Unread**; only marking it **Done** removes it from the **Inbox**.
- "filters/views" was used as if they were one concept. Resolved: a **Filter** is the matching rule, a **View** is the named tab in the TUI that applies one.
