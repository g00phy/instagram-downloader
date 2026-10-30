//! How the `igdl` binary talks to the console: colours only on a terminal
//! (and not with NO_COLOR), and the usual usage error when started without
//! arguments from a shell.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// Runs igdl offline with an isolated data dir and stderr piped (not a
/// terminal). `env` is applied after the colour variables are cleared.
fn igdl(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_igdl"));
    command
        .args(args)
        .env("IGDL_DATA_DIR", dir.join("data"))
        .env("IGDL_API_BASE", "http://127.0.0.1:9")
        .env_remove("IGDL_COOKIES")
        .env_remove("IGDL_LOG")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(Stdio::null())
        .current_dir(dir);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn verbose_list(dir: &TempDir, env: &[(&str, &str)]) -> String {
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let output = igdl(
        dir.path(),
        &["-v", "list", "--config", config.to_str().unwrap()],
        env,
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn log_lines_have_no_colour_codes_when_stderr_is_not_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = verbose_list(&dir, &[]);
    assert!(stderr.contains("INFO"), "{stderr}");
    assert!(stderr.contains("settings file:"), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
}

#[test]
fn clicolor_force_still_colours_log_lines() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = verbose_list(&dir, &[("CLICOLOR_FORCE", "1")]);
    assert!(stderr.contains("\u{1b}["), "{stderr:?}");
}

/// `igdl -v list` with stdout and stderr on a pseudo-terminal (util-linux
/// `script`), colour variables cleared, then `env` applied. `None` (test
/// skipped) when no pseudo-terminal can be had.
#[cfg(target_os = "linux")]
fn verbose_list_on_terminal(dir: &TempDir, env: &[(&str, &str)]) -> Option<String> {
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let inner = format!(
        "'{}' -v list --config '{}'",
        env!("CARGO_BIN_EXE_igdl"),
        config.display()
    );
    let mut command = Command::new("script");
    command
        .args(["-qec", &inner, "/dev/null"])
        .env("IGDL_DATA_DIR", dir.path().join("data"))
        .env("IGDL_API_BASE", "http://127.0.0.1:9")
        .env("TERM", "xterm")
        .env_remove("IGDL_COOKIES")
        .env_remove("IGDL_LOG")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(Stdio::null())
        .current_dir(dir.path());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.contains("settings file:") {
        Some(text)
    } else {
        eprintln!("skipped: no pseudo-terminal via `script` ({output:?})");
        None
    }
}

#[cfg(target_os = "linux")]
#[test]
fn log_lines_are_coloured_on_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    if let Some(text) = verbose_list_on_terminal(&dir, &[]) {
        assert!(text.contains("\u{1b}["), "{text:?}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn no_color_turns_colours_off_on_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    if let Some(text) = verbose_list_on_terminal(&dir, &[("NO_COLOR", "1")]) {
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
}

#[test]
fn no_arguments_from_a_shell_is_the_usual_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = igdl(dir.path(), &[], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Usage:"), "{stderr}");
    assert!(!stderr.contains("Press Enter"), "{stderr}");
}
