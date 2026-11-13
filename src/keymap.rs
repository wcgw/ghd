//! Key bindings (the kit's D12): the kit's, pure Vim, and ghd's defaults, bound by the ex commands they run, so
//! `:map` can rebind any of them.

pub use wcgw_kit::keymap::*;

/// What ghd's commands on Notifications are about, in `:help`: after opening, before buffers.
pub const TRIAGE: Topic = Topic::new(75, "Notifications");

/// voight's, in `:help`.
pub const VOIGHT: Topic = Topic::new(76, "voight");

/// ghd's default bindings: keys, the ex command each runs, what which-key says of it, and its `:help` topic.
const DEFAULTS: [(&str, &str, &str, Topic); 22] = [
    ("gx", "browse link", "open link in browser", Topic::OPENING),
    ("gX", "browse", "open on GitHub", Topic::OPENING),
    ("gy", "yanklink", "yank its link", Topic::YANKING),
    ("gf", "files", "pull request's files", Topic::OPENING),
    ("p", "preview", "toggle the Preview", Topic::OPENING),
    ("<leader>w", "whole", "files whole or diffs", Topic::OPENING),
    (
        "<2-LeftMouse>",
        "open",
        "open under the mouse",
        Topic::OPENING,
    ),
    ("x", "done", "mark Done, or bring back", TRIAGE),
    ("I", "read", "mark read", TRIAGE),
    ("D", "donelist", "toggle the Done list", TRIAGE),
    ("u", "unreadonly", "toggle unread only", TRIAGE),
    ("A", "toggle!", "select all shown, or none", TRIAGE),
    ("<Esc>", "clear", "clear selection or filter", TRIAGE),
    ("<leader>1", "buffer 1", "View 1", Topic::BUFFERS),
    ("<leader>2", "buffer 2", "View 2", Topic::BUFFERS),
    ("<leader>3", "buffer 3", "View 3", Topic::BUFFERS),
    ("<leader>4", "buffer 4", "View 4", Topic::BUFFERS),
    ("<leader>5", "buffer 5", "View 5", Topic::BUFFERS),
    ("<leader>6", "buffer 6", "View 6", Topic::BUFFERS),
    ("<leader>7", "buffer 7", "View 7", Topic::BUFFERS),
    ("<leader>8", "buffer 8", "View 8", Topic::BUFFERS),
    ("<leader>9", "buffer 9", "View 9", Topic::BUFFERS),
];

/// ghd's defaults that go on past a command: `s` selects and moves down, `<leader>f` starts typing a filter. Keys,
/// the keys they stand for, what which-key says of them, and their `:help` topic.
const KEYS: [(&str, &str, &str, Topic); 2] = [
    ("s", ":toggle<CR>j", "select or not, and down", TRIAGE),
    ("<leader>f", ":filter<Space>", "filter the View", TRIAGE),
];

/// The kit's bindings and ghd's.
#[must_use]
pub fn keymap() -> Keymap {
    let mut keymap = Keymap::new();
    for (notation, command, description, topic) in DEFAULTS {
        keymap.bind(&[Mode::Normal], notation, command, description, topic);
    }
    for (notation, keys, description, topic) in KEYS {
        keymap.bind_keys(&[Mode::Normal], notation, keys, description, topic);
    }
    keymap
}
