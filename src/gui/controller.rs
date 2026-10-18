//! The background side of the window: owns the queue, archive, API client and
//! downloader on its own thread (tokio runtime) and executes [`Command`]s.
//!
//! Layout: `runner` starts the controller and runs its event loop, `grab` is the
//! LinkGrabber (link resolution and staging), `downloads` schedules downloads and
//! applies engine events, `account` handles cookies and settings, `logging`
//! forwards `tracing` events to the Log tab and `sink` sends updates.

mod account;
mod downloads;
mod grab;
mod logging;
mod runner;
mod sink;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::protocol::{Command, Update};
use crate::cli::GlobalArgs;
use crate::config::Settings;
use sink::Sink;

pub use logging::UiLogLayer;

/// Called after every batch of updates (the window passes `ctx.request_repaint`).
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// How long [`ControllerHandle::shutdown`] waits for the thread before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the runtime may take to drop its remaining tasks after the controller ended.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

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

/// A running controller thread. Dropping it shuts the controller down too.
pub struct ControllerHandle {
    commands: UnboundedSender<Command>,
    /// `None` once joined (or detached), or if the thread could not be started.
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ControllerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerHandle")
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl ControllerHandle {
    /// Starts the controller thread. The first updates it sends are `Settings`,
    /// `Account`, `Queue`, `Staged`, `Grabbing` and `Downloading` (or `Fatal`).
    pub fn spawn(config: ControllerConfig, notify: Notify) -> (ControllerHandle, UiLink) {
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (update_tx, update_rx) = std::sync::mpsc::channel();
        let sink = Sink::new(update_tx, notify);
        let thread_sink = sink.clone();
        let spawned = std::thread::Builder::new()
            .name("igdl-controller".to_string())
            .spawn(move || controller_thread(config, thread_sink, command_rx));
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(err) => {
                sink.send(Update::Fatal(format!(
                    "cannot start the background thread: {err}"
                )));
                None
            }
        };
        let handle = ControllerHandle {
            commands: command_tx.clone(),
            thread,
        };
        let link = UiLink {
            commands: command_tx,
            updates: update_rx,
        };
        (handle, link)
    }

    /// Sends `Shutdown` and waits (bounded) for the thread to save and exit.
    pub fn shutdown(mut self) {
        self.stop();
    }

    /// Sends `Shutdown` and joins the thread, detaching it after [`JOIN_TIMEOUT`].
    /// Does nothing the second time.
    fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = self.commands.send(Command::Shutdown);
        let deadline = Instant::now() + JOIN_TIMEOUT;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                // Dropping the handle detaches the thread; it still saves when it gets there.
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = thread.join();
    }
}

impl Drop for ControllerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Body of the controller thread: a two-worker runtime running the controller.
fn controller_thread(
    config: ControllerConfig,
    sink: Sink,
    mut commands: UnboundedReceiver<Command>,
) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("igdl-controller-worker")
        .enable_all()
        .build();
    match runtime {
        Ok(runtime) => {
            runtime.block_on(runner::run(config, sink, &mut commands));
            // Grab requests or downloads that did not stop in time are dropped here.
            runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
        }
        Err(err) => {
            sink.send(Update::Fatal(format!(
                "cannot start the background runtime: {err}"
            )));
            while let Some(command) = commands.blocking_recv() {
                if command == Command::Shutdown {
                    break;
                }
            }
        }
    }
}
