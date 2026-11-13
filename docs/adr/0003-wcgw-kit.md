# ghd runs on wcgw-kit

ghd, devnull, muttrs and slackers each hand-rolled their terminal loop, keys, command line, themes and chrome, and
drifted apart. [wcgw-kit](https://github.com/wcgw/wcgw-kit) gives them one Neovim look & feel (its `docs/plan.md`,
D1–D20): it owns the loop, keys and motions, `:` commands, windows, chrome, colorschemes, pictures and the mouse; an
application owns its domain and shows it as the kit's buffers, windows, commands and statusline segments. ghd moves
onto it (the plan's M7).

What goes, for the kit's: ghd's key `match`, `Command` enum, command line and its history, help popup, panes and
their scrollbars, border badges, `Scrolled`, the Files screen's own search, `themes/*.toml` and `theme.rs`, the
Search (`search.rs`), and tui-markdown. What stays ghd's: the GitHub client, the Inbox, Filters (ADR 0001), inferring
Done (ADR 0002), the cache, Previews, Changed files, Assessments.

How ghd's screen became the kit's:

- Each View is a listed buffer, a block per Notification, its columns laid out by ghd for the window's width
  (`nowrap`), so the cursor stays on its Notification as the Inbox changes.
- The Preview is a window below the list, a buffer per Preview: its body and each comment a block, `]]` and `[[`
  between them, opening on the comment the Notification is about. It shows all of them, where ghd showed one at a
  time and stepped with `J` and `K`.
- The Files screen shows the list of Changed files in the list's window and the file at its cursor below, a buffer
  per file as it shows.
- Every action is an ex command with a default key (the kit's D12: Vim's motions stay Vim's), so `:map` reaches
  anything. The Search becomes `:filter`, a Filter typed for the moment; looking for text is the kit's `/`.
- Markdown is laid out by `wcgw-kit-text`, after GitHub's rules (`markdown::as_github_shows`); code is highlighted in
  the standard syntax groups, links refer to their address for `:open`, and the Heatmap shades spans.
- Colours are highlight groups (`Ghd*`, `VoightHeat*`) linked to standard ones, with ghd's own colours for
  `tokyonight-moon` and the ANSI `default` scheme, as its two themes had them.
- `config.toml` keeps what ghd shows; how it looks and its keys go to the kit's `init.vim`.
- voight's thread is voight's shared `Assessor`; ghd keeps which text an Assessment is of and those made.

## Considered Options

- **Keep ghd's own UI**: no churn, but the four apps go on drifting, and ghd keeps its own command line, history,
  help and themes to maintain.
- **Use the kit's widgets only, keeping ghd's loop**: the kit's D1 rejects it: the feel is in the dispatch, which
  would drift again.

## Consequences

- Keys change where Vim has them: `e` is `x`, `f` is `gf`, `w` is `<leader>w`, `Space` is `s`, `Tab` and `1`–`9`
  are `gt` and `<leader>1`–`9`, `r` is `<C-l>`, `H` is `<leader>h`, `?` is `:help`, `o` and `Enter` are `<CR>` and
  `gX`. `q` closes a window, quitting from the last.
- A View's tab no longer counts its unread Notifications: buffer names don't change in the kit, so the count is in
  the statusline, for the View on screen.
- The row at the cursor is drawn as `CursorLine` by ghd, as the kit has no `'cursorline'` yet.
- The kit can't hand the application back when it quits: ghd leaves its snapshot and the changes GitHub hasn't
  answered behind as it's dropped, and `main` waits for those (10 s at most) and keeps the Inbox after the terminal is
  restored.
- Only the View on screen comes back where its cursor was from the cache: the kit sets a window's cursor only when it
  first shows a buffer.
- A View's lines are laid out for the terminal's width, as the kit doesn't tell a window's: a View in a `:vsplit`
  wraps.
