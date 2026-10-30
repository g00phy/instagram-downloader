// No console window behind the GUI on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

/// igdl desktop window: LinkGrabber, download list and settings.
#[derive(Debug, Parser)]
#[command(name = "igdl-gui", version)]
struct Args {
    #[command(flatten)]
    global: igdl::cli::GlobalArgs,
}

/// Exit code of a process that panicked (what Rust's runtime uses).
const EXIT_PANIC: u8 = 101;

fn main() -> ExitCode {
    // Without a console (Windows) stderr goes nowhere: panics and startup
    // errors also go to a crash log, and a dialog says where it is.
    let crash_dirs = igdl::gui::crash_log_dirs();
    igdl::gui::install_panic_hook(crash_dirs.clone(), show_panic_dialog);
    // Unwind safety: after a panic nothing from the closure is used again.
    let code = std::panic::catch_unwind(AssertUnwindSafe(|| run(&crash_dirs)))
        .unwrap_or(ExitCode::from(EXIT_PANIC));
    // A panic on this thread is reported only now, with the window gone: a
    // dialog from inside the hook could re-enter winit while it unwinds.
    igdl::gui::notify_panic(show_panic_dialog);
    code
}

fn run(crash_dirs: &[PathBuf]) -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err) => return usage(&err),
    };
    match igdl::gui::run(args.global) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("igdl-gui: {err}");
            let log =
                igdl::gui::append_crash_log(crash_dirs, &igdl::gui::startup_error_entry(&err));
            let mut text = err.to_string();
            if let Some(log) = log {
                text.push_str(&format!("\n\n(Also written to {}.)", log.display()));
            }
            // Without a console (Windows) a dialog is the only way to be seen.
            show_dialog(rfd::MessageLevel::Error, &text);
            ExitCode::from(igdl::cli::exit_code(&err))
        }
    }
}

/// `--help`, `--version` or a bad argument: clap's text on stdout/stderr (for a
/// console or a redirect) and, on Windows, in a dialog; clap's exit code.
fn usage(err: &clap::Error) -> ExitCode {
    let _ = err.print();
    if cfg!(windows) {
        let level = if is_info(err.kind()) {
            rfd::MessageLevel::Info
        } else {
            rfd::MessageLevel::Error
        };
        show_dialog(level, &err.render().to_string());
    }
    ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2))
}

/// `--help` and `--version` are answers, not errors.
fn is_info(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion)
}

/// After a panic: where the report is (Windows only; elsewhere stderr has it).
fn show_panic_dialog(log: Option<&Path>) {
    if cfg!(windows) {
        show_dialog(rfd::MessageLevel::Error, &igdl::gui::panic_message(log));
    }
}

fn show_dialog(level: rfd::MessageLevel, text: &str) {
    let _ = rfd::MessageDialog::new()
        .set_level(level)
        .set_title("igdl-gui")
        .set_description(text)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn args_are_well_formed() {
        Args::command().debug_assert();
    }

    #[test]
    fn help_and_version_are_info_with_exit_code_zero() {
        for flag in ["--help", "--version"] {
            let err = Args::try_parse_from(["igdl-gui", flag]).unwrap_err();
            assert!(is_info(err.kind()), "{flag}");
            assert_eq!(err.exit_code(), 0);
            assert!(!err.render().to_string().is_empty());
        }
    }

    #[test]
    fn a_bad_argument_is_an_error_with_exit_code_two() {
        let err = Args::try_parse_from(["igdl-gui", "C:\\links.txt"]).unwrap_err();
        assert!(!is_info(err.kind()));
        assert_eq!(err.exit_code(), 2);
        let text = err.render().to_string();
        assert!(text.contains("unexpected argument"), "{text}");
        assert!(
            !text.contains('\u{1b}'),
            "no ANSI escapes in a dialog: {text:?}"
        );
    }
}
