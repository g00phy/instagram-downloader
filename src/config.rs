//! `config.toml` settings, default directories and value parsers shared by the CLI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::NaiveDate;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::links::ProfileSection;

/// Environment variable overriding the data directory.
pub const ENV_DATA_DIR: &str = "IGDL_DATA_DIR";

/// Largest accepted `jobs` value.
pub const MAX_JOBS: usize = 32;

/// Largest accepted `retries` value.
pub const MAX_RETRIES: u32 = 100;

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

impl Settings {
    /// Checks the values that are set: `jobs` in `1..=32`, `api_delay` finite and
    /// ≥ 0, `retries` ≤ 100, a non-empty `template`, a valid `limit_rate` and
    /// known `include` sections. The error names the offending key.
    pub fn validate(&self) -> Result<()> {
        if let Some(jobs) = self.jobs
            && !(1..=MAX_JOBS).contains(&jobs)
        {
            return Err(Error::Config(format!(
                "jobs must be between 1 and {MAX_JOBS} (got {jobs})"
            )));
        }
        if let Some(delay) = self.api_delay
            && !(delay.is_finite() && delay >= 0.0)
        {
            return Err(Error::Config(format!(
                "api_delay must be a number of seconds >= 0 (got {delay})"
            )));
        }
        if let Some(retries) = self.retries
            && retries > MAX_RETRIES
        {
            return Err(Error::Config(format!(
                "retries must be at most {MAX_RETRIES} (got {retries})"
            )));
        }
        if let Some(template) = &self.template
            && template.trim().is_empty()
        {
            return Err(Error::Config("template must not be empty".to_string()));
        }
        if let Some(rate) = &self.limit_rate {
            parse_rate(rate).map_err(|err| with_context("limit_rate", err))?;
        }
        if let Some(include) = &self.include {
            if include.is_empty() {
                return Err(Error::Config(
                    "include must name at least one profile section".to_string(),
                ));
            }
            for section in include {
                ProfileSection::from_str(section).map_err(|err| with_context("include", err))?;
            }
        }
        Ok(())
    }
}

/// `<config dir>/config.toml` (`%APPDATA%\igdl\config\config.toml`, `~/.config/igdl/config.toml`).
pub fn default_config_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join("config.toml"))
}

/// Data directory: `$IGDL_DATA_DIR`, else the platform data dir
/// (`%APPDATA%\igdl\data`, `~/.local/share/igdl`).
pub fn default_data_dir() -> Result<PathBuf> {
    data_dir_from(std::env::var_os(ENV_DATA_DIR), || {
        project_dirs().map(|dirs| dirs.data_dir().to_path_buf())
    })
}

/// Loads settings. An explicit path must exist; the default path may be absent
/// (→ `Settings::default()`). Unknown keys are a [`crate::error::Error::Config`].
pub fn load(explicit: Option<&Path>) -> Result<Settings> {
    match explicit {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|err| read_error(path, &err))?;
            parse_settings(&text, path)
        }
        None => match default_config_path() {
            Some(path) => load_optional(&path),
            None => Ok(Settings::default()),
        },
    }
}

/// Parses a bandwidth like `500K`, `2M`, `1.5m`, `100000` (bytes/second, K = 1024).
pub fn parse_rate(text: &str) -> Result<u64> {
    let shown = text.trim();
    let invalid = || {
        Error::Config(format!(
            "invalid rate '{shown}' (expected bytes per second, e.g. 500K, 2M or 100000)"
        ))
    };
    let lower = shown.to_ascii_lowercase();
    let mut rest = lower.as_str();
    rest = rest.strip_suffix("/s").unwrap_or(rest).trim_end();
    rest = rest.strip_suffix('b').unwrap_or(rest).trim_end();
    if let Some(binary) = rest.strip_suffix('i')
        && binary.ends_with(['k', 'm', 'g'])
    {
        rest = binary;
    }
    let (number, multiplier) = [('k', 1u64 << 10), ('m', 1 << 20), ('g', 1 << 30)]
        .iter()
        .find_map(|&(suffix, multiplier)| rest.strip_suffix(suffix).map(|n| (n, multiplier)))
        .unwrap_or((rest, 1));
    let number = number.trim();
    let well_formed = number.bytes().any(|b| b.is_ascii_digit())
        && number.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if !well_formed {
        return Err(invalid());
    }
    let value = number.parse::<f64>().map_err(|_| invalid())? * multiplier as f64;
    if value < 1.0 {
        return Err(Error::Config(format!(
            "rate '{shown}' is too low (minimum 1 byte per second)"
        )));
    }
    // Float → integer casts saturate, so absurdly large rates become u64::MAX.
    Ok(value as u64)
}

/// Parses `YYYY-MM-DD` as a UTC date. `end_of_day` returns 23:59:59 of that day
/// (for `--until`), otherwise 00:00:00. Returns unix seconds.
pub fn parse_date(text: &str, end_of_day: bool) -> Result<i64> {
    let trimmed = text.trim();
    let invalid = || Error::Config(format!("invalid date '{trimmed}' (expected YYYY-MM-DD)"));
    let date = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").map_err(|_| invalid())?;
    let (hour, minute, second) = if end_of_day { (23, 59, 59) } else { (0, 0, 0) };
    let at = date.and_hms_opt(hour, minute, second).ok_or_else(invalid)?;
    Ok(at.and_utc().timestamp())
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "igdl")
}

/// [`default_data_dir`] with the environment value and platform lookup passed in.
fn data_dir_from(
    env_value: Option<OsString>,
    platform: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(value) = env_value.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    platform().ok_or_else(|| {
        Error::Config(format!(
            "cannot determine the data directory (no home directory); set {ENV_DATA_DIR} or use --data-dir"
        ))
    })
}

/// Loads a config file that may legitimately be absent (the default path).
fn load_optional(path: &Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_settings(&text, path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(err) => Err(read_error(path, &err)),
    }
}

fn read_error(path: &Path, err: &std::io::Error) -> Error {
    Error::Config(format!("cannot read config file {}: {err}", path.display()))
}

/// Parses and validates `config.toml` contents; errors are prefixed with `path`.
fn parse_settings(text: &str, path: &Path) -> Result<Settings> {
    // Windows editors may save a UTF-8 byte order mark, which TOML rejects.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let settings: Settings = toml::from_str(text).map_err(|err| {
        Error::Config(format!(
            "{}: {}",
            path.display(),
            err.to_string().trim_end()
        ))
    })?;
    settings
        .validate()
        .map_err(|err| with_context(&path.display().to_string(), err))?;
    Ok(settings)
}

/// Prefixes a config error message with `context: `.
fn with_context(context: &str, err: Error) -> Error {
    match err {
        Error::Config(message) => Error::Config(format!("{context}: {message}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_message<T: std::fmt::Debug>(result: Result<T>) -> String {
        match result {
            Err(Error::Config(message)) => message,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    fn write_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn loads_full_config() {
        let (_dir, path) = write_config(
            r#"
            out_dir = 'D:\insta'
            cookies = 'C:\Users\me\secrets\instagram-cookies.txt'
            data_dir = "/tmp/igdl data"
            jobs = 6
            template = "{owner}/{category}/{date}_{id}_{index}.{ext}"
            retries = 3
            api_delay = 2
            limit_rate = "2M"
            user_agent = "Mozilla/5.0 Test"
            include = ["posts", "Stories"]
            set_mtime = false
            "#,
        );
        let settings = load(Some(&path)).unwrap();
        assert_eq!(settings.out_dir, Some(PathBuf::from(r"D:\insta")));
        assert_eq!(settings.data_dir, Some(PathBuf::from("/tmp/igdl data")));
        assert_eq!(settings.jobs, Some(6));
        assert_eq!(settings.retries, Some(3));
        assert_eq!(settings.api_delay, Some(2.0));
        assert_eq!(settings.limit_rate.as_deref(), Some("2M"));
        assert_eq!(
            settings.include,
            Some(vec!["posts".to_string(), "Stories".to_string()])
        );
        assert_eq!(settings.set_mtime, Some(false));
    }

    #[test]
    fn empty_and_bom_configs() {
        let (_dir, path) = write_config("");
        assert_eq!(load(Some(&path)).unwrap(), Settings::default());
        let (_dir, path) = write_config("\u{feff}jobs = 2\n");
        assert_eq!(load(Some(&path)).unwrap().jobs, Some(2));
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let message = config_message(load(Some(&path)));
        assert!(message.contains("nope.toml"), "{message}");
        assert!(message.starts_with("cannot read config file"), "{message}");
    }

    #[test]
    fn absent_default_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_optional(&dir.path().join("config.toml")).unwrap();
        assert_eq!(settings, Settings::default());

        let (_dir, path) = write_config("jobs = 3");
        assert_eq!(load_optional(&path).unwrap().jobs, Some(3));
        // A directory where the file should be is not "absent".
        assert!(load_optional(dir.path()).is_err());
    }

    #[test]
    fn toml_errors_name_the_path() {
        let (_dir, path) = write_config("jobs = \"many\"\n");
        let message = config_message(load(Some(&path)));
        assert!(
            message.starts_with(&path.display().to_string()),
            "{message}"
        );

        let (_dir, path) = write_config("colour = \"blue\"\n");
        let message = config_message(load(Some(&path)));
        assert!(message.contains("colour"), "{message}");

        let (_dir, path) = write_config("jobs = \n");
        assert!(matches!(load(Some(&path)), Err(Error::Config(_))));
    }

    #[test]
    fn validation_errors() {
        let cases = [
            ("jobs = 0", "jobs must be between 1 and 32"),
            ("jobs = 33", "jobs must be between 1 and 32"),
            ("api_delay = -0.5", "api_delay"),
            ("api_delay = nan", "api_delay"),
            ("api_delay = inf", "api_delay"),
            ("retries = 101", "retries must be at most 100"),
            ("template = \"  \"", "template must not be empty"),
            ("limit_rate = \"fast\"", "limit_rate: invalid rate 'fast'"),
            ("limit_rate = \"0\"", "limit_rate: rate '0' is too low"),
            (
                "include = [\"posts\", \"likes\"]",
                "include: unknown profile section 'likes'",
            ),
            ("include = []", "include must name at least one"),
        ];
        for (contents, expected) in cases {
            let (_dir, path) = write_config(contents);
            let message = config_message(load(Some(&path)));
            assert!(message.contains(expected), "{contents}: {message}");
            assert!(message.contains("config.toml"), "{contents}: {message}");
        }
        let (_dir, path) = write_config("jobs = 32\nretries = 0\napi_delay = 0\n");
        assert!(load(Some(&path)).is_ok());
    }

    #[test]
    fn settings_validate_directly() {
        assert!(Settings::default().validate().is_ok());
        let settings = Settings {
            jobs: Some(1),
            limit_rate: Some("500K".to_string()),
            include: Some(vec!["highlights".to_string()]),
            ..Settings::default()
        };
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn parses_rates() {
        let cases = [
            ("100000", 100_000),
            ("500K", 512_000),
            ("500k", 512_000),
            ("2M", 2 * 1024 * 1024),
            ("1.5m", 1_572_864),
            ("1G", 1 << 30),
            (" 2 MB/s ", 2 * 1024 * 1024),
            ("500KB/s", 512_000),
            ("2MiB", 2 * 1024 * 1024),
            ("100b", 100),
            ("100B/s", 100),
            ("0.5K", 512),
            ("1", 1),
            ("1.", 1),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_rate(text).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn rejects_bad_rates() {
        for text in [
            "", " ", "0", "0.5", "-1", "+5", "K", "abc", "1.2.3M", "inf", "nan", "1e3", "2T", "MB",
            "5i",
        ] {
            let message = config_message(parse_rate(text));
            assert!(message.contains("rate"), "{text}: {message}");
        }
        assert_eq!(parse_rate("99999999999999999999G").unwrap(), u64::MAX);
    }

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("2024-05-01", false).unwrap(), 1_714_521_600);
        assert_eq!(
            parse_date(" 2024-05-01 ", true).unwrap(),
            1_714_521_600 + 86_399
        );
        assert_eq!(parse_date("2024-02-29", false).unwrap(), 1_709_164_800);
        assert_eq!(parse_date("1970-01-01", false).unwrap(), 0);
    }

    #[test]
    fn rejects_bad_dates() {
        for text in [
            "",
            "2024-13-01",
            "2023-02-29",
            "2024-04-31",
            "01/05/2024",
            "2024-05-01T00:00:00",
            "yesterday",
        ] {
            let message = config_message(parse_date(text, false));
            assert_eq!(
                message,
                format!("invalid date '{}' (expected YYYY-MM-DD)", text.trim())
            );
        }
    }

    #[test]
    fn data_dir_prefers_non_empty_env() {
        let platform = || Some(PathBuf::from("/platform/igdl"));
        assert_eq!(
            data_dir_from(Some(OsString::from("/custom data")), platform).unwrap(),
            PathBuf::from("/custom data")
        );
        assert_eq!(
            data_dir_from(Some(OsString::new()), platform).unwrap(),
            PathBuf::from("/platform/igdl")
        );
        assert_eq!(
            data_dir_from(None, platform).unwrap(),
            PathBuf::from("/platform/igdl")
        );
        let message = config_message(data_dir_from(None, || None));
        assert!(message.contains(ENV_DATA_DIR), "{message}");
    }

    #[test]
    fn default_paths() {
        if let Some(path) = default_config_path() {
            assert!(path.ends_with("config.toml"));
            assert!(path.to_string_lossy().contains("igdl"));
        }
    }
}
