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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shell: None,
            languages: vec!["fa".into(), "ar".into()],
            log_level: "warn".into(),
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
