//! The window's view-model: everything the controller has reported (see
//! [`Update`]), plus the numbers derived from it (speeds, ETAs, per-package
//! totals). Pure data, no egui types, so it is unit-tested on its own.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::protocol::{AccountStatus, LogLevel, LogLine, StagedPackage, Update};
use crate::config::Settings;
use crate::model::{Item, ItemStatus, MediaKind, Package};
use crate::queue::{Queue, StatusCounts};

/// Most log lines kept for the Log tab; older ones are dropped.
pub const LOG_CAPACITY: usize = 2000;

/// Time constant of the speed smoothing: a change in throughput is reflected to
/// ~63 % after this long.
const SPEED_TIME_CONSTANT_SECS: f64 = 2.0;

/// Progress samples closer together than this are merged before computing a
/// rate (byte counts arrive per network chunk, far too often to be meaningful).
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// An item whose byte count has not moved for this long counts as stalled (0 B/s).
const STALE_AFTER: Duration = Duration::from_secs(3);

/// After a queue snapshot, a still-`Queued` item keeps its live progress for this
/// long: the throttled snapshot may predate the item's start.
const QUEUED_GRACE: Duration = Duration::from_secs(2);

/// Live byte counts of one running item, with a smoothed speed estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveProgress {
    /// Bytes on disk so far (including a resumed part).
    pub done: u64,
    /// Expected size, when the server announced it.
    pub total: Option<u64>,
    /// Smoothed bytes per second; `None` until two samples were far enough apart.
    speed: Option<f64>,
    /// Start of the current rate sample.
    sample_at: Instant,
    sample_done: u64,
    /// When the last `Progress` update arrived.
    updated_at: Instant,
}

impl LiveProgress {
    fn new(done: u64, total: Option<u64>, now: Instant) -> Self {
        LiveProgress {
            done,
            total,
            speed: None,
            sample_at: now,
            sample_done: done,
            updated_at: now,
        }
    }

    /// Folds a new byte count into the estimate (time-aware exponential smoothing).
    fn record(&mut self, done: u64, total: Option<u64>, now: Instant) {
        self.done = done;
        self.total = total;
        self.updated_at = now;
        if done < self.sample_done {
            // The transfer restarted from scratch (e.g. the server ignored Range).
            self.sample_done = done;
            self.sample_at = now;
            return;
        }
        let elapsed = now.saturating_duration_since(self.sample_at);
        if elapsed < MIN_SAMPLE_INTERVAL {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let rate = (done - self.sample_done) as f64 / secs;
        let alpha = 1.0 - (-secs / SPEED_TIME_CONSTANT_SECS).exp();
        self.speed = Some(match self.speed {
            Some(previous) => previous + alpha * (rate - previous),
            None => rate,
        });
        self.sample_at = now;
        self.sample_done = done;
    }

    /// Bytes per second at `now`: `None` while unknown, `0` once stalled.
    pub fn speed(&self, now: Instant) -> Option<f64> {
        if now.saturating_duration_since(self.updated_at) > STALE_AFTER {
            return Some(0.0);
        }
        self.speed
    }
}

/// Totals over one package's items.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackageAggregate {
    pub counts: StatusCounts,
    /// Bytes downloaded over the items that are not skipped.
    pub bytes_done: u64,
    /// Sum of the known sizes of the items that are not skipped.
    pub bytes_total: u64,
    /// Every item that is not skipped has a known size.
    pub sizes_known: bool,
    /// Combined speed of the package's running items (bytes per second).
    pub speed: f64,
    /// Mean per-file completion in `0..=1` (finished files count as 1).
    pub fraction: f32,
}

impl PackageAggregate {
    /// Number of files in the package.
    pub fn files_total(&self) -> usize {
        self.counts.total()
    }

    /// Files that are done or skipped.
    pub fn files_finished(&self) -> usize {
        self.counts.done + self.counts.skipped
    }

    /// Time left at the current speed, when every size is known.
    pub fn eta(&self) -> Option<Duration> {
        if !self.sizes_known {
            return None;
        }
        eta(self.bytes_total.saturating_sub(self.bytes_done), self.speed)
    }

    /// One status summarising the package.
    pub fn status(&self) -> PackageStatus {
        let counts = &self.counts;
        if counts.downloading > 0 {
            PackageStatus::Downloading
        } else if counts.queued > 0 {
            PackageStatus::Queued
        } else if counts.failed > 0 {
            PackageStatus::Failed(counts.failed)
        } else if counts.done > 0 {
            PackageStatus::Done
        } else if counts.skipped > 0 {
            PackageStatus::Skipped
        } else {
            PackageStatus::Empty
        }
    }
}

/// Summary status of a package, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageStatus {
    Downloading,
    Queued,
    /// Nothing left to do, but this many items failed.
    Failed(usize),
    Done,
    Skipped,
    Empty,
}

/// Everything the window knows, fed by [`ViewState::apply`].
#[derive(Debug, Clone)]
pub struct ViewState {
    /// Last download-list snapshot.
    pub queue: Arc<Queue>,
    progress: HashMap<u64, LiveProgress>,
    /// LinkGrabber contents.
    pub staged: Vec<StagedPackage>,
    /// Links still being resolved (0 = idle).
    pub grabbing: usize,
    /// Whether downloads are running.
    pub downloading: bool,
    /// `None` until the controller reports it.
    pub account: Option<AccountStatus>,
    /// Effective settings.
    pub settings: Settings,
    /// Bumped on every `Settings` update (0 = none received yet), so forms can
    /// tell when to reload.
    pub settings_generation: u64,
    /// Where settings are saved (`None`: they cannot be).
    pub config_path: Option<PathBuf>,
    /// Download root used when the settings name none.
    pub default_out_dir: PathBuf,
    log: VecDeque<LogLine>,
    unseen_errors: usize,
    /// Why the controller cannot work, if it cannot.
    pub fatal: Option<String>,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            queue: Arc::new(Queue::default()),
            progress: HashMap::new(),
            staged: Vec::new(),
            grabbing: 0,
            downloading: false,
            account: None,
            settings: Settings::default(),
            settings_generation: 0,
            config_path: None,
            default_out_dir: PathBuf::new(),
            log: VecDeque::new(),
            unseen_errors: 0,
            fatal: None,
        }
    }
}

impl ViewState {
    /// Folds one controller update into the state; `now` drives the speed estimate.
    pub fn apply(&mut self, update: Update, now: Instant) {
        match update {
            Update::Settings {
                settings,
                config_path,
                default_out_dir,
            } => {
                self.settings = *settings;
                self.config_path = config_path;
                self.default_out_dir = default_out_dir;
                self.settings_generation += 1;
            }
            Update::Queue(queue) => {
                self.queue = queue;
                self.prune_progress(now);
            }
            Update::Progress {
                item_id,
                done,
                total,
            } => match self.progress.get_mut(&item_id) {
                Some(live) => live.record(done, total, now),
                None => {
                    self.progress
                        .insert(item_id, LiveProgress::new(done, total, now));
                }
            },
            Update::Staged(staged) => self.staged = staged,
            Update::Grabbing { pending_links } => self.grabbing = pending_links,
            Update::Downloading(running) => {
                self.downloading = running;
                if !running {
                    self.progress.clear();
                }
            }
            Update::Account(status) => self.account = Some(status),
            Update::Log(line) => self.push_log(line),
            Update::Fatal(message) => self.fatal = Some(message),
        }
    }

    /// Keeps live progress only for items that are (or may just have become) running.
    fn prune_progress(&mut self, now: Instant) {
        let queue = &self.queue;
        self.progress
            .retain(|id, live| match queue.item(*id).map(|item| item.status) {
                Some(ItemStatus::Downloading) => true,
                Some(ItemStatus::Queued) => {
                    now.saturating_duration_since(live.updated_at) < QUEUED_GRACE
                }
                _ => false,
            });
    }

    /// Status totals over the whole download list.
    pub fn counts(&self) -> StatusCounts {
        self.queue.counts()
    }

    /// Live progress of a running item.
    pub fn live(&self, item_id: u64) -> Option<&LiveProgress> {
        self.progress.get(&item_id)
    }

    /// Number of items with live progress.
    pub fn live_count(&self) -> usize {
        self.progress.len()
    }

    /// `(bytes done, expected size)` of an item: live counts while it runs,
    /// otherwise the last saved ones.
    pub fn item_bytes(&self, item: &Item) -> (u64, Option<u64>) {
        match self.progress.get(&item.id) {
            Some(live) => (live.done, live.total.or(item.bytes_total)),
            None if item.status == ItemStatus::Done => {
                let size = item.bytes_total.unwrap_or(item.bytes_done);
                (size, Some(size))
            }
            None => (item.bytes_done, item.bytes_total),
        }
    }

    /// Completion of an item in `0..=1`; `None` when its size is unknown.
    pub fn item_fraction(&self, item: &Item) -> Option<f32> {
        if item.status.is_finished() {
            return Some(1.0);
        }
        let (done, total) = self.item_bytes(item);
        match total {
            Some(0) | None => None,
            Some(total) => Some((done as f64 / total as f64).clamp(0.0, 1.0) as f32),
        }
    }

    /// Smoothed speed of a running item in bytes per second.
    pub fn item_speed(&self, item_id: u64, now: Instant) -> Option<f64> {
        self.progress.get(&item_id).and_then(|live| live.speed(now))
    }

    /// Time left for a running item at its current speed.
    pub fn item_eta(&self, item: &Item, now: Instant) -> Option<Duration> {
        let speed = self.item_speed(item.id, now)?;
        let (done, total) = self.item_bytes(item);
        eta(total?.saturating_sub(done), speed)
    }

    /// Combined speed of every running item (bytes per second).
    pub fn total_speed(&self, now: Instant) -> f64 {
        self.progress
            .values()
            .filter_map(|live| live.speed(now))
            .sum()
    }

    /// Totals over one package.
    pub fn package_aggregate(&self, package: &Package, now: Instant) -> PackageAggregate {
        let mut aggregate = PackageAggregate {
            counts: StatusCounts::of(&package.items),
            sizes_known: true,
            ..PackageAggregate::default()
        };
        let mut fraction_sum = 0.0_f64;
        for item in &package.items {
            fraction_sum += f64::from(self.item_fraction(item).unwrap_or(0.0));
            if item.status == ItemStatus::Skipped {
                continue;
            }
            let (done, total) = self.item_bytes(item);
            aggregate.bytes_done += done;
            match total {
                Some(total) => aggregate.bytes_total += total,
                None => aggregate.sizes_known = false,
            }
            aggregate.speed += self.item_speed(item.id, now).unwrap_or(0.0);
        }
        if !package.items.is_empty() {
            aggregate.fraction = (fraction_sum / package.items.len() as f64) as f32;
        }
        aggregate
    }

    /// Number of files across all staged packages.
    pub fn staged_files(&self) -> usize {
        self.staged.iter().map(|package| package.files.len()).sum()
    }

    /// The Log tab's lines, oldest first.
    pub fn log(&self) -> &VecDeque<LogLine> {
        &self.log
    }

    /// Error lines that arrived since the Log tab was last looked at.
    pub fn unseen_errors(&self) -> usize {
        self.unseen_errors
    }

    /// The Log tab is visible: errors so far count as seen.
    pub fn mark_log_seen(&mut self) {
        self.unseen_errors = 0;
    }

    /// Empties the Log tab.
    pub fn clear_log(&mut self) {
        self.log.clear();
        self.unseen_errors = 0;
    }

    /// Adds a line to the Log tab (also used for the window's own messages).
    pub fn push_log(&mut self, line: LogLine) {
        if line.level == LogLevel::Error {
            self.unseen_errors += 1;
        }
        self.log.push_back(line);
        while self.log.len() > LOG_CAPACITY {
            self.log.pop_front();
        }
    }
}

/// `remaining / speed`, when the speed is positive.
fn eta(remaining: u64, speed: f64) -> Option<Duration> {
    if remaining == 0 {
        return Some(Duration::ZERO);
    }
    if !(speed.is_finite() && speed > 0.0) {
        return None;
    }
    let secs = remaining as f64 / speed;
    // Anything beyond a few days is noise; cap it so the conversion cannot overflow.
    Some(Duration::from_secs_f64(secs.min(99.0 * 3600.0)))
}

/// `(images, videos)` among a staged package's files.
pub fn kind_counts(package: &StagedPackage) -> (usize, usize) {
    package
        .files
        .iter()
        .fold((0, 0), |(images, videos), file| match file.media.kind {
            MediaKind::Image => (images + 1, videos),
            MediaKind::Video => (images, videos + 1),
        })
}

/// `1.5 KB`, `2.4 MB`, … (binary units, like `--limit-rate`).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `2.4 MB/s`.
pub fn format_speed(bytes_per_second: f64) -> String {
    let bytes = if bytes_per_second.is_finite() && bytes_per_second > 0.0 {
        bytes_per_second.round() as u64
    } else {
        0
    };
    format!("{}/s", format_bytes(bytes))
}

/// `0:42`, `12:05`, `1:02:09`.
pub fn format_eta(eta: Duration) -> String {
    let secs = eta.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `3 downloading · 12 queued · 40 done · 1 failed` (zero counts left out).
pub fn status_summary(counts: &StatusCounts) -> String {
    if counts.total() == 0 {
        return "The download list is empty".to_string();
    }
    [
        (counts.downloading, "downloading"),
        (counts.queued, "queued"),
        (counts.done, "done"),
        (counts.skipped, "skipped"),
        (counts.failed, "failed"),
    ]
    .iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect::<Vec<_>>()
    .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use crate::gui::protocol::StagedFile;
    use crate::model::{Category, ItemError, MediaFile};

    fn media(key: &str, kind: MediaKind) -> MediaFile {
        MediaFile {
            key: key.to_string(),
            post_id: "100".to_string(),
            shortcode: Some("C0ffeeXYZ12".to_string()),
            index: 1,
            count: 1,
            kind,
            category: Category::Post,
            url: "https://cdn.example.invalid/x.jpg".to_string(),
            width: Some(1080),
            height: Some(1350),
            owner: "example_user".to_string(),
            owner_id: Some("42".to_string()),
            taken_at: 1_714_521_600,
            caption: None,
            highlight: None,
            ext: "jpg".to_string(),
        }
    }

    fn item(id: u64, status: ItemStatus, done: u64, total: Option<u64>) -> Item {
        Item {
            id,
            media: media(&format!("k{id}"), MediaKind::Image),
            rel_path: PathBuf::from(format!("example_user/posts/file_{id}.jpg")),
            status,
            bytes_done: done,
            bytes_total: total,
            error: (status == ItemStatus::Failed).then(|| ItemError {
                kind: ErrorKind::Network,
                message: "connection reset".to_string(),
            }),
            path: None,
            note: None,
        }
    }

    fn queue(items: Vec<Item>) -> Arc<Queue> {
        Arc::new(Queue {
            version: 1,
            next_id: 100,
            packages: vec![Package {
                id: 1,
                name: "example_user · posts".to_string(),
                source: "https://www.instagram.com/example_user/".to_string(),
                out_dir: PathBuf::from("/downloads"),
                created_at: 0,
                items,
            }],
        })
    }

    fn progress(item_id: u64, done: u64, total: Option<u64>) -> Update {
        Update::Progress {
            item_id,
            done,
            total,
        }
    }

    fn log_line(level: LogLevel, text: &str) -> Update {
        Update::Log(LogLine {
            at: 0,
            level,
            text: text.to_string(),
        })
    }

    #[test]
    fn applies_snapshots_and_flags() {
        let mut state = ViewState::default();
        let now = Instant::now();
        let settings = Settings {
            jobs: Some(6),
            ..Settings::default()
        };
        state.apply(
            Update::Settings {
                settings: Box::new(settings.clone()),
                config_path: Some(PathBuf::from("/cfg/config.toml")),
                default_out_dir: PathBuf::from("/home/me"),
            },
            now,
        );
        assert_eq!(state.settings, settings);
        assert_eq!(state.settings_generation, 1);
        assert_eq!(state.default_out_dir, PathBuf::from("/home/me"));

        let q = queue(vec![
            item(2, ItemStatus::Done, 10, Some(10)),
            item(3, ItemStatus::Queued, 0, None),
        ]);
        state.apply(Update::Queue(q.clone()), now);
        assert!(Arc::ptr_eq(&state.queue, &q));

        state.apply(Update::Grabbing { pending_links: 3 }, now);
        state.apply(Update::Downloading(true), now);
        state.apply(
            Update::Account(AccountStatus::LoggedIn {
                username: "me".into(),
            }),
            now,
        );
        state.apply(Update::Fatal("queue locked".into()), now);
        assert_eq!(state.grabbing, 3);
        assert!(state.downloading);
        assert_eq!(
            state.account,
            Some(AccountStatus::LoggedIn {
                username: "me".into()
            })
        );
        assert_eq!(state.fatal.as_deref(), Some("queue locked"));

        let staged = StagedPackage {
            id: 7,
            name: "post C0ffeeXYZ12".into(),
            source: "https://www.instagram.com/p/C0ffeeXYZ12/".into(),
            out_dir: PathBuf::from("/downloads"),
            files: vec![
                StagedFile {
                    media: media("a", MediaKind::Image),
                    rel_path: PathBuf::from("a.jpg"),
                },
                StagedFile {
                    media: media("b", MediaKind::Video),
                    rel_path: PathBuf::from("b.mp4"),
                },
            ],
            archived: 0,
            already_queued: 0,
            notes: vec![],
        };
        state.apply(Update::Staged(vec![staged.clone()]), now);
        assert_eq!(state.staged_files(), 2);
        assert_eq!(kind_counts(&staged), (1, 1));
    }

    #[test]
    fn counts_come_from_the_queue() {
        let mut state = ViewState::default();
        state.apply(
            Update::Queue(queue(vec![
                item(2, ItemStatus::Done, 10, Some(10)),
                item(3, ItemStatus::Queued, 0, None),
                item(4, ItemStatus::Queued, 0, None),
                item(5, ItemStatus::Failed, 0, None),
                item(6, ItemStatus::Downloading, 5, Some(10)),
            ])),
            Instant::now(),
        );
        let counts = state.counts();
        assert_eq!(
            (
                counts.done,
                counts.queued,
                counts.failed,
                counts.downloading
            ),
            (1, 2, 1, 1)
        );
        assert_eq!(
            status_summary(&counts),
            "1 downloading · 2 queued · 1 done · 1 failed"
        );
        assert_eq!(
            status_summary(&StatusCounts::default()),
            "The download list is empty"
        );
    }

    #[test]
    fn speed_is_estimated_from_successive_progress() {
        let mut state = ViewState::default();
        let start = Instant::now();
        state.apply(
            Update::Queue(queue(vec![item(2, ItemStatus::Downloading, 0, None)])),
            start,
        );
        state.apply(progress(2, 0, Some(20_000_000)), start);
        assert_eq!(state.item_speed(2, start), None, "one sample has no rate");

        // Samples closer than the minimum interval are merged.
        let t = start + Duration::from_millis(100);
        state.apply(progress(2, 50_000, Some(20_000_000)), t);
        assert_eq!(state.item_speed(2, t), None);

        // 1 MB/s steady.
        let mut t = start;
        for step in 1..=20u64 {
            t = start + Duration::from_millis(500 * step);
            state.apply(progress(2, step * 500_000, Some(20_000_000)), t);
        }
        let speed = state.item_speed(2, t).unwrap();
        assert!((speed - 1_000_000.0).abs() < 1.0, "{speed}");
        assert!((state.total_speed(t) - speed).abs() < f64::EPSILON);

        // A sudden jump is smoothed, not taken as-is.
        let t2 = t + Duration::from_millis(500);
        state.apply(progress(2, 14_000_000, Some(20_000_000)), t2);
        let smoothed = state.item_speed(2, t2).unwrap();
        assert!(
            smoothed > 1_000_000.0 && smoothed < 8_000_000.0,
            "{smoothed}"
        );

        // Stalled transfers report 0 B/s.
        assert_eq!(state.item_speed(2, t2 + Duration::from_secs(10)), Some(0.0));
    }

    #[test]
    fn item_fraction_and_eta() {
        let mut state = ViewState::default();
        let start = Instant::now();
        let downloading = item(2, ItemStatus::Downloading, 0, Some(4_000_000));
        state.apply(Update::Queue(queue(vec![downloading.clone()])), start);
        assert_eq!(state.item_fraction(&downloading), Some(0.0));
        state.apply(progress(2, 1_000_000, Some(4_000_000)), start);
        state.apply(
            progress(2, 2_000_000, Some(4_000_000)),
            start + Duration::from_secs(1),
        );
        assert_eq!(state.item_fraction(&downloading), Some(0.5));
        let eta = state
            .item_eta(&downloading, start + Duration::from_secs(1))
            .unwrap();
        assert_eq!(eta.as_secs(), 2);

        let unknown = item(3, ItemStatus::Queued, 0, None);
        assert_eq!(state.item_fraction(&unknown), None);
        assert_eq!(state.item_eta(&unknown, start), None);
        let done = item(4, ItemStatus::Done, 7, Some(7));
        assert_eq!(state.item_fraction(&done), Some(1.0));
        assert_eq!(state.item_bytes(&done), (7, Some(7)));
    }

    #[test]
    fn progress_is_dropped_for_items_no_longer_downloading() {
        let mut state = ViewState::default();
        let start = Instant::now();
        state.apply(
            Update::Queue(queue(vec![
                item(2, ItemStatus::Downloading, 0, None),
                item(3, ItemStatus::Downloading, 0, None),
            ])),
            start,
        );
        state.apply(progress(2, 10, Some(100)), start);
        state.apply(progress(3, 10, Some(100)), start);
        state.apply(progress(4, 10, Some(100)), start);
        assert_eq!(state.live_count(), 3);

        let later = start + Duration::from_secs(5);
        state.apply(
            Update::Queue(queue(vec![
                item(2, ItemStatus::Downloading, 50, Some(100)),
                item(3, ItemStatus::Done, 100, Some(100)),
                item(4, ItemStatus::Queued, 0, None),
            ])),
            later,
        );
        assert!(state.live(2).is_some());
        assert!(state.live(3).is_none(), "finished");
        assert!(state.live(4).is_none(), "queued and silent for a while");

        // A just-started item the snapshot still shows as queued keeps its progress.
        state.apply(progress(4, 20, Some(100)), later);
        state.apply(
            Update::Queue(queue(vec![
                item(2, ItemStatus::Downloading, 50, Some(100)),
                item(4, ItemStatus::Queued, 0, None),
            ])),
            later,
        );
        assert!(state.live(4).is_some());

        state.apply(Update::Downloading(false), later);
        assert_eq!(state.live_count(), 0);
    }

    #[test]
    fn package_aggregate_sums_items() {
        let mut state = ViewState::default();
        let start = Instant::now();
        let q = queue(vec![
            item(2, ItemStatus::Done, 100, Some(100)),
            item(3, ItemStatus::Downloading, 0, Some(200)),
            item(4, ItemStatus::Skipped, 0, None),
            item(5, ItemStatus::Queued, 0, None),
        ]);
        state.apply(Update::Queue(q.clone()), start);
        state.apply(progress(3, 0, Some(200)), start);
        state.apply(progress(3, 100, Some(200)), start + Duration::from_secs(1));
        let aggregate = state.package_aggregate(&q.packages[0], start + Duration::from_secs(1));
        assert_eq!(aggregate.files_total(), 4);
        assert_eq!(aggregate.files_finished(), 2);
        assert_eq!(aggregate.bytes_done, 200);
        assert_eq!(aggregate.bytes_total, 300);
        assert!(!aggregate.sizes_known, "item 5 has no size yet");
        assert_eq!(aggregate.eta(), None);
        assert!((aggregate.speed - 100.0).abs() < 1e-6);
        // (1 + 0.5 + 1 + 0) / 4
        assert!((aggregate.fraction - 0.625).abs() < 1e-6);
        assert_eq!(aggregate.status(), PackageStatus::Downloading);
    }

    #[test]
    fn package_status_priorities() {
        let status = |items: Vec<Item>| {
            let state = ViewState::default();
            let q = queue(items);
            state
                .package_aggregate(&q.packages[0], Instant::now())
                .status()
        };
        assert_eq!(
            status(vec![
                item(2, ItemStatus::Failed, 0, None),
                item(3, ItemStatus::Queued, 0, None)
            ]),
            PackageStatus::Queued
        );
        assert_eq!(
            status(vec![
                item(2, ItemStatus::Failed, 0, None),
                item(3, ItemStatus::Done, 1, Some(1))
            ]),
            PackageStatus::Failed(1)
        );
        assert_eq!(
            status(vec![
                item(2, ItemStatus::Skipped, 0, None),
                item(3, ItemStatus::Done, 1, Some(1))
            ]),
            PackageStatus::Done
        );
        assert_eq!(
            status(vec![item(2, ItemStatus::Skipped, 0, None)]),
            PackageStatus::Skipped
        );
        assert_eq!(status(vec![]), PackageStatus::Empty);
    }

    #[test]
    fn log_is_capped_and_counts_unseen_errors() {
        let mut state = ViewState::default();
        let now = Instant::now();
        state.apply(log_line(LogLevel::Info, "first"), now);
        state.apply(log_line(LogLevel::Error, "boom"), now);
        state.apply(log_line(LogLevel::Warn, "careful"), now);
        state.apply(log_line(LogLevel::Error, "boom again"), now);
        assert_eq!(state.unseen_errors(), 2);
        state.mark_log_seen();
        assert_eq!(state.unseen_errors(), 0);
        state.apply(log_line(LogLevel::Error, "third"), now);
        assert_eq!(state.unseen_errors(), 1);

        for n in 0..LOG_CAPACITY + 10 {
            state.apply(log_line(LogLevel::Info, &format!("line {n}")), now);
        }
        assert_eq!(state.log().len(), LOG_CAPACITY);
        assert_eq!(state.log().front().unwrap().text, "line 10");
        assert_eq!(
            state.log().back().unwrap().text,
            format!("line {}", LOG_CAPACITY + 9)
        );

        state.clear_log();
        assert!(state.log().is_empty());
        assert_eq!(state.unseen_errors(), 0);
    }

    #[test]
    fn formats_sizes_speeds_and_etas() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2_516_582), "2.4 MB");
        assert_eq!(format_bytes(150 * 1024 * 1024), "150 MB");
        assert_eq!(format_bytes(3 << 30), "3.0 GB");
        assert_eq!(format_speed(2_516_582.0), "2.4 MB/s");
        assert_eq!(format_speed(f64::NAN), "0 B/s");
        assert_eq!(format_eta(Duration::from_secs(42)), "0:42");
        assert_eq!(format_eta(Duration::from_secs(725)), "12:05");
        assert_eq!(format_eta(Duration::from_secs(3729)), "1:02:09");
        assert_eq!(eta(10, 0.0), None);
        assert_eq!(eta(0, 0.0), Some(Duration::ZERO));
        assert_eq!(eta(100, 50.0), Some(Duration::from_secs(2)));
    }
}
