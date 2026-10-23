//! Themes: how each part of the UI looks, one file per theme.
//!
//! A theme file has a `[palette]` of named colors, and `[styles]` for the parts of the UI, each with any of `fg`,
//! `bg` (a palette name, or a color like `#82aaff` or `blue`), `bold`, `dim`, `italic`, `underlined` and
//! `reversed`. A part the file leaves out gets no style at all: the terminal's own colors.
//!
//! ```toml
//! [palette]
//! bg = "#222436"
//! blue = "#82aaff"
//!
//! [styles]
//! text = { bg = "bg" }
//! tab_current = { fg = "bg", bg = "blue", bold = true }
//!
//! [styles.kind]
//! pull_request = { fg = "blue" }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use crate::inbox::SubjectKind;
use crate::state::SubjectState;
use crate::syntax::Class;

/// The theme used unless the configuration names another.
pub const DEFAULT: &str = "tokyonight-moon";

/// The themes built into ghd, by name.
const BUILT_IN: [(&str, &str); 2] = [
    (
        "tokyonight-moon",
        include_str!("../themes/tokyonight-moon.toml"),
    ),
    ("terminal", include_str!("../themes/terminal.toml")),
];

/// Declares [`Theme`], one style per part of the UI, with the name each has in a theme file.
macro_rules! theme {
    ($($(#[$doc:meta])* $field:ident = $key:literal,)+) => {
        /// How each part of the UI looks. The default theme styles nothing.
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct Theme {
            $($(#[$doc])* pub $field: Style,)+
        }

        impl Theme {
            /// Every style's name in a theme file, `[styles.kind]` ones as `kind.…`.
            #[cfg(test)]
            const STYLE_NAMES: &[&str] = &[$($key,)+];

            /// The style called `name` in a theme file; `None` if there's no such style.
            fn style_mut(&mut self, name: &str) -> Option<&mut Style> {
                match name {
                    $($key => Some(&mut self.$field),)+
                    _ => None,
                }
            }
        }
    };
}

theme! {
    /// Everything not styled otherwise. Its background, if any, paints the whole screen.
    text = "text",
    border = "border",
    /// The thumb of the scrollbar on a pane's right border, while its text is longer than the pane.
    scrollbar = "scrollbar",
    /// A View's tab, other than the one on screen.
    tab = "tab",
    tab_current = "tab_current",
    /// The cursor's row, over the rest of its style.
    cursor = "cursor",
    /// A selected Notification's row, over its other styles but the cursor's, and the `▌` marking it; also how many
    /// are selected, in the bottom border.
    selected = "selected",
    /// An unread Notification's row, over its columns' styles.
    unread = "unread",
    /// A read Notification's row, over its columns' styles.
    read = "read",
    /// The `●` before an unread Notification; also "● unread only" in the bottom border.
    unread_marker = "unread_marker",
    kind_pull_request = "kind.pull_request",
    kind_issue = "kind.issue",
    kind_release = "kind.release",
    kind_discussion = "kind.discussion",
    kind_commit = "kind.commit",
    kind_check_suite = "kind.check_suite",
    /// A kind of Subject ghd doesn't know.
    kind_other = "kind.other",
    /// The kind column of a pull request or an issue in that State, over the kind's own style.
    state_open = "state.open",
    state_draft = "state.draft",
    state_merged = "state.merged",
    state_closed = "state.closed",
    state_completed = "state.completed",
    state_not_planned = "state.not_planned",
    /// A State's color on a read Notification's row, over it: to tone it down, rather than lose it to `read`.
    read_state = "read_state",
    repository = "repository",
    title = "title",
    reason = "reason",
    age = "age",
    /// What's said in place of the list: fetching, or nothing to show.
    message = "message",
    /// Errors, on screen and in the status line.
    error = "error",
    /// What the latest action did, in the status line.
    status = "status",
    /// The Search being typed.
    prompt = "prompt",
    /// The ways to complete the word being typed, other than the current one.
    completion = "completion",
    completion_current = "completion_current",
    /// The Search, in the bottom border; and what's looked for in a file, with how many times it's found.
    search = "search",
    /// Where what's looked for in a file is found, over the style of the text there.
    found = "found",
    /// … the place jumped to.
    found_current = "found_current",
    /// "fetching…", in the top border.
    fetching = "fetching",
    /// "? keys", in the bottom border.
    hint = "hint",
    help_keys = "help_keys",
    /// The help's heading over the search prompt's keys.
    help_heading = "help_heading",
    /// Headings in a Preview's Markdown, and tables' header rows.
    markdown_heading = "markdown.heading",
    /// Code in a Preview's Markdown, inline and in blocks.
    markdown_code = "markdown.code",
    /// Links in a Preview's Markdown: their text, and where they go.
    markdown_link = "markdown.link",
    /// Quotes and alerts (`> [!NOTE]`) in a Preview's Markdown.
    markdown_quote = "markdown.quote",
    /// What lays out a Preview's Markdown: list markers, table borders, heading markers, images' descriptions,
    /// footnotes.
    markdown_muted = "markdown.muted",
    /// Highlighted code in a Preview's Markdown, over `markdown.code`: comments.
    syntax_comment = "syntax.comment",
    /// … keywords, `self` and such.
    syntax_keyword = "syntax.keyword",
    syntax_string = "syntax.string",
    /// … numbers, `true`, escapes in strings, and such.
    syntax_constant = "syntax.constant",
    syntax_function = "syntax.function",
    /// … types, and keys in JSON, YAML and such.
    syntax_type = "syntax.type",
    syntax_operator = "syntax.operator",
    /// Lines added: how many, in a pull request's Preview; the `+` of each in a file's diff, and the `▎` in the gutter
    /// of a file shown whole; in a diff in Markdown. And on the Files screen, an added file's letter.
    diff_added = "diff.added",
    /// Lines removed: how many, in a pull request's Preview; the `-` of each in a file's diff, and the `▁` or `▔` next to
    /// them in the gutter of a file shown whole; in a diff in Markdown. And on the Files screen, a removed file's
    /// letter.
    diff_removed = "diff.removed",
    /// A line added, in a file's diff, under its highlighted code: a background, say.
    diff_added_line = "diff.added_line",
    /// A line removed, in a file's diff, under its highlighted code.
    diff_removed_line = "diff.removed_line",
    /// On the Files screen, a modified, renamed or copied file's letter; and the `▎` in the gutter of a file shown
    /// whole for a line that took the place of one removed.
    diff_changed = "diff.changed",
    /// A pull request's checks, in its Preview: all passed.
    ci_passing = "ci.passing",
    /// Some failed.
    ci_failing = "ci.failing",
    /// Some haven't finished, none failed.
    ci_pending = "ci.pending",
    /// voight's Assessment, at the end of the Preview's bottom border.
    voight = "voight",
    /// In the Heatmap, text in a window voight scored 0%; see [`Theme::heat`].
    heatmap_human = "heatmap.human",
    /// … scored 50%.
    heatmap_even = "heatmap.even",
    /// … scored 100%.
    heatmap_llm = "heatmap.llm",
}

impl Theme {
    /// The theme called `name`: `themes/<name>.toml` in `config_dir` if there's one, or else the built-in theme of
    /// that name.
    ///
    /// # Errors
    ///
    /// Fails if `name` isn't a theme name (letters, digits, `-` and `_`), if there's no theme of that name, or if
    /// its file can't be read or isn't a valid theme. The error names the file, and the style or color at fault.
    pub async fn load(name: &str, config_dir: Option<&Path>) -> anyhow::Result<Self> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("\"{name}\" isn't a theme name: use letters, digits, `-` and `_`");
        }
        let path = config_dir.map(|dir| dir.join("themes").join(format!("{name}.toml")));
        if let Some(path) = &path {
            match tokio::fs::read_to_string(path).await {
                Ok(text) => {
                    return Self::parse(&text)
                        .with_context(|| format!("invalid theme in {}", path.display()));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("reading {}", path.display()));
                }
            }
        }
        if let Some((_, text)) = BUILT_IN.iter().find(|(built_in, _)| *built_in == name) {
            Self::parse(text).with_context(|| format!("invalid built-in theme {name}"))
        } else {
            let built_in: Vec<&str> = BUILT_IN.iter().map(|(name, _)| *name).collect();
            let file = path.map_or_else(String::new, |path| {
                format!("there's no {}, and ", path.display())
            });
            bail!(
                "no theme named \"{name}\": {file}ghd has no such built-in theme (it has {})",
                built_in.join(", ")
            )
        }
    }

    /// The style for Subjects of `kind`.
    pub fn kind(&self, kind: &SubjectKind) -> Style {
        match kind {
            SubjectKind::PullRequest => self.kind_pull_request,
            SubjectKind::Issue => self.kind_issue,
            SubjectKind::Release => self.kind_release,
            SubjectKind::Discussion => self.kind_discussion,
            SubjectKind::Commit => self.kind_commit,
            SubjectKind::CheckSuite => self.kind_check_suite,
            SubjectKind::Other(_) => self.kind_other,
        }
    }

    /// The style for Subjects in `state`, over their kind's.
    pub fn state(&self, state: SubjectState) -> Style {
        match state {
            SubjectState::Open => self.state_open,
            SubjectState::Draft => self.state_draft,
            SubjectState::Merged => self.state_merged,
            SubjectState::Closed => self.state_closed,
            SubjectState::Completed => self.state_completed,
            SubjectState::NotPlanned => self.state_not_planned,
        }
    }

    /// The style for highlighted code of `class`, over `markdown.code`.
    pub fn syntax(&self, class: Class) -> Style {
        match class {
            Class::Comment => self.syntax_comment,
            Class::Keyword => self.syntax_keyword,
            Class::String => self.syntax_string,
            Class::Constant => self.syntax_constant,
            Class::Function => self.syntax_function,
            Class::Type => self.syntax_type,
            Class::Operator => self.syntax_operator,
            Class::Inserted => self.diff_added,
            Class::Deleted => self.diff_removed,
        }
    }

    /// The style of text in the Heatmap that voight scored `share`, between 0 and 1: `heatmap.human` at 0,
    /// `heatmap.even` at 0.5 and `heatmap.llm` at 1. In between, the nearer one's, with a background blended from
    /// both's when they're RGB colors.
    pub fn heat(&self, share: f64) -> Style {
        let share = share.clamp(0.0, 1.0);
        let (from, to, toward) = if share < 0.5 {
            (self.heatmap_human, self.heatmap_even, share * 2.0)
        } else {
            (self.heatmap_even, self.heatmap_llm, share * 2.0 - 1.0)
        };
        let nearer = if toward < 0.5 { from } else { to };
        let (Some(Color::Rgb(r0, g0, b0)), Some(Color::Rgb(r1, g1, b1))) = (from.bg, to.bg) else {
            return nearer;
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rounded, between two `u8`s"
        )]
        let blend =
            |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * toward).round() as u8;
        nearer.bg(Color::Rgb(blend(r0, r1), blend(g0, g1), blend(b0, b1)))
    }

    /// The theme that the TOML `text` describes.
    fn parse(text: &str) -> anyhow::Result<Self> {
        let file: File = toml::from_str(text)?;
        let palette = file
            .palette
            .into_iter()
            .map(|(name, value)| match value.parse::<Color>() {
                Ok(color) => Ok((name, color)),
                Err(_) => Err(anyhow!("palette color `{name}` isn't a color: \"{value}\"")),
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let mut theme = Self::default();
        theme.set_styles(None, file.styles, &palette)?;
        Ok(theme)
    }

    /// Sets the styles in `table`, a theme file's `[styles]` or a table in it (`[styles.kind]`, its names prefixed
    /// with `group`), resolving their colors with `palette`.
    fn set_styles(
        &mut self,
        group: Option<&str>,
        table: toml::Table,
        palette: &HashMap<String, Color>,
    ) -> anyhow::Result<()> {
        for (key, value) in table {
            let name = group.map_or_else(|| key.clone(), |group| format!("{group}.{key}"));
            match value {
                // A table of tables is a group of styles; a style's own values are strings and booleans.
                toml::Value::Table(table)
                    if !table.is_empty() && table.values().all(toml::Value::is_table) =>
                {
                    self.set_styles(Some(&name), table, palette)?;
                }
                value => {
                    let entry: StyleEntry = value
                        .try_into()
                        .with_context(|| format!("style `{name}`"))?;
                    let style = entry
                        .resolve(palette)
                        .with_context(|| format!("style `{name}`"))?;
                    *self
                        .style_mut(&name)
                        .ok_or_else(|| anyhow!("unknown style `{name}`"))? = style;
                }
            }
        }
        Ok(())
    }
}

/// A theme file, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    palette: BTreeMap<String, String>,
    #[serde(default)]
    styles: toml::Table,
}

/// A style, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per key a theme file can write, and the modifiers are independent of each other: they are \
              not a state to model as an enum"
)]
struct StyleEntry {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    bold: bool,
    #[serde(default)]
    dim: bool,
    #[serde(default)]
    italic: bool,
    #[serde(default)]
    underlined: bool,
    #[serde(default)]
    reversed: bool,
}

impl StyleEntry {
    /// The style, its colors looked up in `palette` first.
    fn resolve(self, palette: &HashMap<String, Color>) -> anyhow::Result<Style> {
        let color = |name: Option<String>| -> anyhow::Result<Option<Color>> {
            let Some(name) = name else {
                return Ok(None);
            };
            if let Some(&color) = palette.get(&name) {
                return Ok(Some(color));
            }
            name.parse().map(Some).map_err(|_| {
                anyhow!("`{name}` isn't in the palette, nor a color like `#82aaff` or `blue`")
            })
        };
        let mut style = Style::new();
        if let Some(fg) = color(self.fg)? {
            style = style.fg(fg);
        }
        if let Some(bg) = color(self.bg)? {
            style = style.bg(bg);
        }
        for (wanted, modifier) in [
            (self.bold, Modifier::BOLD),
            (self.dim, Modifier::DIM),
            (self.italic, Modifier::ITALIC),
            (self.underlined, Modifier::UNDERLINED),
            (self.reversed, Modifier::REVERSED),
        ] {
            if wanted {
                style = style.add_modifier(modifier);
            }
        }
        Ok(style)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn parse(text: &str) -> Theme {
        Theme::parse(text).unwrap_or_else(|error| panic!("{text:?} should parse: {error:#}"))
    }

    fn rejection(text: &str) -> String {
        match Theme::parse(text) {
            Ok(theme) => panic!("{text:?} should be rejected, got {theme:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// A directory of its own under the system's temporary one, removed when dropped.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ghd-{name}-{}", std::process::id()));
            std::fs::create_dir_all(path.join("themes")).expect("temporary directory is writable");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            // Best effort: a leftover temporary directory is harmless.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn styles_take_palette_names_or_colors() {
        let theme = parse(
            r##"
            [palette]
            bg = "#222436"
            blue = "#82aaff"

            [styles]
            text = { fg = "white", bg = "bg" }
            tab_current = { fg = "bg", bg = "blue", bold = true, underlined = true }
            read = { fg = "#636da6", dim = true, italic = true }
            cursor = { reversed = true }

            [styles.kind]
            pull_request = { fg = "12" }
            "##,
        );

        assert_eq!(
            theme.text,
            Style::new()
                .fg(Color::White)
                .bg(Color::Rgb(0x22, 0x24, 0x36))
        );
        assert_eq!(
            theme.tab_current,
            Style::new()
                .fg(Color::Rgb(0x22, 0x24, 0x36))
                .bg(Color::Rgb(0x82, 0xaa, 0xff))
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(
            theme.read,
            Style::new()
                .fg(Color::Rgb(0x63, 0x6d, 0xa6))
                .add_modifier(Modifier::DIM | Modifier::ITALIC)
        );
        assert_eq!(theme.cursor, Style::new().add_modifier(Modifier::REVERSED));
        assert_eq!(
            theme.kind(&SubjectKind::PullRequest),
            Style::new().fg(Color::Indexed(12))
        );
    }

    #[test]
    fn left_out_styles_have_no_style() {
        let theme = parse("[styles]\ntext = { fg = \"white\" }");

        assert_eq!(theme.border, Style::default());
        assert_eq!(theme.kind(&SubjectKind::Issue), Style::default());
        assert_eq!(parse(""), Theme::default());
    }

    #[test]
    fn mistakes_are_named() {
        let cases = [
            (
                "[styles]\nbordr = { fg = \"red\" }",
                "unknown style `bordr`",
            ),
            (
                "[styles.kind]\nissues = { fg = \"red\" }",
                "unknown style `kind.issues`",
            ),
            (
                "[styles]\nborder = { fg = \"blu\" }",
                "style `border`: `blu` isn't in the palette",
            ),
            ("[styles]\nborder = { colour = \"red\" }", "style `border`"),
            (
                "[palette]\nblue = \"#82aaf\"",
                "palette color `blue` isn't a color",
            ),
            ("[colors]\nblue = \"#82aaff\"", "unknown field `colors`"),
        ];
        for (text, expected) in cases {
            let message = rejection(text);
            assert!(message.contains(expected), "{text:?}: {message}");
        }
    }

    #[test]
    fn tokyonight_moon_styles_every_part() {
        let theme = parse(BUILT_IN[0].1);
        let mut unstyled = Vec::new();
        for name in Theme::STYLE_NAMES {
            let mut copy = theme;
            if *copy.style_mut(name).expect("a known style") == Style::default() {
                unstyled.push(*name);
            }
        }

        assert!(
            unstyled.is_empty(),
            "tokyonight-moon leaves out {unstyled:?}"
        );
    }

    #[test]
    fn heat_blends_rgb_backgrounds_and_steps_between_others() {
        let blended = parse(
            r##"
            [styles.heatmap]
            human = { fg = "black", bg = "#0ac80a" }
            even = { fg = "black", bg = "#c8c80a" }
            llm = { fg = "white", bg = "#c80a0a", bold = true }
            "##,
        );
        assert_eq!(blended.heat(0.0), blended.heatmap_human);
        assert_eq!(blended.heat(0.5), blended.heatmap_even);
        assert_eq!(blended.heat(1.0), blended.heatmap_llm);
        assert_eq!(blended.heat(-1.0), blended.heatmap_human, "clamped");
        assert_eq!(blended.heat(2.0), blended.heatmap_llm, "clamped");
        assert_eq!(
            blended.heat(0.2),
            Style::new().fg(Color::Black).bg(Color::Rgb(86, 200, 10)),
            "40% of the way to yellow"
        );
        assert_eq!(
            blended.heat(0.8),
            Style::new()
                .fg(Color::White)
                .bg(Color::Rgb(200, 86, 10))
                .add_modifier(Modifier::BOLD),
            "nearer red: its foreground"
        );

        let stepped = parse(
            r#"
            [styles.heatmap]
            human = { bg = "green" }
            even = { bg = "yellow" }
            llm = { bg = "red" }
            "#,
        );
        assert_eq!(stepped.heat(0.24), Style::new().bg(Color::Green));
        assert_eq!(stepped.heat(0.26), Style::new().bg(Color::Yellow));
        assert_eq!(stepped.heat(0.74), Style::new().bg(Color::Yellow));
        assert_eq!(stepped.heat(0.76), Style::new().bg(Color::Red));
    }

    #[test]
    fn every_built_in_theme_parses() {
        for (name, text) in BUILT_IN {
            Theme::parse(text).unwrap_or_else(|error| panic!("{name}: {error:#}"));
        }
    }

    #[tokio::test]
    async fn default_theme_is_built_in() {
        let theme = Theme::load(DEFAULT, None).await.expect("built in");

        assert_eq!(theme.text.fg, Some(Color::Rgb(0xc8, 0xd3, 0xf5)));
        assert_eq!(theme.text.bg, None, "the terminal's background");
    }

    #[tokio::test]
    async fn theme_file_comes_before_the_built_in_one() {
        let dir = ScratchDir::new("theme-file");
        std::fs::write(
            dir.0.join("themes/tokyonight-moon.toml"),
            "[styles]\ntext = { fg = \"red\" }",
        )
        .expect("temporary directory is writable");
        std::fs::write(
            dir.0.join("themes/mine.toml"),
            "[styles]\ntext = { fg = \"green\" }",
        )
        .expect("temporary directory is writable");

        let replaced = Theme::load("tokyonight-moon", Some(&dir.0))
            .await
            .expect("valid theme");
        let mine = Theme::load("mine", Some(&dir.0))
            .await
            .expect("valid theme");
        let built_in = Theme::load("terminal", Some(&dir.0))
            .await
            .expect("built in");

        assert_eq!(replaced.text, Style::new().fg(Color::Red));
        assert_eq!(mine.text, Style::new().fg(Color::Green));
        assert_eq!(built_in, parse(BUILT_IN[1].1));
    }

    #[tokio::test]
    async fn unknown_or_odd_theme_names_are_rejected() {
        let dir = ScratchDir::new("theme-names");
        std::fs::write(dir.0.join("themes/broken.toml"), "[styles]\nbordr = {}")
            .expect("temporary directory is writable");

        let missing = Theme::load("solarized", Some(&dir.0))
            .await
            .expect_err("no such theme");
        assert!(
            format!("{missing:#}").contains("no theme named \"solarized\""),
            "{missing:#}"
        );
        assert!(
            format!("{missing:#}").contains("tokyonight-moon, terminal"),
            "{missing:#}"
        );

        let broken = Theme::load("broken", Some(&dir.0))
            .await
            .expect_err("invalid theme");
        assert!(
            format!("{broken:#}").contains("broken.toml: unknown style `bordr`"),
            "{broken:#}"
        );

        // A valid theme file outside `themes/`, which a name like `../outside` would reach.
        std::fs::write(
            dir.0.join("outside.toml"),
            "[styles]\ntext = { fg = \"red\" }",
        )
        .expect("temporary directory is writable");
        for name in ["", "../outside", "themes/../../outside", "tokyo night"] {
            let error = Theme::load(name, Some(&dir.0))
                .await
                .expect_err("not a theme name");
            assert!(
                format!("{error:#}").contains("isn't a theme name"),
                "{name:?}: {error:#}"
            );
        }
    }
}
