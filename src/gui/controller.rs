//! The background side of the window: owns the queue, archive, API client and
//! downloader on its own thread (tokio runtime) and executes [`Command`]s.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use tokio::sync::mpsc::UnboundedSender;

use super::protocol::{Command, Update};
use crate::cli::GlobalArgs;
use crate::config::Settings;

/// Called after every batch of updates (the window passes `ctx.request_repaint`).
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// Everything the controller needs at start.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Overrides from the command line / environment (data dir, cookies, API base…).
    pub global: GlobalArgs,
    /// Settings loaded from `config.toml`.
    pub settings: Settings,
    /// Where `ApplySettings` saves (`None`: settings cannot be saved).
    pub config_path: Option<PathBuf>,
}

/// The window's end of the channels.
pub struct UiLink {
    pub commands: UnboundedSender<Command>,
    pub updates: Receiver<Update>,
}

/// A running controller thread.
pub struct ControllerHandle {
    _private: (),
}

impl ControllerHandle {
    /// Starts the controller thread. The first updates it sends are `Settings`,
    /// `Account`, `Queue`, `Staged`, `Grabbing` and `Downloading` (or `Fatal`).
    pub fn spawn(config: ControllerConfig, notify: Notify) -> (ControllerHandle, UiLink) {
        let _ = (config, notify);
        todo!("controller agent")
    }

    /// Sends `Shutdown` and waits (bounded) for the thread to save and exit.
    pub fn shutdown(self) {
        todo!("controller agent")
    }
}

/// `tracing` layer that forwards `igdl` events (INFO and above) to the running
/// controller's window as [`Update::Log`] lines (redacted). A no-op while no
/// controller runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct UiLogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for UiLogLayer {}
