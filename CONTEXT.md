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

**State**:
Where a **Subject** stands on GitHub: a pull request open, draft, merged or closed; an issue open, or closed as completed or as not planned. Other kinds of **Subject** have none here.
_Avoid_: status

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

**Done list**:
The **Notifications** ghd takes for **Done**, shown in every **View** in place of the **Inbox** when the user asks; ghd's view of **Done**, not GitHub's.
_Avoid_: archive, done view

**Filter**:
A rule that a **Notification** either matches or doesn't; it has no name and is never shown on its own.
_Avoid_: query, rule, search

**Cursor**:
The highlighted row of the **View** on screen: what a single action (open, mark read, mark **Done**) acts on when nothing is selected.
_Avoid_: selection, current row

**Selection**:
The **Notifications** the user has marked to act on together; when there is one, actions act on all of it instead of the **Cursor**.
_Avoid_: marked, picked, cursor

**Search**:
Text the user types, for the moment, to narrow the **View** on screen to the **Notifications** it matches; unlike a **Filter**, it matches parts of words and isn't kept anywhere.
_Avoid_: filter, query, find

**Preview**:
What ghd shows of the **Subject** at the **Cursor** without opening it: a header (number, title, **State**, author, labels, and who merged or closed it), then what its **Reason** calls for: the comment that mentions the user, the description for a review request or an assignment, or else the latest comment GitHub points to, or the **Subject**'s body. Showing it never clears **Unread** nor marks **Done**.
_Avoid_: details, peek

**Assessment**:
voight's estimate, as a percentage, of how much of the text a **Preview** shows was written by an LLM; made only of text with enough prose to mean something. Never evidence of authorship.
_Avoid_: score, AI detection, verdict

**View**:
A named tab, defined in configuration, that shows the **Inbox** **Notifications** matching its **Filter**.
_Avoid_: folder, bucket, tab, filter

## Relationships

- An **Inbox** belongs to exactly one **Account**
- A **Notification** is about exactly one **Subject** and carries one **Reason**
- A **Subject**'s **State** isn't part of its **Notification**: ghd looks it up apart, again whenever the **Notification** has new activity, and shows the last one it knows
- A **Filter** or a **Search** can ask for a **State**; a **Notification** whose **State** isn't known matches none, only the negation of one
- GitHub's API doesn't say which **Notifications** are **Done**: ghd takes for **Done** those it marked **Done** itself (until they get new activity or are **Unread** again) and read ones GitHub has no read time for (see `docs/adr/0002-inferring-done.md`)
- Bringing a **Notification** back from the **Done list** puts it in ghd's **Inbox** only: GitHub's API can't move a thread back to its inbox, so GitHub keeps it **Done**
- Opening a **Notification** clears **Unread**; marking it **Done** removes it from the **Inbox** and so from every **View**
- A **View** has exactly one **Filter**
- There is always one built-in **View**, named "Inbox", that shows the whole **Inbox**; it comes first and can't be removed or redefined, and configured **Views** follow in config order
- A **Notification** can appear in zero, one or many configured **Views** (and always in the Inbox **View**); Views do not claim Notifications from each other
- There is one **Selection** at a time, across **Views** like the **Search**: it can hold **Notifications** the **View** on screen doesn't show, and an action acts on all of them
- There is at most one **Search** at a time, and it narrows whichever **View** is on screen, so it follows the user from **View** to **View**
- Each **View** can be narrowed, for the moment, to its **Unread** **Notifications**, independently of the other **Views**; it doesn't narrow the **Done list**
- **Unread** can only be cleared from ghd, never set again: GitHub's API has no way to mark a **Notification** unread

## Example dialogue

> **Dev:** "I clicked a review request in the browser, so it's not **Unread** any more — does it drop out of my 'Reviews' **View**?"
> **Domain expert:** "No. Reading only clears **Unread**; it stays in the **Inbox**, and so in every **View** whose **Filter** matches it, until you mark it **Done**."
> **Dev:** "And since it's a review request on `wcgw/ghd`, it also shows in my 'ghd' **View**?"
> **Domain expert:** "If both **Filters** match, yes — **Views** never take a **Notification** away from each other. And the Inbox **View** always shows it regardless."

## Flagged ambiguities

- "Inbox" names both the set of not-Done **Notifications** and the built-in **View** that shows it. Resolved: **Inbox** is the set; say "the Inbox View" for the tab.
- "read" and "done" are easy to conflate. Resolved: reading a **Notification** only clears **Unread**; only marking it **Done** removes it from the **Inbox**.
- "selection" meant the highlighted row until several **Notifications** could be marked. Resolved: the highlighted row is the **Cursor**; the **Selection** is what's marked.
- "search" could mean a **Filter** or a **Search**. Resolved: a **Filter** belongs to a **View** and is configured; a **Search** is typed, temporary, and narrows the **View** on screen.
- "filters/views" was used as if they were one concept. Resolved: a **Filter** is the matching rule, a **View** is the named tab in the TUI that applies one.
