//! Command implementations: the LinkGrabber (`grab`), the download run
//! (`start`), `get` (both), queue maintenance, `cookies check` and `watch`.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use indicatif::{HumanBytes, HumanDuration};
use tokio_util::sync::CancellationToken;

use crate::archive::Archive;
use crate::cookies::{self, CookieJar};
use crate::download::{DownloadConfig, Downloader, Event, Job, Outcome, UrlRefresher};
use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::instagram::{self, Client, ClientConfig, CrawlOptions, Resolved};
use crate::links::{self, Target};
use crate::model::{ItemError, ItemStatus, MediaFile, Package};
use crate::naming;
use crate::progress::{self, Progress, Style};
use crate::queue::{ClearFilter, Queue, QueueStore, StatusCounts};
use crate::redact;

/// Fully resolved settings for one invocation (config file + flags + defaults).
#[derive(Debug, Clone)]
pub struct Options {
    pub cookies: Option<PathBuf>,
    pub data_dir: PathBuf,
    /// Absolute download root for new packages.
    pub out_dir: PathBuf,
    pub template: String,
    pub client: ClientConfig,
    pub download: DownloadConfig,
    pub crawl: CrawlOptions,
    /// Consult and update `archive.txt`.
    pub use_archive: bool,
    pub style: Style,
}

/// What a command achieved, for the exit code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Links or files that failed.
    pub failed: usize,
    /// The run was stopped with Ctrl+C.
    pub interrupted: bool,
}

impl Report {
    fn merge(&mut self, other: Report) {
        self.failed += other.failed;
        self.interrupted |= other.interrupted;
    }
}

/// How `igdl list` shows the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListView {
    Packages,
    Items,
    Failed,
    Json,
}

// ---------------------------------------------------------------------------
// Inputs

/// Collects links from arguments and links files (`-` reads stdin).
pub fn collect_links(args: &[String], inputs: &[PathBuf]) -> Result<Vec<String>> {
    let mut links: Vec<String> = Vec::new();
    for arg in args {
        let found = links::read_links(arg);
        if found.is_empty() {
            links.push(arg.trim().to_string());
        } else {
            links.extend(found);
        }
    }
    for input in inputs {
        let text = if input.as_os_str() == "-" {
            // Bytes, not `read_to_string`: `igdl get -i - < links.txt` in cmd.exe
            // passes a PowerShell-written UTF-16 file through unchanged.
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|err| Error::io("cannot read links from stdin", err))?;
            decode_links(&bytes)
        } else {
            read_links_file(input)?
        };
        links.extend(links::read_links(&text));
    }
    let mut seen = HashSet::new();
    links.retain(|link| !link.is_empty() && seen.insert(link.clone()));
    if links.is_empty() {
        // PowerShell treats a bare `@natgeo` as splatting an undefined variable
        // and passes nothing, so the user did type a link that never arrived.
        return Err(Error::Config(
            "no links given (pass URLs, usernames, or -i FILE; \
             in PowerShell quote '@natgeo' or leave out the @)"
                .into(),
        ));
    }
    Ok(links)
}

/// Reads a links file written by any Windows tool: UTF-8 (with or without a
/// BOM), UTF-16 from Windows PowerShell 5.1's `>`/`Out-File`, and anything else
/// (e.g. ANSI from `Set-Content`) lossily, since links are ASCII anyway.
fn read_links_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|err| Error::io(format!("cannot read {}", path.display()), err))?;
    Ok(decode_links(&bytes))
}

/// Decodes links text like [`fsutil::decode_text`], falling back to lossy UTF-8.
fn decode_links(bytes: &[u8]) -> String {
    fsutil::decode_text(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// Loads and validates the configured cookie file: `None` when none is
/// configured, else the jar and non-fatal warnings (e.g. "csrftoken missing").
pub fn load_cookies(path: Option<&Path>) -> Result<Option<(CookieJar, Vec<String>)>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let jar = cookies::load(path)?;
    match cookies::validate(&jar, now()) {
        Ok(report) => Ok(Some((jar, report.warnings))),
        Err(Error::Auth(message)) => Err(Error::Cookies(format!("{}: {message}", path.display()))),
        Err(err) => Err(err),
    }
}

/// Shown when no cookie file is configured.
pub const NO_COOKIES_WARNING: &str = "no cookies configured (--cookies FILE or IGDL_COOKIES); \
     Instagram shows most content only to logged-in sessions";

fn make_client(options: &Options) -> Result<Arc<Client>> {
    let jar = match load_cookies(options.cookies.as_deref())? {
        Some((jar, warnings)) => {
            for warning in &warnings {
                progress::note(&format!("warning: cookies: {warning}"));
            }
            Some(jar)
        }
        None => {
            progress::note(&format!("warning: {NO_COOKIES_WARNING}"));
            None
        }
    };
    Ok(Arc::new(Client::new(options.client.clone(), jar)?))
}

/// The archive in the data dir, or a disabled one when `use_archive` is off.
pub(crate) fn open_archive(options: &Options) -> Result<Archive> {
    if options.use_archive {
        Archive::open(&options.data_dir.join("archive.txt"))
    } else {
        Ok(Archive::disabled())
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// LinkGrabber

/// One link resolved into files with their planned output paths.
#[derive(Debug, Clone)]
pub(crate) struct Grabbed {
    pub(crate) link: String,
    pub(crate) name: String,
    pub(crate) files: Vec<(MediaFile, PathBuf)>,
    /// Files left out because the archive already has them.
    pub(crate) archived: usize,
    pub(crate) notes: Vec<String>,
}

/// Resolves every link. Per-link failures are reported and counted; an
/// authentication or rate-limit failure stops the remaining links (they would
/// fail the same way and only provoke Instagram further).
async fn grab_links(
    client: &Client,
    options: &Options,
    archive: &Archive,
    links: &[String],
    cancel: &CancellationToken,
    mut on_grabbed: impl FnMut(Grabbed) -> Result<()>,
) -> Result<Report> {
    let mut report = Report::default();
    let is_archived = |key: &str| archive.contains(key);
    for (position, link) in links.iter().enumerate() {
        if cancel.is_cancelled() {
            report.interrupted = true;
            break;
        }
        let target = match links::parse_link(link) {
            Ok(target) => target,
            Err(err) => {
                progress::note(&format!("error: {link}: {err}"));
                report.failed += 1;
                continue;
            }
        };
        progress::note(&format!("grabbing {}", target.canonical_url()));
        let resolved = tokio::select! {
            resolved = instagram::resolve(client, &target, &options.crawl, &is_archived) => resolved,
            () = cancel.cancelled() => Err(Error::Cancelled),
        };
        match resolved {
            Ok(resolved) => on_grabbed(plan(link, &target, resolved, options, archive)?)?,
            Err(Error::Cancelled) => {
                report.interrupted = true;
                break;
            }
            Err(err) => {
                progress::note(&format!("error: {link}: {err}"));
                report.failed += 1;
                if matches!(err.kind(), ErrorKind::Auth | ErrorKind::RateLimited) {
                    let rest = links.len() - position - 1;
                    if rest > 0 {
                        progress::note(&format!(
                            "stopping: {rest} remaining link(s) not processed"
                        ));
                        report.failed += rest;
                    }
                    hint(&err);
                    break;
                }
            }
        }
    }
    Ok(report)
}

/// Renders output paths for the resolved files and drops archived ones.
pub(crate) fn plan(
    link: &str,
    target: &Target,
    resolved: Resolved,
    options: &Options,
    archive: &Archive,
) -> Result<Grabbed> {
    let mut files = Vec::with_capacity(resolved.files.len());
    let mut used: HashSet<PathBuf> = HashSet::new();
    let mut archived = 0;
    for media in resolved.files {
        if archive.contains(&media.key) {
            archived += 1;
            continue;
        }
        let rel_path = distinct_path(naming::render(&options.template, &media)?, &mut used);
        files.push((media, rel_path));
    }
    let name = if resolved.name.is_empty() {
        target.describe()
    } else {
        resolved.name
    };
    Ok(Grabbed {
        link: link.to_string(),
        name,
        files,
        archived,
        notes: resolved.notes,
    })
}

/// Keeps planned paths unique within a batch (templates without `{index}` or
/// `{id}` can render the same name twice): `x.jpg`, `x (2).jpg`, …
///
/// Paths that differ only in letter case count as the same, because NTFS, SMB
/// shares and macOS store them as one file: the first keeps its casing, the
/// next one is numbered. `used` holds the case-folded keys (see [`fold_case`]).
fn distinct_path(path: PathBuf, used: &mut HashSet<PathBuf>) -> PathBuf {
    if used.insert(fold_case(&path)) {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut n = 2;
    loop {
        let candidate = path.with_file_name(format!("{stem} ({n}){ext}"));
        if used.insert(fold_case(&candidate)) {
            return candidate;
        }
        n += 1;
    }
}

/// Upper-cases a path the way case-insensitive filesystems compare names (NTFS
/// upper-cases both sides), so `Good.jpg` and `good.jpg` give the same key.
fn fold_case(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_uppercase())
}

fn print_grabbed(grabbed: &Grabbed, package: Option<u64>, already_queued: usize) {
    let id = package.map(|id| format!("#{id} ")).unwrap_or_default();
    let mut extra = Vec::new();
    if grabbed.archived > 0 {
        extra.push(format!("{} already downloaded", grabbed.archived));
    }
    if already_queued > 0 {
        extra.push(format!("{already_queued} already queued"));
    }
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(" ({})", extra.join(", "))
    };
    let added = grabbed.files.len() - already_queued;
    progress::note(&format!("+ {id}{}: {added} file(s){extra}", grabbed.name));
    for note in &grabbed.notes {
        progress::note(&format!("  note: {note}"));
    }
}

/// `igdl grab`: resolve links and add packages to the queue.
pub async fn grab(
    options: &Options,
    links: &[String],
    cancel: &CancellationToken,
) -> Result<Report> {
    naming::validate_template(&options.template)?;
    let store = QueueStore::open(&options.data_dir)?;
    let mut queue = store.load()?;
    let client = make_client(options)?;
    let archive = open_archive(options)?;
    let (report, added) = grab_into_queue(
        &client, options, &archive, &store, &mut queue, links, cancel,
    )
    .await?;
    if !added.is_empty() {
        progress::note("run `igdl start` to download the queue");
    }
    Ok(report)
}

/// Resolves links and stores each package as soon as it is resolved (so an
/// interrupted grab keeps what it found). Returns the new package ids.
async fn grab_into_queue(
    client: &Client,
    options: &Options,
    archive: &Archive,
    store: &QueueStore,
    queue: &mut Queue,
    links: &[String],
    cancel: &CancellationToken,
) -> Result<(Report, Vec<u64>)> {
    let mut added = Vec::new();
    let report = grab_links(client, options, archive, links, cancel, |grabbed| {
        let already_queued = grabbed
            .files
            .iter()
            .filter(|(media, _)| queue.contains_key(&media.key))
            .count();
        let package = queue.add_package(
            grabbed.name.clone(),
            grabbed.link.clone(),
            options.out_dir.clone(),
            grabbed.files.clone(),
        );
        print_grabbed(&grabbed, package, already_queued);
        if let Some(id) = package {
            added.push(id);
            store.save(queue)?;
        }
        Ok(())
    })
    .await?;
    Ok((report, added))
}

/// `igdl get --dry-run`: resolve and print the planned paths on stdout.
async fn dry_run(
    options: &Options,
    links: &[String],
    cancel: &CancellationToken,
) -> Result<Report> {
    naming::validate_template(&options.template)?;
    let client = make_client(options)?;
    let archive = open_archive(options)?;
    let mut total = 0;
    let report = grab_links(&client, options, &archive, links, cancel, |grabbed| {
        print_grabbed(&grabbed, None, 0);
        for (_, rel_path) in &grabbed.files {
            println!("{}", options.out_dir.join(rel_path).display());
        }
        total += grabbed.files.len();
        Ok(())
    })
    .await?;
    progress::note(&format!("dry run: {total} file(s) would be downloaded"));
    Ok(report)
}

// ---------------------------------------------------------------------------
// Downloads

/// Re-resolves expired CDN URLs through the API.
pub(crate) struct ClientRefresher(pub(crate) Arc<Client>);

impl UrlRefresher for ClientRefresher {
    fn refresh<'a>(&'a self, media: &'a MediaFile) -> BoxFuture<'a, Result<String>> {
        Box::pin(instagram::refresh_url(&self.0, media))
    }
}

/// Downloads the queued items of `packages` (all when empty), persisting state
/// as items finish.
async fn run_downloads(
    client: Arc<Client>,
    options: &Options,
    store: &QueueStore,
    queue: &mut Queue,
    archive: &mut Archive,
    packages: &[u64],
    cancel: &CancellationToken,
) -> Result<Report> {
    let interrupted = queue.reset_interrupted();
    if interrupted > 0 {
        tracing::info!("{interrupted} interrupted download(s) will be resumed");
    }
    let mut jobs = Vec::new();
    for item_id in queue.pending(packages) {
        let Some(out_dir) = queue.package_of(item_id).map(|p| p.out_dir.clone()) else {
            continue;
        };
        let Some(item) = queue.item_mut(item_id) else {
            continue;
        };
        if archive.contains(&item.media.key) {
            item.status = ItemStatus::Skipped;
            item.note = Some("already downloaded (archive)".into());
            continue;
        }
        jobs.push(Job {
            item_id,
            media: item.media.clone(),
            dest: out_dir.join(&item.rel_path),
        });
    }
    store.save(queue)?;
    if jobs.is_empty() {
        progress::note("nothing to download");
        return Ok(Report::default());
    }

    let downloader = Downloader::new(options.download.clone())?;
    let refresher: Arc<dyn UrlRefresher> = Arc::new(ClientRefresher(client));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut ui = Progress::new(options.style, jobs.len());
    let mut last_save = Instant::now();

    let run = downloader.run(jobs, Some(refresher), sender, cancel.clone());
    let consume = async {
        while let Some(event) = receiver.recv().await {
            ui.handle(&event);
            let effect = apply_event(queue, archive, &event);
            if let Some(path) = &effect.downloaded {
                ui.emit_path(path);
            }
            if let Some(warning) = &effect.warning {
                progress::note(&format!("warning: {warning}"));
            }
            if effect.finished && last_save.elapsed() >= Duration::from_secs(1) {
                if let Err(err) = store.save(queue) {
                    progress::note(&format!("warning: cannot save the queue: {err}"));
                }
                last_save = Instant::now();
            }
        }
    };
    tokio::join!(run, consume);

    let elapsed = ui.elapsed();
    let totals = ui.finish();
    store.save(queue)?;

    let mut summary = format!(
        "{} downloaded ({}), {} skipped, {} failed in {}",
        totals.done,
        HumanBytes(totals.bytes),
        totals.skipped,
        totals.failed,
        HumanDuration(elapsed)
    );
    if totals.cancelled > 0 {
        summary.push_str(&format!(
            "; {} interrupted (resume with `igdl start`)",
            totals.cancelled
        ));
    }
    progress::note(&summary);
    if totals.failed > 0 {
        progress::note("run `igdl list --failed` for details and `igdl retry` to try again");
    }
    Ok(Report {
        failed: totals.failed,
        interrupted: cancel.is_cancelled(),
    })
}

/// What applying one engine event changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EventEffect {
    /// An item finished (done, skipped, failed or cancelled).
    pub(crate) finished: bool,
    /// The path of a newly downloaded file.
    pub(crate) downloaded: Option<PathBuf>,
    /// A non-fatal problem worth showing (e.g. the archive could not be written).
    pub(crate) warning: Option<String>,
}

/// Applies one engine event to the queue (and archive).
pub(crate) fn apply_event(queue: &mut Queue, archive: &mut Archive, event: &Event) -> EventEffect {
    match event {
        Event::Started {
            item_id,
            total,
            resumed_from,
            ..
        } => {
            if let Some(item) = queue.item_mut(*item_id) {
                item.status = ItemStatus::Downloading;
                item.bytes_total = *total;
                item.bytes_done = *resumed_from;
            }
            EventEffect::default()
        }
        Event::Progress {
            item_id,
            done,
            total,
        } => {
            if let Some(item) = queue.item_mut(*item_id) {
                item.bytes_done = *done;
                item.bytes_total = *total;
            }
            EventEffect::default()
        }
        Event::Retrying { .. } => EventEffect::default(),
        Event::Refreshed { item_id, url } => {
            if let Some(item) = queue.item_mut(*item_id) {
                item.media.url = url.clone();
            }
            EventEffect::default()
        }
        Event::Finished { item_id, outcome } => {
            let mut effect = EventEffect {
                finished: true,
                ..EventEffect::default()
            };
            let Some(item) = queue.item_mut(*item_id) else {
                return effect;
            };
            match outcome {
                Outcome::Done { path, bytes } => {
                    item.status = ItemStatus::Done;
                    item.bytes_done = *bytes;
                    item.bytes_total = Some(*bytes);
                    item.path = Some(path.clone());
                    item.error = None;
                    effect.warning = record(archive, &item.media.key);
                    effect.downloaded = Some(path.clone());
                }
                Outcome::Skipped { path, reason } => {
                    item.status = ItemStatus::Skipped;
                    item.path = Some(path.clone());
                    item.note = Some(reason.clone());
                    item.error = None;
                    effect.warning = record(archive, &item.media.key);
                }
                Outcome::Failed { error } if error.kind == ErrorKind::Cancelled => {
                    item.status = ItemStatus::Queued;
                }
                Outcome::Failed { error } => {
                    item.status = ItemStatus::Failed;
                    item.error = Some(ItemError {
                        kind: error.kind,
                        message: redact::redact(&error.message),
                    });
                }
            }
            effect
        }
    }
}

fn record(archive: &mut Archive, key: &str) -> Option<String> {
    archive
        .insert(key)
        .err()
        .map(|err| format!("cannot update the archive: {err}"))
}

/// `igdl start`: download everything queued (or the given packages).
pub async fn start(
    options: &Options,
    packages: &[u64],
    cancel: &CancellationToken,
) -> Result<Report> {
    let store = QueueStore::open(&options.data_dir)?;
    let mut queue = store.load()?;
    for id in packages {
        if queue.package(*id).is_none() {
            return Err(Error::Config(format!("no package #{id} in the queue")));
        }
    }
    let client = make_client(options)?;
    let mut archive = open_archive(options)?;
    run_downloads(
        client,
        options,
        &store,
        &mut queue,
        &mut archive,
        packages,
        cancel,
    )
    .await
}

/// `igdl get`: grab, then download just those packages. Finished packages are
/// removed from the queue afterwards; failed items stay for `igdl retry`.
pub async fn get(
    options: &Options,
    links: &[String],
    dry: bool,
    cancel: &CancellationToken,
) -> Result<Report> {
    if dry {
        return dry_run(options, links, cancel).await;
    }
    naming::validate_template(&options.template)?;
    let store = QueueStore::open(&options.data_dir)?;
    let mut queue = store.load()?;
    let client = make_client(options)?;
    let mut archive = open_archive(options)?;
    let (mut report, added) = grab_into_queue(
        &client, options, &archive, &store, &mut queue, links, cancel,
    )
    .await?;
    if added.is_empty() || report.interrupted {
        if added.is_empty() && report.failed == 0 {
            progress::note("nothing new to download");
        }
        return Ok(report);
    }
    let downloaded = run_downloads(
        client,
        options,
        &store,
        &mut queue,
        &mut archive,
        &added,
        cancel,
    )
    .await?;
    report.merge(downloaded);
    let finished: Vec<u64> = added
        .iter()
        .copied()
        .filter(|id| {
            queue
                .package(*id)
                .is_some_and(|p| p.items.iter().all(|i| i.status.is_finished()))
        })
        .collect();
    if !finished.is_empty() {
        queue.remove(&finished);
        store.save(&queue)?;
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Queue maintenance

/// `igdl list`.
pub fn list(data_dir: &Path, view: ListView) -> Result<()> {
    let queue = QueueStore::read_unlocked(data_dir)?;
    if view == ListView::Json {
        let json = serde_json::to_string_pretty(&queue)
            .map_err(|err| Error::Config(format!("cannot serialise the queue: {err}")))?;
        println!("{json}");
        return Ok(());
    }
    if queue.packages.is_empty() {
        println!("queue is empty");
        return Ok(());
    }
    for package in &queue.packages {
        println!("{}", package_line(package));
        for item in &package.items {
            let show = match view {
                ListView::Items => true,
                ListView::Failed => item.status == ItemStatus::Failed,
                ListView::Packages | ListView::Json => {
                    matches!(item.status, ItemStatus::Failed | ItemStatus::Downloading)
                }
            };
            if !show {
                continue;
            }
            let size = item
                .bytes_total
                .map(|total| format!("  {}", HumanBytes(total)))
                .unwrap_or_default();
            let detail = match (&item.error, &item.note) {
                (Some(error), _) => format!("  {}: {}", error.kind, error.message),
                (None, Some(note)) => format!("  ({note})"),
                (None, None) => String::new(),
            };
            println!(
                "    #{:<5} {:<11} {}{size}{detail}",
                item.id,
                status_name(item.status),
                item.rel_path.display()
            );
        }
    }
    let counts = queue.counts();
    println!(
        "{} package(s), {} file(s): {}",
        queue.packages.len(),
        counts.total(),
        counts_text(&counts)
    );
    Ok(())
}

fn package_line(package: &Package) -> String {
    let counts = StatusCounts::of(&package.items);
    format!(
        "#{:<5} {}  [{}]  → {}",
        package.id,
        package.name,
        counts_text(&counts),
        package.out_dir.display()
    )
}

fn counts_text(counts: &StatusCounts) -> String {
    let parts = [
        (counts.done, "done"),
        (counts.skipped, "skipped"),
        (counts.downloading, "downloading"),
        (counts.queued, "queued"),
        (counts.failed, "failed"),
    ];
    let shown: Vec<String> = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    if shown.is_empty() {
        "empty".into()
    } else {
        shown.join(", ")
    }
}

fn status_name(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Queued => "queued",
        ItemStatus::Downloading => "downloading",
        ItemStatus::Done => "done",
        ItemStatus::Skipped => "skipped",
        ItemStatus::Failed => "failed",
    }
}

/// `igdl retry`.
pub fn retry(data_dir: &Path, ids: &[u64]) -> Result<()> {
    let store = QueueStore::open(data_dir)?;
    let mut queue = store.load()?;
    let changed = queue.retry(ids);
    store.save(&queue)?;
    progress::note(&format!("{changed} failed item(s) re-queued"));
    Ok(())
}

/// `igdl remove`.
pub fn remove(data_dir: &Path, ids: &[u64]) -> Result<()> {
    let store = QueueStore::open(data_dir)?;
    let mut queue = store.load()?;
    let removed = queue.remove(ids);
    store.save(&queue)?;
    progress::note(&format!(
        "{removed} item(s) removed from the queue (files on disk are kept)"
    ));
    Ok(())
}

/// `igdl clear`.
pub fn clear(data_dir: &Path, filter: ClearFilter) -> Result<()> {
    let store = QueueStore::open(data_dir)?;
    let mut queue = store.load()?;
    let removed = queue.clear(filter);
    store.save(&queue)?;
    progress::note(&format!("{removed} item(s) removed from the queue"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Cookies

/// `igdl cookies check`: report on the cookie file; `online` also asks
/// Instagram which account the session belongs to.
pub async fn cookies_check(options: &Options, online: bool) -> Result<()> {
    let path = options.cookies.as_deref().ok_or_else(|| {
        Error::Config("no cookie file given (--cookies FILE or IGDL_COOKIES)".into())
    })?;
    let jar = cookies::load(path)?;
    println!("file:      {}", path.display());
    println!("cookies:   {}", jar.names().join(", "));
    let report = match cookies::validate(&jar, now()) {
        Ok(report) => report,
        Err(Error::Auth(message)) => return Err(Error::Cookies(message)),
        Err(err) => return Err(err),
    };
    match report.session_expires {
        Some(expires) => println!("sessionid: expires {}", format_date(expires)),
        None => println!("sessionid: session cookie (no expiry date)"),
    }
    if let Some(user_id) = &report.user_id {
        println!("user id:   {user_id}");
    }
    for warning in &report.warnings {
        println!("warning:   {warning}");
    }
    if online {
        let client = Client::new(options.client.clone(), Some(jar))?;
        let user = client.current_user().await?;
        println!("logged in: @{} (id {})", user.username, user.id);
    } else {
        println!("ok (use --online to verify the session with Instagram)");
    }
    Ok(())
}

fn format_date(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

// ---------------------------------------------------------------------------
// Clipboard observer

/// `igdl watch`: queue Instagram links copied to the clipboard; with `start`,
/// download each batch right away.
pub async fn watch(
    options: &Options,
    start: bool,
    interval: Duration,
    cancel: &CancellationToken,
) -> Result<Report> {
    naming::validate_template(&options.template)?;
    let store = QueueStore::open(&options.data_dir)?;
    let mut queue = store.load()?;
    let client = make_client(options)?;
    let mut archive = open_archive(options)?;
    let mut clipboard = crate::watch::Clipboard::open()?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut report = Report::default();
    progress::note("watching the clipboard for Instagram links (Ctrl+C to stop)");
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(interval) => {}
        }
        let links: Vec<String> = clipboard
            .new_links()
            .into_iter()
            .filter(|link| seen.insert(link.clone()))
            .collect();
        if links.is_empty() {
            continue;
        }
        let (grabbed, added) = grab_into_queue(
            &client, options, &archive, &store, &mut queue, &links, cancel,
        )
        .await?;
        report.merge(grabbed);
        if start && !added.is_empty() && !cancel.is_cancelled() {
            let downloaded = run_downloads(
                client.clone(),
                options,
                &store,
                &mut queue,
                &mut archive,
                &added,
                cancel,
            )
            .await?;
            report.merge(downloaded);
        }
    }
    report.interrupted = false;
    Ok(report)
}

/// Prints an actionable hint for errors users can fix.
pub fn hint(err: &Error) {
    let text = match err.kind() {
        ErrorKind::Auth | ErrorKind::Cookies => {
            "hint: log in to instagram.com in your browser, export fresh cookies, and pass them with --cookies (check with `igdl cookies check --online`)"
        }
        ErrorKind::RateLimited => {
            "hint: Instagram is rate limiting this account; wait a while and use a larger --api-delay"
        }
        ErrorKind::Private => "hint: the cookies' account must follow this profile",
        _ => return,
    };
    progress::note(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_paths_get_numbered() {
        let mut used = HashSet::new();
        let a = distinct_path(PathBuf::from("u/x.jpg"), &mut used);
        let b = distinct_path(PathBuf::from("u/x.jpg"), &mut used);
        let c = distinct_path(PathBuf::from("u/x.jpg"), &mut used);
        assert_eq!(a, PathBuf::from("u/x.jpg"));
        assert_eq!(b, PathBuf::from("u/x (2).jpg"));
        assert_eq!(c, PathBuf::from("u/x (3).jpg"));
    }

    #[test]
    fn distinct_paths_ignore_letter_case() {
        let mut used = HashSet::new();
        let first = distinct_path(PathBuf::from("u/Good morning.jpg"), &mut used);
        let second = distinct_path(PathBuf::from("u/good morning.jpg"), &mut used);
        let third = distinct_path(PathBuf::from("u/GOOD MORNING.jpg"), &mut used);
        assert_eq!(first, PathBuf::from("u/Good morning.jpg"));
        assert_eq!(second, PathBuf::from("u/good morning (2).jpg"));
        assert_eq!(third, PathBuf::from("u/GOOD MORNING (3).jpg"));

        // Folder names count too: `Travel` and `travel` are one folder on NTFS.
        let a = distinct_path(PathBuf::from("u/Travel/01.jpg"), &mut used);
        let b = distinct_path(PathBuf::from("u/travel/01.jpg"), &mut used);
        let c = distinct_path(PathBuf::from("u/travel/02.jpg"), &mut used);
        assert_eq!(a, PathBuf::from("u/Travel/01.jpg"));
        assert_eq!(b, PathBuf::from("u/travel/01 (2).jpg"));
        assert_eq!(c, PathBuf::from("u/travel/02.jpg"));

        // A numbered name that is already taken in another casing moves on.
        let d = distinct_path(PathBuf::from("u/X.jpg"), &mut used);
        let e = distinct_path(PathBuf::from("u/x (2).JPG"), &mut used);
        let f = distinct_path(PathBuf::from("u/x.jpg"), &mut used);
        assert_eq!(d, PathBuf::from("u/X.jpg"));
        assert_eq!(e, PathBuf::from("u/x (2).JPG"));
        assert_eq!(f, PathBuf::from("u/x (3).jpg"));
    }

    #[test]
    fn links_files_may_be_utf16_or_ansi() {
        let dir = tempfile::tempdir().unwrap();
        let utf16 = dir.path().join("links16.txt");
        let text = "https://www.instagram.com/p/AAA/\r\nnatgeo\r\n";
        let bytes: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&utf16, bytes).unwrap();
        // Windows-1252 "é" in a comment is not UTF-8; the links still load.
        let ansi = dir.path().join("ansi.txt");
        std::fs::write(&ansi, b"# caf\xE9\r\nhttps://www.instagram.com/p/BBB/\r\n").unwrap();

        let links = collect_links(&[], &[utf16, ansi]).unwrap();
        assert_eq!(
            links,
            [
                "https://www.instagram.com/p/AAA/",
                "natgeo",
                "https://www.instagram.com/p/BBB/"
            ]
        );
    }

    #[test]
    fn piped_links_are_decoded_like_files() {
        // What `igdl get -i - < links.txt` in cmd.exe receives from a file
        // written by Windows PowerShell 5.1: UTF-16LE with a BOM.
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("natgeo\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_links(&utf16), "natgeo\r\n");
        assert_eq!(decode_links(b"\xEF\xBB\xBFnatgeo\n"), "natgeo\n");
        assert_eq!(decode_links(b"caf\xE9 natgeo"), "caf\u{FFFD} natgeo");
    }

    #[test]
    fn missing_links_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");
        let err = collect_links(&[], &[missing]).unwrap_err();
        assert!(err.to_string().contains("nope.txt"), "{err}");
    }

    #[test]
    fn no_links_mentions_powershell_quoting() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "# nothing here\n").unwrap();
        for result in [
            collect_links(&[], &[]),
            collect_links(&["  ".to_string()], &[empty]),
        ] {
            let message = match result {
                Err(Error::Config(message)) => message,
                other => panic!("expected a config error, got {other:?}"),
            };
            assert!(message.starts_with("no links given"), "{message}");
            assert!(message.contains("PowerShell quote '@natgeo'"), "{message}");
        }
    }
}
