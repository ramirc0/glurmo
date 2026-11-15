//! XDG base directories, with the spec's fallbacks under `$HOME`.

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
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(under_home)))
        .with_context(|| format!("neither {variable} nor HOME is set"))
}
