//! How the `igdl` binary talks to the console: colours only on a terminal, and
//! the usual usage error when started without arguments from a shell.

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

#[test]
fn no_arguments_from_a_shell_is_the_usual_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = igdl(dir.path(), &[], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Usage:"), "{stderr}");
    assert!(!stderr.contains("Press Enter"), "{stderr}");
}
