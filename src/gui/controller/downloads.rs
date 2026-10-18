//! Scheduling downloads, applying engine events, and queue maintenance commands.

use std::collections::HashSet;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::runner::{Controller, Internal, RunTotals, current_client, lock_archive};
use crate::app::{self, ClientRefresher};
use crate::download::{Event, Job, Outcome, UrlRefresher};
use crate::error::ErrorKind;
use crate::gui::protocol::Update;
use crate::model::ItemStatus;
use crate::queue::ClearFilter;

/// Note on items skipped because the archive has them.
const ARCHIVED_NOTE: &str = "already downloaded (archive)";

impl Controller {
    /// Downloads the queued items of `packages` (all when empty) that are not
    /// already running, as one engine run.
    pub(super) fn start_downloads(&mut self, packages: &[u64]) {
        let mut jobs = Vec::new();
        let mut archived = HashSet::new();
        {
            let archive = lock_archive(&self.archive);
            let chosen = self
                .queue
                .packages
                .iter()
                .filter(|package| packages.is_empty() || packages.contains(&package.id));
            for package in chosen {
                for item in &package.items {
                    if item.status != ItemStatus::Queued || self.scheduled.contains(&item.id) {
                        continue;
                    }
                    if archive.contains(&item.media.key) {
                        archived.insert(item.id);
                        continue;
                    }
                    jobs.push(Job {
                        item_id: item.id,
                        media: item.media.clone(),
                        dest: package.out_dir.join(&item.rel_path),
                    });
                }
            }
        }
        if !archived.is_empty() {
            self.skip_archived(&archived);
        }
        if jobs.is_empty() {
            if archived.is_empty() {
                self.sink.info("nothing to download");
            }
            return;
        }
        self.launch(jobs);
    }

    /// Marks items the archive already has as skipped.
    fn skip_archived(&mut self, ids: &HashSet<u64>) {
        let queue = self.queue_mut();
        for package in &mut queue.packages {
            for item in package
                .items
                .iter_mut()
                .filter(|item| ids.contains(&item.id))
            {
                item.status = ItemStatus::Skipped;
                item.note = Some(ARCHIVED_NOTE.to_string());
            }
        }
        self.sink
            .info(format!("{} file(s) skipped: {ARCHIVED_NOTE}", ids.len()));
        self.queue_edited();
    }

    /// Starts one engine run for `jobs`; it reports back with `RunEnded`.
    fn launch(&mut self, jobs: Vec<Job>) {
        let item_ids: Vec<u64> = jobs.iter().map(|job| job.item_id).collect();
        self.scheduled.extend(item_ids.iter().copied());
        if self.active_runs == 0 {
            self.totals = RunTotals::default();
            self.sink.send(Update::Downloading(true));
        }
        self.active_runs += 1;
        self.sink
            .info(format!("downloading {} file(s)", item_ids.len()));

        let downloader = Arc::clone(&self.downloader);
        let refresher: Arc<dyn UrlRefresher> =
            Arc::new(ClientRefresher(current_client(&self.client)));
        let events = self.events_tx.clone();
        let cancel = self.download_cancel.clone();
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            downloader.run(jobs, Some(refresher), events, cancel).await;
            let _ = done.send(Internal::RunEnded { item_ids });
        });
    }

    /// Applies one engine event to the queue and archive and tells the window.
    pub(super) fn on_engine_event(&mut self, event: Event) {
        let effect = {
            let mut archive = lock_archive(&self.archive);
            app::apply_event(Arc::make_mut(&mut self.queue), &mut archive, &event)
        };
        let mut persist = true;
        match &event {
            Event::Started {
                item_id,
                total,
                resumed_from,
                ..
            } => self.sink.send(Update::Progress {
                item_id: *item_id,
                done: *resumed_from,
                total: *total,
            }),
            Event::Progress {
                item_id,
                done,
                total,
            } => {
                // Byte counts are not worth a save of their own.
                persist = false;
                self.sink.send(Update::Progress {
                    item_id: *item_id,
                    done: *done,
                    total: *total,
                });
            }
            Event::Retrying {
                item_id,
                attempt,
                delay,
                reason,
            } => {
                persist = false;
                self.sink.warn(format!(
                    "{}: {reason}; retrying in {}s (attempt {attempt})",
                    self.item_name(*item_id),
                    delay.as_secs().max(1)
                ));
            }
            Event::Refreshed { item_id, .. } => {
                self.sink.info(format!(
                    "{}: refreshed the expired link",
                    self.item_name(*item_id)
                ));
            }
            Event::Finished { item_id, outcome } => {
                self.scheduled.remove(item_id);
                match outcome {
                    Outcome::Done { .. } => self.totals.done += 1,
                    Outcome::Skipped { .. } => self.totals.skipped += 1,
                    Outcome::Failed { error } if error.kind == ErrorKind::Cancelled => {
                        self.totals.stopped += 1;
                    }
                    Outcome::Failed { error } => {
                        self.totals.failed += 1;
                        self.sink
                            .error(format!("{}: {}", self.item_name(*item_id), error.message));
                    }
                }
            }
        }
        if let Some(warning) = &effect.warning {
            self.sink.warn(warning);
        }
        self.queue_progressed(persist);
    }

    /// The item's output path for log lines (or its id if it is gone).
    fn item_name(&self, item_id: u64) -> String {
        self.queue
            .item(item_id)
            .map(|item| item.rel_path.display().to_string())
            .unwrap_or_else(|| format!("item #{item_id}"))
    }

    /// One engine run returned (its events are applied already).
    pub(super) fn run_ended(&mut self, item_ids: &[u64]) {
        // Jobs that never started (the run was stopped) have no `Finished` event.
        for item_id in item_ids {
            self.scheduled.remove(item_id);
        }
        self.active_runs = self.active_runs.saturating_sub(1);
        self.save();
        if self.active_runs > 0 {
            return;
        }
        self.send_queue();
        if let Some(downloader) = self.pending_downloader.take() {
            self.downloader = Arc::new(downloader);
        }
        self.sink.send(Update::Downloading(false));
        self.sink.info(self.totals.summary());
    }

    /// `Stop`: cancel the running downloads (their items return to queued and
    /// partial files are kept). Grabs keep running.
    pub(super) fn stop_downloads(&mut self) {
        if self.active_runs == 0 {
            self.sink.info("nothing is downloading");
            return;
        }
        self.download_cancel.cancel();
        self.download_cancel = CancellationToken::new();
        self.sink
            .info("stopping the downloads (partial files are kept)");
    }

    /// `Retry`: re-queue failed items among `ids` (empty = all).
    pub(super) fn retry(&mut self, ids: &[u64]) {
        let changed = self.queue_mut().retry(ids);
        self.queue_edited();
        self.sink
            .info(format!("{changed} failed item(s) re-queued"));
    }

    /// `Remove`: remove packages/items, except those with running downloads.
    pub(super) fn remove(&mut self, ids: &[u64]) {
        let (refused, allowed): (Vec<u64>, Vec<u64>) =
            ids.iter().copied().partition(|id| self.is_running(*id));
        if !refused.is_empty() {
            self.sink
                .warn("stop the downloads before removing running items");
        }
        if allowed.is_empty() {
            return;
        }
        let removed = self.queue_mut().remove(&allowed);
        self.queue_edited();
        self.sink.info(format!(
            "{removed} item(s) removed from the download list (files on disk are kept)"
        ));
    }

    /// Whether `id` is a scheduled item or a package containing one.
    fn is_running(&self, id: u64) -> bool {
        self.scheduled.contains(&id)
            || self.queue.package(id).is_some_and(|package| {
                package
                    .items
                    .iter()
                    .any(|item| self.scheduled.contains(&item.id))
            })
    }

    /// `ClearFinished`: remove packages whose items are all done or skipped.
    pub(super) fn clear_finished(&mut self) {
        let removed = self.queue_mut().clear(ClearFilter::Finished);
        self.queue_edited();
        self.sink.info(format!(
            "{removed} finished item(s) removed from the download list"
        ));
    }
}
