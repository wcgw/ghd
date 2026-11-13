#!/usr/bin/env python3
"""Turns a theme file of ghd's from before wcgw-kit (`themes/<name>.toml`) into `:hi` lines for `init.vim`.

    scripts/theme-to-vim.py ~/.config/ghd/themes/mine.toml >> ~/.config/ghd/init.vim

Each style goes to the highlight group that does its job now: ghd's own (`GhdRepository`, `GhdStateMerged`, ...) or a
standard one (`CursorLine`, `Keyword`, `DiffAdd`, ...). A style with no group left, like the old border badges', is
said in a comment. `dim` has no `gui=` attribute: it is left out, also said in a comment.
"""

import sys
import tomllib

# Where each style of a theme file goes now.
GROUPS = {
    "text": "Normal",
    "border": "WinSeparator",
    "tab": "TabLine",
    "tab_current": "TabLineSel",
    "cursor": "CursorLine",
    "selected": "GhdSelected",
    "unread": "GhdUnread",
    "read": "GhdRead",
    "unread_marker": "GhdUnreadMarker",
    "repository": "GhdRepository",
    "title": "GhdTitle",
    "reason": "GhdReason",
    "age": "GhdAge",
    "message": "GhdMessage",
    "error": "GhdError",
    "completion": "Pmenu",
    "completion_current": "PmenuSel",
    "found": "Search",
    "found_current": "CurSearch",
    "hint": "GhdHint",
    "help_keys": "WhichKey",
    "kind.pull_request": "GhdKindPullRequest",
    "kind.issue": "GhdKindIssue",
    "kind.release": "GhdKindRelease",
    "kind.discussion": "GhdKindDiscussion",
    "kind.commit": "GhdKindCommit",
    "kind.check_suite": "GhdKindCheckSuite",
    "kind.other": "GhdKindOther",
    "state.open": "GhdStateOpen",
    "state.draft": "GhdStateDraft",
    "state.merged": "GhdStateMerged",
    "state.closed": "GhdStateClosed",
    "state.completed": "GhdStateCompleted",
    "state.not_planned": "GhdStateNotPlanned",
    "markdown.heading": "@markup.heading",
    "markdown.code": "@markup.raw",
    "markdown.link": "@markup.link",
    "markdown.quote": "@markup.quote",
    "markdown.muted": "NonText",
    "syntax.comment": "Comment",
    "syntax.keyword": "Keyword",
    "syntax.string": "String",
    "syntax.constant": "Constant",
    "syntax.function": "Function",
    "syntax.type": "Type",
    "syntax.operator": "Operator",
    "diff.added": "Added",
    "diff.removed": "Removed",
    "diff.changed": "Changed",
    "diff.added_line": "DiffAdd",
    "diff.removed_line": "DiffDelete",
    "ci.passing": "GhdCiPassing",
    "ci.failing": "GhdCiFailing",
    "ci.pending": "GhdCiPending",
    "heatmap.human": "VoightHeatHuman",
    "heatmap.even": "VoightHeatEven",
    "heatmap.llm": "VoightHeatLlm",
}

# The colour names `:hi` takes, besides `#rrggbb`.
NAMED = {"black", "red", "green", "yellow", "blue", "magenta", "cyan", "gray", "grey", "darkgray", "darkgrey", "white"}

# Attributes, as `gui=` names them.
ATTRIBUTES = {"bold": "bold", "italic": "italic", "underlined": "underline", "reversed": "reverse"}


def styles(table, prefix=""):
    """Each style of `table`, a theme file's `[styles]` or a table in it, by its dotted name."""
    for key, value in table.items():
        name = f"{prefix}{key}"
        if isinstance(value, dict) and value and all(isinstance(v, dict) for v in value.values()):
            yield from styles(value, f"{name}.")
        else:
            yield name, value


def color(value, palette):
    """`value`, a palette name or a colour, as `:hi` takes it; `None` when it takes no such colour."""
    value = palette.get(value, value)
    if value.startswith("#") and len(value) == 7:
        return value
    return value.lower() if value.lower() in NAMED else None


def main(path):
    with open(path, "rb") as file:
        theme = tomllib.load(file)
    palette = theme.get("palette", {})
    print(f'" From {path}')
    for name, style in styles(theme.get("styles", {})):
        group = GROUPS.get(name)
        if group is None:
            print(f'" {name}: no group does its job now')
            continue
        attrs = []
        for key in ("fg", "bg"):
            if key in style:
                value = color(style[key], palette)
                if value is None:
                    print(f'" {name}: {key} = "{style[key]}" is no colour :hi takes')
                else:
                    attrs.append(f"gui{key}={value}")
        gui = [ATTRIBUTES[key] for key in ATTRIBUTES if style.get(key)]
        if gui:
            attrs.append("gui=" + ",".join(gui))
        if style.get("dim"):
            print(f'" {name}: dim has no gui= attribute, left out')
        print(f"hi {group} {' '.join(attrs) if attrs else 'NONE'}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
