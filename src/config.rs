//! The configuration file: the Account to act as, the theme, and the Views.
//!
//! ```toml
//! host = "github.example.com"   # optional, github.com by default
//! user = "alexsnaps"            # optional, `gh`'s active account on the host by default
//! theme = "terminal"            # optional, tokyonight-moon by default
//! icons = true                  # optional, false by default: Nerd Font icons for kinds of Subject
//! voight = "metal"              # optional, "cpu" by default: where voight assesses Previews ("cpu", "metal" or
//!                               # "cuda"; a GPU needs ghd built with `--features cuda` or `metal`), or "off"
//!
//! [[view]]
//! name = "Reviews"
//! filter = "reason:review-requested reason:team-mention -is:read"
//! ```

use std::io::ErrorKind;

use anyhow::{Context as _, bail};
use serde::Deserialize;

use crate::assessment::Device;
use crate::filter::Filter;
use crate::github::Account;
use crate::{theme, xdg};

/// The built-in View's name.
const INBOX_VIEW: &str = "Inbox";

/// A named tab showing the Inbox Notifications that match its Filter.
#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    pub filter: Filter,
}

/// What the configuration file asks for.
#[derive(Debug)]
pub struct Config {
    account: Account,
    /// The name of the theme to use.
    theme: String,
    /// Kinds of Subject show as Nerd Font icons rather than text.
    icons: bool,
    /// Where voight runs to assess Previews; `None` when it's off.
    voight: Option<Device>,
    /// The built-in Inbox View, then the configured ones in the file's order; names are unique, ignoring case.
    views: Vec<View>,
}

impl Config {
    /// Reads the configuration file: `$XDG_CONFIG_HOME/ghd/config.toml`, or `~/.config/ghd/config.toml` when
    /// `XDG_CONFIG_HOME` isn't set. A missing file is the same as an empty one.
    ///
    /// # Errors
    ///
    /// Fails if the file exists but can't be read, or isn't a valid configuration. The error names the file, and
    /// what's wrong in it: the host or user, or the View and the part of its filter.
    pub async fn load() -> anyhow::Result<Self> {
        let Some(path) = xdg::config_dir().map(|dir| dir.join("config.toml")) else {
            return Ok(Self::default());
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => Self::parse(&text)
                .with_context(|| format!("invalid configuration in {}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The Account to act as.
    pub fn account(&self) -> &Account {
        &self.account
    }

    /// The name of the theme to use.
    pub fn theme(&self) -> &str {
        &self.theme
    }

    /// Whether kinds of Subject show as Nerd Font icons rather than text. There's no telling from inside a
    /// terminal which font it draws with, so it's the user's call.
    pub fn icons(&self) -> bool {
        self.icons
    }

    /// Where voight runs to assess Previews; `None` when it's off. On the CPU unless the file says otherwise, if
    /// ghd was built with voight.
    pub fn voight(&self) -> Option<Device> {
        self.voight
    }

    /// The Views to show as tabs: the built-in Inbox View first, then the configured ones in the file's order.
    pub fn into_views(self) -> Vec<View> {
        self.views
    }

    /// The configuration that the TOML `text` describes.
    fn parse(text: &str) -> anyhow::Result<Self> {
        let file: File = toml::from_str(text)?;
        let mut config = Self {
            account: Account::new(file.host.as_deref(), file.user.as_deref())?,
            theme: file.theme.unwrap_or_else(|| theme::DEFAULT.to_owned()),
            icons: file.icons.unwrap_or_default(),
            voight: match file.voight.map(VoightSetting::device) {
                None => Self::default().voight,
                Some(None) => None,
                Some(Some(device)) if !cfg!(feature = "voight") => bail!(
                    "voight = \"{device}\", but this ghd was built without voight: set voight = \"off\", or \
                     build ghd with its `voight` feature"
                ),
                Some(Some(device)) if !device.is_built_in() => bail!(
                    "voight = \"{device}\", but this ghd was built without {device} support: set voight = \
                     \"cpu\", or build ghd with `--features {device}`"
                ),
                Some(Some(device)) => Some(device),
            },
            ..Self::default()
        };
        for entry in file.views {
            if entry.name.trim().is_empty() {
                bail!("a View needs a name");
            }
            if entry.name.eq_ignore_ascii_case(INBOX_VIEW) {
                bail!(
                    "the {INBOX_VIEW} View is built in and can't be redefined; give View \"{}\" another name",
                    entry.name
                );
            }
            if config
                .views
                .iter()
                .any(|view| view.name.eq_ignore_ascii_case(&entry.name))
            {
                bail!("there are two Views named \"{}\"", entry.name);
            }
            let filter = entry
                .filter
                .parse()
                .with_context(|| format!("View \"{}\"", entry.name))?;
            config.views.push(View {
                name: entry.name,
                filter,
            });
        }
        Ok(config)
    }
}

/// `gh`'s active account on github.com, the default theme, text for kinds of Subject, and only the built-in Inbox
/// View.
impl Default for Config {
    fn default() -> Self {
        Self {
            account: Account::default(),
            theme: theme::DEFAULT.to_owned(),
            icons: false,
            voight: cfg!(feature = "voight").then_some(Device::Cpu),
            views: vec![View {
                name: INBOX_VIEW.to_owned(),
                filter: Filter::default(),
            }],
        }
    }
}

/// The configuration file, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    host: Option<String>,
    user: Option<String>,
    theme: Option<String>,
    icons: Option<bool>,
    voight: Option<VoightSetting>,
    #[serde(default, rename = "view")]
    views: Vec<ViewEntry>,
}

/// The `voight` setting, as written: `"off"`, or the device to run on.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum VoightSetting {
    Off,
    Cpu,
    Metal,
    Cuda,
}

impl VoightSetting {
    /// The device voight runs on; `None` when it's off.
    fn device(self) -> Option<Device> {
        match self {
            Self::Off => None,
            Self::Cpu => Some(Device::Cpu),
            Self::Metal => Some(Device::Metal),
            Self::Cuda => Some(Device::Cuda),
        }
    }
}

/// A `[[view]]` table, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewEntry {
    name: String,
    filter: String,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::inbox::{ApiPaths, Notification, Reason, SubjectKind, ThreadId};

    fn names(config: &Config) -> Vec<&str> {
        config.views.iter().map(|view| view.name.as_str()).collect()
    }

    fn rejection(text: &str) -> String {
        match Config::parse(text) {
            Ok(config) => panic!("{text:?} should be rejected, got {:?}", names(&config)),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn account_is_github_com_unless_the_file_says() {
        assert_eq!(Config::default().account, Account::default());
        assert_eq!(
            Config::parse("").expect("valid").account,
            Account::default()
        );

        let config =
            Config::parse("host = \"github.example.com\"\nuser = \"alexsnaps\"").expect("valid");
        assert_eq!(
            config.account,
            Account::new(Some("github.example.com"), Some("alexsnaps")).expect("valid account")
        );
    }

    #[test]
    fn theme_is_tokyonight_moon_unless_the_file_says() {
        assert_eq!(Config::default().theme(), "tokyonight-moon");
        assert_eq!(Config::parse("").expect("valid").theme(), "tokyonight-moon");
        assert_eq!(
            Config::parse("theme = \"terminal\"")
                .expect("valid")
                .theme(),
            "terminal"
        );
    }

    #[test]
    fn icons_are_off_unless_the_file_says() {
        assert!(!Config::default().icons());
        assert!(!Config::parse("").expect("valid").icons());
        assert!(Config::parse("icons = true").expect("valid").icons());
        assert!(rejection("icons = \"yes\"").contains("icons"));
    }

    #[cfg(feature = "voight")]
    #[test]
    fn voight_runs_on_the_cpu_unless_the_file_says() {
        let voight = |text| Config::parse(text).expect("valid").voight();
        assert_eq!(Config::default().voight(), Some(Device::Cpu));
        assert_eq!(voight(""), Some(Device::Cpu));
        assert_eq!(voight("voight = \"off\""), None);
        if Device::Metal.is_built_in() {
            assert_eq!(voight("voight = \"metal\""), Some(Device::Metal));
        }
        if Device::Cuda.is_built_in() {
            assert_eq!(voight("voight = \"cuda\""), Some(Device::Cuda));
        }
        assert!(rejection("voight = \"gpu\"").contains("voight"));
        assert!(rejection("voight = true").contains("voight"));
    }

    #[cfg(all(feature = "voight", not(feature = "cuda")))]
    #[test]
    fn cuda_needs_building_in() {
        assert!(rejection("voight = \"cuda\"").contains("--features cuda"));
    }

    #[cfg(not(feature = "voight"))]
    #[test]
    fn voight_is_off_without_it_built_in() {
        assert_eq!(Config::default().voight(), None);
        assert_eq!(
            Config::parse("voight = \"off\"").expect("valid").voight(),
            None
        );
        assert!(rejection("voight = \"cpu\"").contains("built without voight"));
    }

    #[test]
    fn bad_host_or_user_is_rejected() {
        assert!(rejection("host = \"https://github.example.com\"").contains("hostname"));
        assert!(rejection("host = \"octocorp.ghe.com\"").contains("aren't supported yet"));
        assert!(rejection("user = \"\"").contains("should be a GitHub login"));
        assert!(rejection("user = \"../alexsnaps\"").contains("should be a GitHub login"));
    }

    #[test]
    fn empty_file_has_only_the_inbox_view() {
        let config = Config::parse("").expect("empty file is valid");

        assert_eq!(names(&config), ["Inbox"]);
        assert_eq!(names(&Config::default()), ["Inbox"]);
    }

    #[test]
    fn views_follow_the_inbox_view_in_file_order() {
        let config = Config::parse(
            r#"
            [[view]]
            name = "Reviews"
            filter = "reason:review-requested"

            [[view]]
            name = "ghd"
            filter = "repo:wcgw/ghd"
            "#,
        )
        .expect("valid configuration");

        assert_eq!(names(&config), ["Inbox", "Reviews", "ghd"]);
        let review = Notification {
            thread: ThreadId(1),
            subject_url: "https://github.com/kuadrant/limitador/pull/1".to_owned(),
            repository: "kuadrant/limitador".to_owned(),
            subject_kind: SubjectKind::PullRequest,
            subject_title: "A pull request".to_owned(),
            reason: Reason::ReviewRequested,
            unread: true,
            updated_at: Utc::now(),
            api_paths: ApiPaths::default(),
        };
        let matching: Vec<bool> = config
            .views
            .iter()
            .map(|view| view.filter.matches(&review, None))
            .collect();
        assert_eq!(matching, [true, true, false]);
    }

    #[test]
    fn inbox_view_cannot_be_redefined() {
        let message = rejection(
            r#"
            [[view]]
            name = "inbox"
            filter = "is:unread"
            "#,
        );

        assert!(message.contains("built in"), "{message}");
    }

    #[test]
    fn view_names_are_unique_ignoring_case() {
        let message = rejection(
            r#"
            [[view]]
            name = "Reviews"
            filter = "reason:review-requested"

            [[view]]
            name = "reviews"
            filter = "reason:team-mention"
            "#,
        );

        assert!(
            message.contains(r#"two Views named "reviews""#),
            "{message}"
        );
    }

    #[test]
    fn bad_filter_names_the_view_and_points_at_the_culprit() {
        let message = rejection(
            r#"
            [[view]]
            name = "Reviews"
            filter = "org:wcgw reason:review-requestd"
            "#,
        );

        assert!(
            message.starts_with(r#"View "Reviews": unknown reason `review-requestd`"#),
            "{message}"
        );
        assert!(message.ends_with("^^^^^^^^^^^^^^^"), "{message}");
    }

    #[test]
    fn misspelled_or_missing_keys_are_rejected() {
        for text in [
            "[[view]]\nname = \"Reviews\"\nfiltr = \"is:unread\"",
            "[[view]]\nname = \"Reviews\"",
            "[[views]]\nname = \"Reviews\"\nfilter = \"is:unread\"",
            "[[view]]\nname = \" \"\nfilter = \"is:unread\"",
        ] {
            rejection(text);
        }
    }
}
