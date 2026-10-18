//! `config.toml` settings, default directories and value parsers shared by the CLI.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Environment variable overriding the data directory.
pub const ENV_DATA_DIR: &str = "IGDL_DATA_DIR";

/// Settings read from `config.toml`. Every field is optional; CLI flags win.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub out_dir: Option<PathBuf>,
    pub cookies: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub template: Option<String>,
    pub retries: Option<u32>,
    /// Seconds between API requests.
    pub api_delay: Option<f64>,
    /// e.g. `"2M"`; see [`parse_rate`].
    pub limit_rate: Option<String>,
    pub user_agent: Option<String>,
    /// Profile sections, e.g. `["posts", "stories"]`.
    pub include: Option<Vec<String>>,
    /// Set file modification times to the post date (default true).
    pub set_mtime: Option<bool>,
}

/// `<config dir>/config.toml` (`%APPDATA%\igdl\config\config.toml`, `~/.config/igdl/config.toml`).
pub fn default_config_path() -> Option<PathBuf> {
    todo!("cookies agent")
}

/// Data directory: `$IGDL_DATA_DIR`, else the platform data dir
/// (`%APPDATA%\igdl\data`, `~/.local/share/igdl`).
pub fn default_data_dir() -> Result<PathBuf> {
    todo!("cookies agent")
}

/// Loads settings. An explicit path must exist; the default path may be absent
/// (→ `Settings::default()`). Unknown keys are a [`crate::error::Error::Config`].
pub fn load(explicit: Option<&Path>) -> Result<Settings> {
    let _ = explicit;
    todo!("cookies agent")
}

/// Parses a bandwidth like `500K`, `2M`, `1.5m`, `100000` (bytes/second, K = 1024).
pub fn parse_rate(text: &str) -> Result<u64> {
    let _ = text;
    todo!("cookies agent")
}

/// Parses `YYYY-MM-DD` as a UTC date. `end_of_day` returns 23:59:59 of that day
/// (for `--until`), otherwise 00:00:00. Returns unix seconds.
pub fn parse_date(text: &str, end_of_day: bool) -> Result<i64> {
    let _ = (text, end_of_day);
    todo!("cookies agent")
}
