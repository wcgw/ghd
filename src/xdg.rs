//! Where ghd keeps its files, after the XDG base directory specification.

use std::env;
use std::path::PathBuf;

/// The directory of ghd's configuration file and themes: `$XDG_CONFIG_HOME/ghd`, or `~/.config/ghd` when
/// `XDG_CONFIG_HOME` isn't set; `None` when neither it nor `HOME` is.
pub fn config_dir() -> Option<PathBuf> {
    ghd_dir("XDG_CONFIG_HOME", ".config")
}

/// The directory of what ghd keeps between runs: `$XDG_CACHE_HOME/ghd`, or `~/.cache/ghd` when `XDG_CACHE_HOME`
/// isn't set; `None` when neither it nor `HOME` is.
pub fn cache_dir() -> Option<PathBuf> {
    ghd_dir("XDG_CACHE_HOME", ".cache")
}

/// `ghd` under the directory in `variable`, or under `under_home` in the home directory when it isn't set.
fn ghd_dir(variable: &str, under_home: &str) -> Option<PathBuf> {
    let set = |name| {
        env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let base = set(variable).or_else(|| Some(set("HOME")?.join(under_home)))?;
    Some(base.join("ghd"))
}
