//! `~/.config/dosu/config.toml`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Command to run inside the pty. Defaults to `$SHELL`.
    pub shell: Option<String>,
    /// Languages this session should be aware of for bidi purposes.
    /// Currently informational; the bidi engine is script-driven
    /// (UAX #9), not locale-driven.
    pub languages: Vec<String>,
    /// Log level for tracing output (off, error, warn, info, debug, trace).
    /// Passed to `tracing_subscriber::EnvFilter` by consumers.
    pub log_level: String,
    /// Whether `dosu` should periodically check GitHub Releases for a
    /// newer version and print a notice on startup (oh-my-zsh style).
    /// The check is always non-blocking and always skipped if
    /// `DOSU_DISABLE_UPDATE_CHECK` is set, regardless of this field.
    pub update_check_enabled: bool,
    /// How many days to wait between update checks. A check that ran
    /// within this window is skipped entirely (no network call); the
    /// cached last-known version is still compared and the notice still
    /// shown if it's newer than the running binary.
    pub update_check_interval_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shell: None,
            languages: vec!["fa".into(), "ar".into()],
            log_level: "warn".into(),
            update_check_enabled: true,
            update_check_interval_days: 7,
        }
    }
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        dirs_next_config().map(|p| p.join("dosu").join("config.toml"))
    }

    pub fn load() -> Self {
        Self::config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
}

/// Minimal `$XDG_CONFIG_HOME` / `~/.config` resolution without pulling in
/// an extra crate.
fn dirs_next_config() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config"))
}
