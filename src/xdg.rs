//! XDG base directories, with the spec's fallbacks under `$HOME`.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// `$XDG_STATE_HOME`, or `~/.local/state`.
///
/// # Errors
///
/// Fails when neither variable is set.
pub fn state_home() -> Result<PathBuf> {
    base("XDG_STATE_HOME", ".local/state")
}

/// `$XDG_DATA_HOME`, or `~/.local/share`.
///
/// # Errors
///
/// Fails when neither variable is set.
pub fn data_home() -> Result<PathBuf> {
    base("XDG_DATA_HOME", ".local/share")
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
///
/// # Errors
///
/// Fails when neither variable is set.
pub fn config_home() -> Result<PathBuf> {
    base("XDG_CONFIG_HOME", ".config")
}

fn base(variable: &str, under_home: &str) -> Result<PathBuf> {
    resolve(
        std::env::var_os(variable),
        std::env::var_os("HOME"),
        under_home,
    )
    .with_context(|| format!("neither {variable} nor HOME is set"))
}

/// The directory an XDG variable names, or the fallback under `home`. The spec says to ignore a value
/// that is not absolute, an empty one included.
fn resolve(value: Option<OsString>, home: Option<OsString>, under_home: &str) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|home| PathBuf::from(home).join(under_home)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_or_empty_value_falls_back_under_home() {
        let home = || Some(OsString::from("/home/otter"));
        let state = |value: &str| resolve(Some(value.into()), home(), ".local/state");
        assert_eq!(state("/scratch/state"), Some("/scratch/state".into()));
        assert_eq!(state(""), Some("/home/otter/.local/state".into()));
        assert_eq!(state("state"), Some("/home/otter/.local/state".into()));
        assert_eq!(resolve(None, None, ".local/state"), None);
    }
}
