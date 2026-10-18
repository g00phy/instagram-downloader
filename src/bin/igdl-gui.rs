// No console window behind the GUI on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use clap::Parser;

/// igdl desktop window: LinkGrabber, download list and settings.
#[derive(Debug, Parser)]
#[command(name = "igdl-gui", version)]
struct Args {
    #[command(flatten)]
    global: igdl::cli::GlobalArgs,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match igdl::gui::run(args.global) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("igdl-gui: {err}");
            // Without a console (Windows) a dialog is the only way to be seen.
            let _ = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("igdl")
                .set_description(err.to_string())
                .show();
            ExitCode::from(igdl::cli::exit_code(&err))
        }
    }
}
