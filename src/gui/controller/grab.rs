//! The LinkGrabber: a worker task resolves links one at a time; the controller
//! plans their output paths and stages them until they are accepted.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::runner::{
    ClientSlot, Controller, Internal, SharedArchive, current_client, lock_archive,
};
use crate::app::{self, Options};
use crate::cli::{self, CrawlArgs, DownloadArgs, KindArg, SectionArg};
use crate::error::{Error, ErrorKind, Result};
use crate::gui::protocol::{AccountStatus, GrabRequest, StagedFile, StagedPackage};
use crate::instagram::{self, Resolved};
use crate::links::{self, Target};
use crate::naming;
use crate::redact;

/// One `Grab` request for the worker.
pub(super) struct GrabJob {
    pub(super) links: Vec<String>,
    pub(super) options: Arc<Options>,
}

/// What the worker reports back.
pub(super) enum GrabEvent {
    /// One link is finished.
    Link { link: String, outcome: LinkOutcome },
    /// The rest of a request was dropped after an authentication or rate-limit failure.
    Skipped { count: usize },
}

/// How one link ended.
pub(super) enum LinkOutcome {
    Resolved {
        target: Target,
        resolved: Resolved,
        options: Arc<Options>,
    },
    Failed(Error),
}

/// Starts the worker that processes grab requests one after the other.
pub(super) fn spawn_worker(
    client: ClientSlot,
    archive: SharedArchive,
    results: UnboundedSender<Internal>,
    cancel: CancellationToken,
) -> UnboundedSender<GrabJob> {
    let (jobs_tx, mut jobs) = mpsc::unbounded_channel::<GrabJob>();
    tokio::spawn(async move {
        loop {
            let job = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                job = jobs.recv() => job,
            };
            let Some(job) = job else { break };
            grab_links(job, &client, &archive, &results, &cancel).await;
        }
    });
    jobs_tx
}

/// Resolves the links of one request, reporting each as it finishes. An
/// authentication or rate-limit failure stops the request: the remaining links
/// would fail the same way and only provoke Instagram further.
async fn grab_links(
    job: GrabJob,
    client: &ClientSlot,
    archive: &SharedArchive,
    results: &UnboundedSender<Internal>,
    cancel: &CancellationToken,
) {
    let archive = Arc::clone(archive);
    let is_archived = move |key: &str| lock_archive(&archive).contains(key);
    let total = job.links.len();
    for (position, link) in job.links.into_iter().enumerate() {
        if cancel.is_cancelled() {
            return;
        }
        let outcome = match links::parse_link(&link) {
            Err(err) => LinkOutcome::Failed(err),
            Ok(target) => {
                let client = current_client(client);
                let resolved = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    resolved = instagram::resolve(&client, &target, &job.options.crawl, &is_archived) => resolved,
                };
                match resolved {
                    Ok(resolved) => LinkOutcome::Resolved {
                        target,
                        resolved,
                        options: Arc::clone(&job.options),
                    },
                    Err(err) => LinkOutcome::Failed(err),
                }
            }
        };
        let stop = matches!(
            &outcome,
            LinkOutcome::Failed(err) if matches!(err.kind(), ErrorKind::Auth | ErrorKind::RateLimited)
        );
        let _ = results.send(Internal::Grab(GrabEvent::Link { link, outcome }));
        if stop {
            let count = total - position - 1;
            if count > 0 {
                let _ = results.send(Internal::Grab(GrabEvent::Skipped { count }));
            }
            return;
        }
    }
}

/// `Some(text)` unless `text` is blank.
fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// What to tell the user to do about a grab failure.
fn hint(kind: ErrorKind) -> Option<&'static str> {
    match kind {
        ErrorKind::Auth | ErrorKind::Cookies => Some(
            "log in to instagram.com in your browser, export fresh cookies and choose the file in Settings",
        ),
        ErrorKind::RateLimited => Some(
            "Instagram is rate limiting this account; wait a while and raise the API delay in Settings",
        ),
        ErrorKind::Private => Some("the cookies' account must follow this profile"),
        _ => None,
    }
}

impl Controller {
    /// `Grab`: extract the links and queue them for the worker.
    pub(super) fn grab(&mut self, request: GrabRequest) {
        let mut seen = HashSet::new();
        let links: Vec<String> = links::read_links(&request.text)
            .into_iter()
            .filter(|link| seen.insert(link.clone()))
            .collect();
        if links.is_empty() {
            self.sink.warn("no links found");
            return;
        }
        let options = match self.request_options(&request) {
            Ok(options) => options,
            Err(err) => {
                self.sink.error(format!("cannot grab: {err}"));
                return;
            }
        };
        let count = links.len();
        let job = GrabJob {
            links,
            options: Arc::new(options),
        };
        if self.grab_jobs.send(job).is_err() {
            self.sink.error("cannot grab: the LinkGrabber stopped");
            return;
        }
        self.pending_links += count;
        self.send_grabbing();
    }

    /// The options of one grab request: the base settings with the request's
    /// crawl filters, template and download folder.
    fn request_options(&self, request: &GrabRequest) -> Result<Options> {
        let crawl = CrawlArgs {
            // No folder in the request: the window's default, not the current directory.
            out: Some(if request.out_dir.as_os_str().is_empty() {
                self.options.out_dir.clone()
            } else {
                request.out_dir.clone()
            }),
            include: request
                .sections
                .iter()
                .copied()
                .map(SectionArg::from)
                .collect(),
            limit: request.limit,
            since: non_empty(&request.since),
            until: non_empty(&request.until),
            only: request.only.map(KindArg::from),
            update: request.update,
            template: non_empty(&request.template),
            no_archive: !self.options.use_archive,
        };
        let mut options = cli::resolve_options(
            &self.global,
            &self.settings,
            Some(&crawl),
            &DownloadArgs::default(),
        )?;
        naming::validate_template(&options.template)?;
        // The queue and archive stay where they were opened.
        options.data_dir = self.options.data_dir.clone();
        Ok(options)
    }

    /// A message from the grab worker.
    pub(super) fn on_grab_event(&mut self, event: GrabEvent) {
        match event {
            GrabEvent::Link { link, outcome } => {
                self.pending_links = self.pending_links.saturating_sub(1);
                match outcome {
                    LinkOutcome::Resolved {
                        target,
                        resolved,
                        options,
                    } => self.stage(link, &target, resolved, &options),
                    LinkOutcome::Failed(err) => self.grab_failed(&link, &err),
                }
            }
            GrabEvent::Skipped { count } => {
                self.pending_links = self.pending_links.saturating_sub(count);
                self.sink.warn(format!(
                    "stopped grabbing: {count} remaining link(s) skipped"
                ));
            }
        }
        self.send_grabbing();
    }

    fn grab_failed(&mut self, link: &str, err: &Error) {
        self.sink.error(format!("{link}: {err}"));
        if err.kind() == ErrorKind::Auth {
            self.set_account(AccountStatus::Invalid(redact::redact(&err.to_string())));
        }
        if let Some(hint) = hint(err.kind()) {
            self.sink.info(format!("hint: {hint}"));
        }
    }

    /// Plans output paths for a resolved link and adds it to the LinkGrabber.
    fn stage(&mut self, link: String, target: &Target, resolved: Resolved, options: &Options) {
        let planned = {
            let archive = lock_archive(&self.archive);
            app::plan(&link, target, resolved, options, &archive)
        };
        let grabbed = match planned {
            Ok(grabbed) => grabbed,
            Err(err) => {
                self.sink.error(format!("{link}: {err}"));
                return;
            }
        };
        let queued: HashSet<&str> = self
            .queue
            .items()
            .map(|item| item.media.key.as_str())
            .collect();
        let already_queued = grabbed
            .files
            .iter()
            .filter(|(media, _)| queued.contains(media.key.as_str()))
            .count();

        let mut extra = Vec::new();
        if grabbed.archived > 0 {
            extra.push(format!("{} already downloaded", grabbed.archived));
        }
        if already_queued > 0 {
            extra.push(format!("{already_queued} already in the download list"));
        }
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({})", extra.join(", "))
        };
        self.sink.info(format!(
            "grabbed {}: {} file(s){extra}",
            grabbed.name,
            grabbed.files.len()
        ));
        for note in &grabbed.notes {
            self.sink.info(format!("{}: {note}", grabbed.name));
        }
        if grabbed.files.is_empty() {
            return;
        }

        let id = self.next_staged_id;
        self.next_staged_id += 1;
        self.staged.push(StagedPackage {
            id,
            name: grabbed.name,
            source: grabbed.link,
            out_dir: options.out_dir.clone(),
            files: grabbed
                .files
                .into_iter()
                .map(|(media, rel_path)| StagedFile { media, rel_path })
                .collect(),
            archived: grabbed.archived,
            already_queued,
            notes: grabbed.notes,
        });
        self.send_staged();
    }

    /// `Accept`: move staged packages (empty = all) into the download list.
    pub(super) fn accept(&mut self, ids: &[u64], start: bool) {
        let (chosen, kept): (Vec<StagedPackage>, Vec<StagedPackage>) =
            std::mem::take(&mut self.staged)
                .into_iter()
                .partition(|package| ids.is_empty() || ids.contains(&package.id));
        self.staged = kept;
        if chosen.is_empty() {
            self.sink.info("nothing to add");
            return;
        }
        let mut added = Vec::new();
        for package in chosen {
            let name = package.name;
            let files = package
                .files
                .into_iter()
                .map(|file| (file.media, file.rel_path))
                .collect();
            let queue = self.queue_mut();
            match queue.add_package(name.clone(), package.source, package.out_dir, files) {
                Some(id) => {
                    let count = queue.package(id).map_or(0, |p| p.items.len());
                    added.push(id);
                    self.sink.info(format!("added {name}: {count} file(s)"));
                }
                None => self.sink.info(format!("nothing new in {name}")),
            }
        }
        self.queue_edited();
        self.send_staged();
        if start && !added.is_empty() {
            self.start_downloads(&added);
        }
    }

    /// `Discard`: drop staged packages (empty = all).
    pub(super) fn discard(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            self.staged.clear();
        } else {
            self.staged.retain(|package| !ids.contains(&package.id));
        }
        self.send_staged();
    }
}
