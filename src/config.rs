//! `config.toml` settings, default directories and value parsers shared by the CLI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::NaiveDate;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsutil;
use crate::links::ProfileSection;

/// Environment variable overriding the data directory.
pub const ENV_DATA_DIR: &str = "IGDL_DATA_DIR";

/// Largest accepted `jobs` value.
pub const MAX_JOBS: usize = 32;

/// Largest accepted `retries` value.
pub const MAX_RETRIES: u32 = 100;

/// File name of the settings file.
const CONFIG_FILE: &str = "config.toml";

/// How to write a Windows path in TOML, for errors caused by backslash escapes.
const WINDOWS_PATH_HINT: &str = "Windows paths need single quotes (out_dir = 'D:\\insta') \
     or doubled backslashes (out_dir = \"D:\\\\insta\")";

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
    /// Checks the values that are set: paths without control characters,
    /// `jobs` in `1..=32`, `api_delay` finite and ≥ 0, `retries` ≤ 100, a
    /// non-empty `template`, a valid `limit_rate` and known `include` sections.
    /// The error names the offending key.
    pub fn validate(&self) -> Result<()> {
        for (key, path) in self.paths() {
            // What `"C:\temp"` becomes in a double-quoted TOML string: `\t` is a TAB.
            if path.to_string_lossy().chars().any(char::is_control) {
                return Err(Error::Config(format!(
                    "{key} contains a control character (\"C:\\temp\" in double quotes holds \
                     a TAB: \\t is an escape); {WINDOWS_PATH_HINT}"
                )));
            }
        }
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

    /// The path settings that are set, with their keys.
    fn paths(&self) -> impl Iterator<Item = (&'static str, &PathBuf)> {
        [
            ("out_dir", &self.out_dir),
            ("cookies", &self.cookies),
            ("data_dir", &self.data_dir),
        ]
        .into_iter()
        .filter_map(|(key, path)| path.as_ref().map(|path| (key, path)))
    }

    /// Applies [`clean_path`] to the path settings. Values with control
    /// characters are left alone so [`Settings::validate`] rejects them: trimming
    /// would turn `"D:\t"` (a TAB, from an escape) into `D:` without a word.
    fn clean_paths(&mut self) {
        for path in [&mut self.out_dir, &mut self.cookies, &mut self.data_dir]
            .into_iter()
            .flatten()
        {
            if let Some(text) = path.to_str()
                && !text.chars().any(char::is_control)
                && clean_path(text) != text
            {
                *path = PathBuf::from(clean_path(text));
            }
        }
    }
}

/// Trims whitespace and one pair of surrounding double quotes from a path typed
/// or pasted by the user (Explorer's "Copy as path" adds the quotes).
pub fn clean_path(text: &str) -> &str {
    let text = text.trim();
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or(text, str::trim)
}

/// `<config dir>/config.toml` (`%APPDATA%\igdl\config\config.toml`, `~/.config/igdl/config.toml`).
pub fn default_config_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join(CONFIG_FILE))
}

/// Data directory: `$IGDL_DATA_DIR`, else the platform data dir
/// (`%APPDATA%\igdl\data`, `~/.local/share/igdl`).
pub fn default_data_dir() -> Result<PathBuf> {
    data_dir_from(std::env::var_os(ENV_DATA_DIR), || {
        project_dirs().map(|dirs| dirs.data_dir().to_path_buf())
    })
}

/// Loads settings. An explicit path must exist; the default path may be absent
/// (→ `Settings::default()`, with a warning when a `config.toml` sits in a
/// near-miss location). UTF-8 and UTF-16 files are accepted; path settings are
/// cleaned with [`clean_path`]. Unknown keys are a [`crate::error::Error::Config`].
pub fn load(explicit: Option<&Path>) -> Result<Settings> {
    match explicit {
        Some(path) => {
            let text = fsutil::read_text(path).map_err(|err| read_error(path, err))?;
            parse_settings(&text, path)
        }
        None => match project_dirs() {
            Some(dirs) => {
                let near_misses =
                    near_miss_paths(dirs.config_dir(), default_data_dir().ok().as_deref());
                load_optional(&dirs.config_dir().join(CONFIG_FILE), &near_misses)
            }
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
        // cmd.exe keeps the quotes of `set IGDL_DATA_DIR="D:\igdl"` in the value.
        return Ok(match value.to_str() {
            Some(text) => PathBuf::from(clean_path(text)),
            None => PathBuf::from(value),
        });
    }
    platform().ok_or_else(|| {
        Error::Config(format!(
            "cannot determine the data directory (no home directory); set {ENV_DATA_DIR} or use --data-dir"
        ))
    })
}

/// Loads a config file that may legitimately be absent (the default path). When
/// it is absent, warns about the first of `near_misses` that exists.
fn load_optional(path: &Path, near_misses: &[PathBuf]) -> Result<Settings> {
    match fsutil::read_text(path) {
        Ok(text) => parse_settings(&text, path),
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            if let Some(found) = near_misses.iter().find(|candidate| candidate.is_file()) {
                tracing::warn!("{}", misplaced_message(found, path));
            }
            Ok(Settings::default())
        }
        Err(err) => Err(read_error(path, err)),
    }
}

/// The warning for a `config.toml` found at `found` while igdl reads `expected`.
fn misplaced_message(found: &Path, expected: &Path) -> String {
    format!(
        "{} is not read: igdl reads its settings from {} (move the file there)",
        found.display(),
        expected.display()
    )
}

/// When the default `config.toml` does not exist but one sits in a near-miss
/// location, the warning to show (the window repeats it in its Log tab, where
/// stderr warnings are invisible on Windows).
pub fn misplaced_config_warning() -> Option<String> {
    let dirs = project_dirs()?;
    let expected = dirs.config_dir().join(CONFIG_FILE);
    if expected.exists() {
        return None;
    }
    near_miss_paths(dirs.config_dir(), default_data_dir().ok().as_deref())
        .iter()
        .find(|candidate| candidate.is_file())
        .map(|found| misplaced_message(found, &expected))
}

/// Places where users put `config.toml` by mistake, given the platform config
/// and data dirs: the parent of the config dir while still inside igdl's own
/// folder (Windows adds a `config` subfolder: `%APPDATA%\igdl\config.toml`), the
/// data dir, and `config.toml.txt` (Notepad's default type, shown without `.txt`).
fn near_miss_paths(config_dir: &Path, data_dir: Option<&Path>) -> Vec<PathBuf> {
    let default = config_dir.join(CONFIG_FILE);
    let mut candidates = Vec::new();
    if config_dir.file_name().is_some_and(|name| name != "igdl")
        && let Some(parent) = config_dir.parent()
    {
        candidates.push(parent.join(CONFIG_FILE));
    }
    if let Some(data_dir) = data_dir {
        candidates.push(data_dir.join(CONFIG_FILE));
    }
    candidates.push(config_dir.join(format!("{CONFIG_FILE}.txt")));
    candidates.retain(|candidate| *candidate != default);
    candidates.dedup();
    candidates
}

/// Turns a [`fsutil::read_text`] failure into a config error naming the path.
fn read_error(path: &Path, err: Error) -> Error {
    match err {
        Error::Io { source, .. } => Error::Config(format!(
            "cannot read config file {}: {source}",
            path.display()
        )),
        other => other,
    }
}

/// Parses and validates `config.toml` contents; errors are prefixed with `path`.
fn parse_settings(text: &str, path: &Path) -> Result<Settings> {
    let mut settings: Settings = toml::from_str(text).map_err(|err| {
        let mut message = format!("{}: {}", path.display(), err.to_string().trim_end());
        if has_backslash_escape(text) {
            message.push_str("\nhint: ");
            message.push_str(WINDOWS_PATH_HINT);
        }
        Error::Config(message)
    })?;
    settings.clean_paths();
    settings
        .validate()
        .map_err(|err| with_context(&path.display().to_string(), err))?;
    Ok(settings)
}

/// Whether a double-quoted TOML string holds a backslash that is not part of a
/// doubled `\\`: the usual sign of a Windows path written in the wrong quotes.
/// A line-by-line heuristic for error hints (multi-line strings are not tracked).
fn has_backslash_escape(text: &str) -> bool {
    text.lines().any(|line| {
        let mut chars = line.chars();
        let mut quote = None;
        while let Some(c) = chars.next() {
            match (quote, c) {
                (None, '#') => break,
                (None, '"' | '\'') => quote = Some(c),
                (Some(open), _) if c == open => quote = None,
                // `\\` is an escaped backslash; anything else after `\` is suspect.
                (Some('"'), '\\') => match chars.next() {
                    Some('\\') => {}
                    _ => return true,
                },
                _ => {}
            }
        }
        false
    })
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
        let settings = load_optional(&dir.path().join("config.toml"), &[]).unwrap();
        assert_eq!(settings, Settings::default());

        let (_dir, path) = write_config("jobs = 3");
        assert_eq!(load_optional(&path, &[]).unwrap().jobs, Some(3));
        // A directory where the file should be is not "absent".
        assert!(load_optional(dir.path(), &[]).is_err());
    }

    /// Collects what a `tracing` fmt subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn load_optional_logged(path: &Path, near_misses: &[PathBuf]) -> (Settings, String) {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let settings = tracing::subscriber::with_default(subscriber, || {
            load_optional(path, near_misses).unwrap()
        });
        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        (settings, output)
    }

    #[test]
    fn warns_about_a_config_in_a_near_miss_location() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("igdl").join("config");
        let default = config_dir.join("config.toml");
        let near_misses = near_miss_paths(&config_dir, None);

        let (settings, output) = load_optional_logged(&default, &near_misses);
        assert_eq!(settings, Settings::default());
        assert!(output.is_empty(), "{output}");

        let misplaced = dir.path().join("igdl").join("config.toml");
        std::fs::create_dir_all(misplaced.parent().unwrap()).unwrap();
        std::fs::write(&misplaced, "jobs = 2\n").unwrap();
        let (settings, output) = load_optional_logged(&default, &near_misses);
        assert_eq!(
            settings,
            Settings::default(),
            "the misplaced file is not read"
        );
        assert!(output.contains("WARN"), "{output}");
        assert!(
            output.contains(&format!("{} is not read", misplaced.display())),
            "{output}"
        );
        assert!(
            output.contains(&format!("reads its settings from {}", default.display())),
            "{output}"
        );

        // Once the real file exists there is nothing to warn about.
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(&default, "jobs = 4\n").unwrap();
        let (settings, output) = load_optional_logged(&default, &near_misses);
        assert_eq!(settings.jobs, Some(4));
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn near_miss_locations() {
        // Windows: %APPDATA%\igdl\config holds the file; %APPDATA%\igdl is a near miss.
        let config_dir = Path::new("/appdata/igdl/config");
        assert_eq!(
            near_miss_paths(config_dir, Some(Path::new("/appdata/igdl/data"))),
            [
                PathBuf::from("/appdata/igdl/config.toml"),
                PathBuf::from("/appdata/igdl/data/config.toml"),
                PathBuf::from("/appdata/igdl/config/config.toml.txt"),
            ]
        );
        // Linux: ~/.config is not igdl's folder; macOS: data dir == config dir.
        assert_eq!(
            near_miss_paths(Path::new("/home/me/.config/igdl"), None),
            [PathBuf::from("/home/me/.config/igdl/config.toml.txt")]
        );
        let mac = Path::new("/Users/me/Library/Application Support/igdl");
        assert_eq!(
            near_miss_paths(mac, Some(mac)),
            [mac.join("config.toml.txt")]
        );
    }

    #[test]
    fn reads_utf16_configs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Windows PowerShell 5.1 `Out-File` / `>` writes UTF-16LE with a BOM.
        let text = "out_dir = 'C:\\Users\\José\\insta'\r\njobs = 5\r\n";
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&path, bytes).unwrap();
        let settings = load(Some(&path)).unwrap();
        assert_eq!(
            settings.out_dir,
            Some(PathBuf::from(r"C:\Users\José\insta"))
        );
        assert_eq!(settings.jobs, Some(5));

        std::fs::write(&path, b"out_dir = 'C:\\Users\\Jos\xE9'\r\n").unwrap();
        let message = config_message(load(Some(&path)));
        assert!(message.contains("config.toml"), "{message}");
        assert!(message.contains("not UTF-8 or UTF-16"), "{message}");
    }

    #[test]
    fn hints_at_single_quotes_for_windows_paths() {
        // Invalid escapes: `\U` and `\i`.
        for contents in [
            r#"cookies = "C:\Users\me\cookies.txt""#,
            r#"out_dir = "D:\insta""#,
            r#"jobs = 2 # "D:\x" in a comment is fine
            out_dir = "D:\insta""#,
        ] {
            let (_dir, path) = write_config(contents);
            let message = config_message(load(Some(&path)));
            assert!(message.contains("config.toml"), "{contents}: {message}");
            assert!(
                message.contains("hint: Windows paths need single quotes (out_dir = 'D:\\insta')"),
                "{contents}: {message}"
            );
        }
        // Errors unrelated to backslashes get no hint.
        for contents in [
            "jobs = \"many\"",
            r#"out_dir = 'D:\insta'
            colour = "blue""#,
            r#"out_dir = "D:\\insta"
            colour = "blue""#,
        ] {
            let (_dir, path) = write_config(contents);
            let message = config_message(load(Some(&path)));
            assert!(!message.contains("hint:"), "{contents}: {message}");
        }
    }

    #[test]
    fn rejects_control_characters_in_paths() {
        // Valid TOML escapes that turn Windows paths into TAB, LF, BS and ESC.
        for (contents, key) in [
            (r#"out_dir = "D:\temp""#, "out_dir"),
            (r#"out_dir = "C:\new folder""#, "out_dir"),
            (r#"cookies = "D:\backup.txt""#, "cookies"),
            (r#"data_dir = "D:\escape""#, "data_dir"),
        ] {
            let (_dir, path) = write_config(contents);
            let message = config_message(load(Some(&path)));
            assert!(
                message.contains(&format!("{key} contains a control character")),
                "{contents}: {message}"
            );
            assert!(
                message.contains("Windows paths need single quotes"),
                "{contents}: {message}"
            );
            assert!(message.contains("config.toml"), "{contents}: {message}");
        }
        let settings = Settings {
            out_dir: Some(PathBuf::from("D:\tinsta")),
            ..Settings::default()
        };
        assert!(settings.validate().is_err());

        // An escape at the very end must not be trimmed away as whitespace:
        // `"D:\t"` is the folder `D:\t` mistyped, not `D:`.
        for contents in [r#"out_dir = "D:\t""#, r#"out_dir = "E:\n""#] {
            let (_dir, path) = write_config(contents);
            let message = config_message(load(Some(&path)));
            assert!(
                message.contains("out_dir contains a control character"),
                "{contents}: {message}"
            );
        }
    }

    #[test]
    fn cleans_quoted_and_padded_paths() {
        assert_eq!(
            clean_path(r#""C:\Users\me\cookies.txt""#),
            r"C:\Users\me\cookies.txt"
        );
        assert_eq!(clean_path(r#"  "D:\My Insta" "#), r"D:\My Insta");
        assert_eq!(clean_path(" /data/igdl \t"), "/data/igdl");
        assert_eq!(clean_path(r#"""#), r#"""#, "a lone quote stays");
        assert_eq!(clean_path(r#""a"b""#), r#"a"b"#, "one pair only");
        assert_eq!(clean_path(r"D:\insta"), r"D:\insta");

        let (_dir, path) = write_config(
            r#"
            out_dir = '"D:\My Insta"'
            cookies = '  C:\Users\me\cookies.txt  '
            data_dir = "/srv/igdl"
            "#,
        );
        let settings = load(Some(&path)).unwrap();
        assert_eq!(settings.out_dir, Some(PathBuf::from(r"D:\My Insta")));
        assert_eq!(
            settings.cookies,
            Some(PathBuf::from(r"C:\Users\me\cookies.txt"))
        );
        assert_eq!(settings.data_dir, Some(PathBuf::from("/srv/igdl")));
    }

    #[test]
    fn detects_backslash_escapes_in_basic_strings() {
        assert!(has_backslash_escape(r#"out_dir = "D:\insta""#));
        assert!(has_backslash_escape(
            r#"a = 1
            b = "x\ty""#
        ));
        assert!(!has_backslash_escape(r#"out_dir = "D:\\insta""#));
        assert!(!has_backslash_escape(r"out_dir = 'D:\insta'"));
        assert!(!has_backslash_escape(r#"out_dir = '"D:\insta"'"#));
        assert!(!has_backslash_escape(r#"jobs = 2 # "D:\insta""#));
        assert!(!has_backslash_escape("template = \"{owner}/{id}.{ext}\""));
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
