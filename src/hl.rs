//! ghd's highlight groups (the kit's D7): those of what ghd shows, each linked to a standard group so any
//! colorscheme colours it, with ghd's own colours for `tokyonight-moon`, as its theme had them, and for the ANSI
//! `default` scheme where a link alone reads badly.

pub use wcgw_kit::hl::*;

/// An unread Notification's row, over its columns' groups.
pub const UNREAD: Group = Group::new("GhdUnread");
/// A read Notification's row, over its columns' groups.
pub const READ: Group = Group::new("GhdRead");
/// A selected Notification's row, over its other groups but the cursor's, and the `▌` marking it.
pub const SELECTED: Group = Group::new("GhdSelected");
/// The `●` before an unread Notification.
pub const UNREAD_MARKER: Group = Group::new("GhdUnreadMarker");
/// A Notification's repository.
pub const REPOSITORY: Group = Group::new("GhdRepository");
/// A Notification's title.
pub const TITLE: Group = Group::new("GhdTitle");
/// A Notification's Reason; a Subject's labels.
pub const REASON: Group = Group::new("GhdReason");
/// How long ago; a Subject's number; who commented when.
pub const AGE: Group = Group::new("GhdAge");
/// What's said in place of a list or a Preview: fetching, or nothing to show.
pub const MESSAGE: Group = Group::new("GhdMessage");
/// What went wrong, said in place of a list or a Preview.
pub const ERROR: Group = Group::new("GhdError");
/// A hint, such as why a mention couldn't be looked for.
pub const HINT: Group = Group::new("GhdHint");
/// A Preview's title.
pub const PREVIEW_TITLE: Group = Group::new("GhdPreviewTitle");
/// A pull request's checks: all passed.
pub const CI_PASSING: Group = Group::new("GhdCiPassing");
/// Some failed.
pub const CI_FAILING: Group = Group::new("GhdCiFailing");
/// Some haven't finished, none failed.
pub const CI_PENDING: Group = Group::new("GhdCiPending");

/// A Subject's number in a Preview, as `#42`.
pub const REFERENCE: Group = Group::new("GhdReference");
/// A Subject's labels in a Preview.
pub const LABELS: Group = Group::new("GhdLabels");
/// A hunk's `@@` line in a file's diff.
pub const HUNK: Group = Group::new("GhdHunk");
/// A Changed file's path on the Files screen.
pub const PATH: Group = Group::new("GhdPath");

/// The kind column of a pull request, and so on for the other kinds of Subject.
pub const KIND_PULL_REQUEST: Group = Group::new("GhdKindPullRequest");
pub const KIND_ISSUE: Group = Group::new("GhdKindIssue");
pub const KIND_RELEASE: Group = Group::new("GhdKindRelease");
pub const KIND_DISCUSSION: Group = Group::new("GhdKindDiscussion");
pub const KIND_COMMIT: Group = Group::new("GhdKindCommit");
pub const KIND_CHECK_SUITE: Group = Group::new("GhdKindCheckSuite");
/// A kind of Subject ghd doesn't know.
pub const KIND_OTHER: Group = Group::new("GhdKindOther");

/// A pull request or an issue in this State: its kind column, and its State in a Preview.
pub const STATE_OPEN: Group = Group::new("GhdStateOpen");
pub const STATE_DRAFT: Group = Group::new("GhdStateDraft");
pub const STATE_MERGED: Group = Group::new("GhdStateMerged");
pub const STATE_CLOSED: Group = Group::new("GhdStateClosed");
pub const STATE_COMPLETED: Group = Group::new("GhdStateCompleted");
pub const STATE_NOT_PLANNED: Group = Group::new("GhdStateNotPlanned");

/// voight's Heatmap: text in a window voight scored 0%.
pub const HEAT_HUMAN: Group = Group::new("VoightHeatHuman");
/// … scored 50%.
pub const HEAT_EVEN: Group = Group::new("VoightHeatEven");
/// … scored 100%.
pub const HEAT_LLM: Group = Group::new("VoightHeatLlm");

/// ghd's groups and the groups they are drawn as when a colorscheme says nothing of them.
const GROUPS: [(Group, Group); 35] = [
    (UNREAD, Group::BOLD),
    (READ, Group::COMMENT),
    (SELECTED, Group::VISUAL),
    (UNREAD_MARKER, Group::DIAGNOSTIC_WARN),
    (REPOSITORY, Group::DIRECTORY),
    (TITLE, Group::NORMAL),
    (REASON, Group::STATEMENT),
    (AGE, Group::COMMENT),
    (MESSAGE, Group::COMMENT),
    (ERROR, Group::ERROR_MSG),
    (HINT, Group::NON_TEXT),
    (PREVIEW_TITLE, Group::TITLE),
    (CI_PASSING, Group::DIAGNOSTIC_OK),
    (CI_FAILING, Group::DIAGNOSTIC_ERROR),
    (CI_PENDING, Group::DIAGNOSTIC_WARN),
    (KIND_PULL_REQUEST, Group::ADDED),
    (KIND_ISSUE, Group::DIAGNOSTIC_WARN),
    (KIND_RELEASE, Group::DIAGNOSTIC_INFO),
    (KIND_DISCUSSION, Group::SPECIAL),
    (KIND_COMMIT, Group::DIAGNOSTIC_HINT),
    (KIND_CHECK_SUITE, Group::CONSTANT),
    (KIND_OTHER, Group::COMMENT),
    (STATE_OPEN, Group::DIAGNOSTIC_OK),
    (STATE_DRAFT, Group::COMMENT),
    (STATE_MERGED, Group::DIAGNOSTIC_HINT),
    (STATE_CLOSED, Group::DIAGNOSTIC_ERROR),
    (STATE_COMPLETED, STATE_MERGED),
    (STATE_NOT_PLANNED, Group::COMMENT),
    (HEAT_HUMAN, Group::DIFF_ADD),
    (HEAT_EVEN, Group::DIFF_CHANGE),
    (HEAT_LLM, Group::DIFF_DELETE),
    (REFERENCE, AGE),
    (LABELS, REASON),
    (HUNK, AGE),
    (PATH, Group::NORMAL),
];

/// ghd's colours for `tokyonight-moon`, as its theme file had them: the "moon" palette of
/// [folke/tokyonight.nvim](https://github.com/folke/tokyonight.nvim).
const TOKYONIGHT_MOON: &str = "\
hi GhdUnread gui=bold
hi GhdRead guifg=#636da6
hi GhdSelected guifg=#ffc777 guibg=#394b70
hi GhdUnreadMarker guifg=#ff966c
hi GhdRepository guifg=#82aaff
hi GhdTitle guifg=#c8d3f5
hi GhdReason guifg=#c099ff
hi GhdAge guifg=#737aa2
hi GhdMessage gui=italic guifg=#636da6
hi GhdError guifg=#ff757f
hi GhdHint guifg=#545c7e
hi GhdPreviewTitle gui=bold guifg=#c8d3f5
hi GhdCiPassing guifg=#c3e88d
hi GhdCiFailing guifg=#ff757f
hi GhdCiPending guifg=#ffc777
hi GhdKindPullRequest guifg=#c3e88d
hi GhdKindIssue guifg=#ffc777
hi GhdKindRelease guifg=#86e1fc
hi GhdKindDiscussion guifg=#fca7ea
hi GhdKindCommit guifg=#4fd6be
hi GhdKindCheckSuite guifg=#ff966c
hi GhdKindOther guifg=#737aa2
hi GhdStateOpen guifg=#c3e88d
hi GhdStateDraft guifg=#737aa2
hi GhdStateMerged guifg=#c099ff
hi GhdStateClosed guifg=#ff757f
hi GhdStateCompleted guifg=#c099ff
hi GhdStateNotPlanned guifg=#737aa2
hi VoightHeatHuman guifg=#222436 guibg=#c3e88d
hi VoightHeatEven guifg=#222436 guibg=#ffc777
hi VoightHeatLlm guifg=#222436 guibg=#ff757f
";

/// ghd's colours for the ANSI `default` scheme, as its `terminal` theme had them: the terminal's own colours, with
/// emphasis, and the Heatmap stepped between green, yellow and red rather than blended.
const DEFAULT: &str = "\
hi GhdUnread gui=bold
hi GhdRead guifg=darkgray
hi GhdSelected gui=bold
hi GhdUnreadMarker NONE
hi GhdRepository NONE
hi GhdTitle NONE
hi GhdReason NONE
hi GhdAge NONE
hi GhdMessage gui=italic
hi GhdStateDraft guifg=darkgray
hi GhdStateMerged guifg=magenta
hi GhdStateClosed guifg=red
hi GhdStateNotPlanned guifg=darkgray
hi VoightHeatHuman guifg=black guibg=green
hi VoightHeatEven guifg=black guibg=yellow
hi VoightHeatLlm guifg=black guibg=red
";

/// The kit's theme with ghd's groups and colours.
///
/// # Panics
///
/// Never: ghd's colours name ghd's groups, which a test checks.
#[must_use]
pub fn theme() -> Theme {
    let mut theme = Theme::new();
    for (group, link) in GROUPS {
        theme.define(group, Some(link));
    }
    for (scheme, text) in [("tokyonight-moon", TOKYONIGHT_MOON), ("default", DEFAULT)] {
        theme
            .add_overrides(scheme, text)
            .expect("ghd's colours name ghd's groups");
    }
    theme
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    #[test]
    fn moon_has_ghds_own_colours_and_other_schemes_the_links() {
        let mut theme = theme();

        assert_eq!(
            theme.style(REPOSITORY).fg,
            Some(Color::Rgb(0x82, 0xaa, 0xff))
        );
        assert_eq!(theme.style(READ).fg, Some(Color::Rgb(0x63, 0x6d, 0xa6)));
        assert!(
            !theme.style(READ).add_modifier.contains(Modifier::ITALIC),
            "read rows aren't comments"
        );

        theme.colorscheme("tokyonight-night").expect("built in");
        assert_eq!(theme.style(REPOSITORY), theme.style(Group::DIRECTORY));
        assert_eq!(theme.style(STATE_COMPLETED), theme.style(STATE_MERGED));
    }

    #[test]
    fn the_ansi_scheme_steps_the_heatmap() {
        let mut theme = theme();

        theme.colorscheme("default").expect("built in");

        assert_eq!(theme.style(HEAT_LLM).bg, Some(Color::Red));
        assert_eq!(theme.style(HEAT_HUMAN).bg, Some(Color::Green));
    }
}
