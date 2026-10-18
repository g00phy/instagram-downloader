//! The desktop window (`igdl-gui`): a JDownloader-style front end over the same
//! queue, archive, crawler and download engine as the CLI.
//!
//! [`view`] renders state and sends [`protocol::Command`]s; [`controller`] runs on
//! its own thread with a tokio runtime, does the work, and streams
//! [`protocol::Update`]s back.

pub mod controller;
pub mod protocol;
pub mod view;

use std::sync::Arc;

use eframe::egui;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::GlobalArgs;
use crate::config;
use crate::error::{Error, Result};
use controller::{ControllerConfig, ControllerHandle, UiLogLayer};
use view::IgdlApp;

/// Opens the window and blocks until it is closed.
pub fn run(global: GlobalArgs) -> Result<()> {
    let settings = config::load(global.config.as_deref())?;
    let config_path = global.config.clone().or_else(config::default_config_path);
    init_logging(&global);
    let config = ControllerConfig {
        global,
        settings,
        config_path,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("igdl — Instagram downloader")
            .with_app_id("igdl")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([760.0, 420.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "igdl",
        options,
        Box::new(move |creation| {
            let ctx = creation.egui_ctx.clone();
            let (handle, link) =
                ControllerHandle::spawn(config, Arc::new(move || ctx.request_repaint()));
            Ok(Box::new(IgdlApp::new(link, Some(handle))))
        }),
    )
    .map_err(|err| Error::Config(format!("cannot open the window: {err}")))
}

/// stderr gets warnings (or `IGDL_LOG`), the Log tab gets igdl's INFO and above.
fn init_logging(global: &GlobalArgs) {
    let default = match global.verbose {
        0 => "warn",
        1 => "igdl=info,warn",
        _ => "igdl=debug,info",
    };
    let stderr_filter =
        EnvFilter::try_from_env("IGDL_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let window_filter = Targets::new()
        .with_target("igdl", LevelFilter::INFO)
        .with_default(LevelFilter::WARN);
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .without_time()
                .with_filter(stderr_filter),
        )
        .with(UiLogLayer.with_filter(window_filter))
        .try_init();
}
