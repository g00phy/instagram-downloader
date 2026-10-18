//! The controller's state, startup, event loop and shutdown.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::ControllerConfig;
use super::account::{self, ClientSetup};
use super::grab::{self, GrabEvent, GrabJob};
use super::logging;
use super::sink::Sink;
use crate::app::{self, Options};
use crate::archive::Archive;
use crate::cli::{self, DownloadArgs, GlobalArgs};
use crate::config::Settings;
use crate::download::{Downloader, Event};
use crate::error::{Error, Result};
use crate::gui::protocol::{Command, LogLine, StagedPackage, Update};
use crate::instagram::{Client, UserSummary};
use crate::queue::{Queue, QueueStore};
use crate::redact;

/// The archive, shared with the grab worker (for `--update`-style checks).
pub(super) type SharedArchive = Arc<Mutex<Archive>>;

/// The current API client; replaced when settings or cookies change.
pub(super) type ClientSlot = Arc<RwLock<Arc<Client>>>;

/// Minimum time between two `Queue` updates caused by download progress.
const QUEUE_INTERVAL: Duration = Duration::from_millis(250);

/// Minimum time between two saves of `queue.json` while downloading.
const SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// How long `Shutdown` waits for running downloads to stop.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

/// Locks the archive. A poisoned lock still guards a consistent archive: every
/// mutation is a single `insert` call.
pub(super) fn lock_archive(archive: &SharedArchive) -> MutexGuard<'_, Archive> {
    archive.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The client in `slot` (the lock is held only to clone the `Arc`).
pub(super) fn current_client(slot: &ClientSlot) -> Arc<Client> {
    Arc::clone(&slot.read().unwrap_or_else(PoisonError::into_inner))
}

/// Messages from the controller's own tasks.
pub(super) enum Internal {
    /// The grab worker finished a link (or skipped some).
    Grab(GrabEvent),
    /// A `Downloader::run` returned; `item_ids` are all the jobs it was given.
    RunEnded { item_ids: Vec<u64> },
    /// A `CheckAccount` finished for the client of `generation`.
    AccountChecked {
        generation: u64,
        result: Result<UserSummary>,
    },
}

/// Outcomes since downloads last went from idle to running.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RunTotals {
    pub(super) done: usize,
    pub(super) skipped: usize,
    pub(super) failed: usize,
    pub(super) stopped: usize,
}

impl RunTotals {
    /// e.g. `12 downloaded, 1 failed`.
    pub(super) fn summary(&self) -> String {
        let mut text = format!("{} downloaded", self.done);
        for (count, label) in [
            (self.skipped, "skipped"),
            (self.failed, "failed"),
            (self.stopped, "stopped"),
        ] {
            if count > 0 {
                text.push_str(&format!(", {count} {label}"));
            }
        }
        text
    }
}

/// Everything the controller owns. Lives on the controller thread; the window
/// only sees it through [`Update`]s.
pub(super) struct Controller {
    pub(super) sink: Sink,
    pub(super) global: GlobalArgs,
    pub(super) settings: Settings,
    pub(super) config_path: Option<PathBuf>,
    /// Options without a grab request (base for downloads, account, defaults).
    pub(super) options: Options,
    /// Holds the queue lock for the controller's life.
    pub(super) store: QueueStore,
    /// The download list; snapshots are `Arc` clones, edits go through `queue_mut`.
    pub(super) queue: Arc<Queue>,
    pub(super) archive: SharedArchive,
    pub(super) client: ClientSlot,
    /// A cookie jar was loaded into the current client.
    pub(super) has_jar: bool,
    /// Bumped whenever the client is replaced (stale account checks are ignored).
    pub(super) client_generation: u64,
    pub(super) downloader: Arc<Downloader>,
    /// A downloader built by `ApplySettings` while downloads were running.
    pub(super) pending_downloader: Option<Downloader>,

    // LinkGrabber
    pub(super) staged: Vec<StagedPackage>,
    pub(super) next_staged_id: u64,
    /// Links handed to the grab worker and not finished yet.
    pub(super) pending_links: usize,
    pub(super) grab_jobs: UnboundedSender<GrabJob>,
    pub(super) grab_cancel: CancellationToken,

    // Downloads
    /// Item ids handed to a running `Downloader::run`.
    pub(super) scheduled: HashSet<u64>,
    pub(super) active_runs: usize,
    pub(super) download_cancel: CancellationToken,
    pub(super) totals: RunTotals,
    pub(super) events_tx: UnboundedSender<Event>,
    pub(super) internal_tx: UnboundedSender<Internal>,

    // Throttling of `Queue` updates and saves
    pub(super) queue_dirty: bool,
    pub(super) last_queue_sent: Instant,
    pub(super) save_dirty: bool,
    pub(super) last_save: Instant,
}

/// The receiving ends the event loop waits on.
struct Inbox<'a> {
    commands: &'a mut UnboundedReceiver<Command>,
    events: UnboundedReceiver<Event>,
    internal: UnboundedReceiver<Internal>,
    logs: UnboundedReceiver<LogLine>,
}

/// What woke the event loop.
enum Next {
    Command(Option<Command>),
    Event(Event),
    Internal(Internal),
    Log(LogLine),
    Flush,
}

/// Runs a controller until `Shutdown` (or until the window drops its sender).
pub(super) async fn run(
    config: ControllerConfig,
    sink: Sink,
    commands: &mut UnboundedReceiver<Command>,
) {
    let (log_tx, logs) = mpsc::unbounded_channel();
    logging::install(log_tx.clone());
    let (events_tx, events) = mpsc::unbounded_channel();
    let (internal_tx, internal) = mpsc::unbounded_channel();
    match Controller::start(config, sink.clone(), events_tx, internal_tx) {
        Ok(mut controller) => {
            let mut inbox = Inbox {
                commands,
                events,
                internal,
                logs,
            };
            controller.event_loop(&mut inbox).await;
            controller.shutdown(&mut inbox).await;
            // Dropping the controller releases the queue lock.
        }
        Err(message) => {
            sink.send(Update::Fatal(redact::redact(&message)));
            while let Some(command) = commands.recv().await {
                if command == Command::Shutdown {
                    break;
                }
            }
        }
    }
    logging::uninstall(&log_tx);
}

impl Controller {
    /// Opens the queue, archive and client and sends the initial updates.
    /// Errors are the text of the `Fatal` update.
    fn start(
        config: ControllerConfig,
        sink: Sink,
        events_tx: UnboundedSender<Event>,
        internal_tx: UnboundedSender<Internal>,
    ) -> std::result::Result<Controller, String> {
        let ControllerConfig {
            global,
            settings,
            config_path,
        } = config;
        let fatal = |err: Error| err.to_string();
        let options = cli::resolve_options(&global, &settings, None, &DownloadArgs::default())
            .map_err(fatal)?;
        let store = QueueStore::open(&options.data_dir).map_err(|err| match err {
            // The only configuration error `open` reports is a lock held elsewhere.
            Error::Config(_) => format!(
                "Another igdl is using the download list in {}. Close it and start igdl-gui again.",
                options.data_dir.display()
            ),
            other => other.to_string(),
        })?;
        let mut queue = store.load().map_err(fatal)?;
        let interrupted = queue.reset_interrupted();
        store.save(&queue).map_err(fatal)?;
        let archive = app::open_archive(&options).map_err(fatal)?;
        let setup = account::build_client(&options).map_err(fatal)?;
        let downloader = Downloader::new(options.download.clone()).map_err(fatal)?;

        let archive = Arc::new(Mutex::new(archive));
        let ClientSetup {
            client,
            account,
            has_jar,
            logs,
        } = setup;
        let client = Arc::new(RwLock::new(client));
        let grab_cancel = CancellationToken::new();
        let grab_jobs = grab::spawn_worker(
            Arc::clone(&client),
            Arc::clone(&archive),
            internal_tx.clone(),
            grab_cancel.clone(),
        );
        let now = Instant::now();
        let mut controller = Controller {
            sink,
            global,
            settings,
            config_path,
            options,
            store,
            queue: Arc::new(queue),
            archive,
            client,
            has_jar,
            client_generation: 0,
            downloader: Arc::new(downloader),
            pending_downloader: None,
            staged: Vec::new(),
            next_staged_id: 1,
            pending_links: 0,
            grab_jobs,
            grab_cancel,
            scheduled: HashSet::new(),
            active_runs: 0,
            download_cancel: CancellationToken::new(),
            totals: RunTotals::default(),
            events_tx,
            internal_tx,
            queue_dirty: false,
            last_queue_sent: now,
            save_dirty: false,
            last_save: now,
        };

        controller.send_settings();
        controller.sink.send(Update::Account(account));
        controller.send_queue();
        controller.sink.send(Update::Staged(Vec::new()));
        controller.sink.send(Update::Grabbing { pending_links: 0 });
        controller.sink.send(Update::Downloading(false));
        for (level, text) in logs {
            controller.sink.log(level, text);
        }
        if interrupted > 0 {
            controller.sink.info(format!(
                "{interrupted} interrupted download(s) will resume when the downloads start"
            ));
        }
        Ok(controller)
    }

    async fn event_loop(&mut self, inbox: &mut Inbox<'_>) {
        loop {
            let deadline = self.flush_deadline();
            let flush_at = tokio::time::Instant::from_std(deadline.unwrap_or_else(Instant::now));
            let next = tokio::select! {
                biased;
                command = inbox.commands.recv() => Next::Command(command),
                Some(event) = inbox.events.recv() => Next::Event(event),
                Some(message) = inbox.internal.recv() => Next::Internal(message),
                Some(line) = inbox.logs.recv() => Next::Log(line),
                () = tokio::time::sleep_until(flush_at), if deadline.is_some() => Next::Flush,
            };
            match next {
                Next::Command(None | Some(Command::Shutdown)) => return,
                Next::Command(Some(command)) => self.handle(command),
                Next::Event(event) => self.on_engine_event(event),
                Next::Internal(message) => self.on_internal(message, &mut inbox.events),
                Next::Log(line) => self.sink.send(Update::Log(line)),
                Next::Flush => self.flush(),
            }
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Grab(request) => self.grab(request),
            Command::Accept { staged, start } => self.accept(&staged, start),
            Command::Discard { staged } => self.discard(&staged),
            Command::Start => self.start_downloads(&[]),
            Command::Stop => self.stop_downloads(),
            Command::Retry(ids) => self.retry(&ids),
            Command::Remove(ids) => self.remove(&ids),
            Command::ClearFinished => self.clear_finished(),
            Command::CheckAccount => self.check_account(),
            Command::ApplySettings(settings) => self.apply_settings(*settings),
            // Handled by the event loop.
            Command::Shutdown => {}
        }
    }

    fn on_internal(&mut self, message: Internal, events: &mut UnboundedReceiver<Event>) {
        match message {
            Internal::Grab(event) => self.on_grab_event(event),
            Internal::RunEnded { item_ids } => {
                // The run sent all its events before it returned: apply them first.
                while let Ok(event) = events.try_recv() {
                    self.on_engine_event(event);
                }
                self.run_ended(&item_ids);
            }
            Internal::AccountChecked { generation, result } => {
                self.account_checked(generation, result);
            }
        }
    }

    /// Cancels grabs and downloads, waits (bounded) for the runs to stop and saves.
    async fn shutdown(&mut self, inbox: &mut Inbox<'_>) {
        self.grab_cancel.cancel();
        self.download_cancel.cancel();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT;
        while self.active_runs > 0 {
            tokio::select! {
                biased;
                Some(event) = inbox.events.recv() => self.on_engine_event(event),
                Some(message) = inbox.internal.recv() => {
                    if let Internal::RunEnded { item_ids } = message {
                        while let Ok(event) = inbox.events.try_recv() {
                            self.on_engine_event(event);
                        }
                        self.run_ended(&item_ids);
                    }
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        self.save();
    }

    /// The queue for editing (copies it first if the window still holds the last snapshot).
    pub(super) fn queue_mut(&mut self) -> &mut Queue {
        Arc::make_mut(&mut self.queue)
    }

    /// Sends the current download list now.
    pub(super) fn send_queue(&mut self) {
        self.queue_dirty = false;
        self.last_queue_sent = Instant::now();
        self.sink.send(Update::Queue(Arc::clone(&self.queue)));
    }

    /// Saves `queue.json` now (errors go to the log).
    pub(super) fn save(&mut self) {
        self.save_dirty = false;
        self.last_save = Instant::now();
        if let Err(err) = self.store.save(&self.queue) {
            self.sink
                .error(format!("cannot save the download list: {err}"));
        }
    }

    /// After a user command changed the queue: save and show it right away.
    pub(super) fn queue_edited(&mut self) {
        self.save();
        self.send_queue();
    }

    /// Records a change made by the download engine; sent/saved when the
    /// throttle intervals allow.
    pub(super) fn queue_progressed(&mut self, persist: bool) {
        self.queue_dirty = true;
        self.save_dirty |= persist;
        self.flush();
    }

    /// Sends and saves what is due.
    fn flush(&mut self) {
        let now = Instant::now();
        if self.queue_dirty && now.duration_since(self.last_queue_sent) >= QUEUE_INTERVAL {
            self.send_queue();
        }
        if self.save_dirty && now.duration_since(self.last_save) >= SAVE_INTERVAL {
            self.save();
        }
    }

    /// When the next throttled send or save is due, if one is pending.
    fn flush_deadline(&self) -> Option<Instant> {
        let queue = self
            .queue_dirty
            .then(|| self.last_queue_sent + QUEUE_INTERVAL);
        let save = self.save_dirty.then(|| self.last_save + SAVE_INTERVAL);
        match (queue, save) {
            (Some(queue), Some(save)) => Some(queue.min(save)),
            (queue, save) => queue.or(save),
        }
    }

    /// Sends the effective settings.
    pub(super) fn send_settings(&self) {
        self.sink.send(Update::Settings {
            settings: Box::new(self.settings.clone()),
            config_path: self.config_path.clone(),
            default_out_dir: self.options.out_dir.clone(),
        });
    }

    /// Sends the LinkGrabber contents.
    pub(super) fn send_staged(&self) {
        self.sink.send(Update::Staged(self.staged.clone()));
    }

    /// Sends the LinkGrabber activity.
    pub(super) fn send_grabbing(&self) {
        self.sink.send(Update::Grabbing {
            pending_links: self.pending_links,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_lists_non_zero_counts() {
        let totals = RunTotals {
            done: 12,
            failed: 1,
            ..RunTotals::default()
        };
        assert_eq!(totals.summary(), "12 downloaded, 1 failed");
        let totals = RunTotals {
            done: 0,
            skipped: 2,
            failed: 0,
            stopped: 3,
        };
        assert_eq!(totals.summary(), "0 downloaded, 2 skipped, 3 stopped");
    }
}
