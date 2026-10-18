//! The persistent download list (`queue.json`) and its exclusive lock.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsutil;
use crate::model::{Item, ItemStatus, MediaFile, Package};

/// Current `queue.json` format version.
pub const QUEUE_VERSION: u32 = 1;

/// File name of the queue inside the data directory.
pub const QUEUE_FILE: &str = "queue.json";

/// File name of the lock file inside the data directory.
pub const LOCK_FILE: &str = "queue.lock";

/// Temporary file `save` writes before renaming it over [`QUEUE_FILE`].
const QUEUE_TMP_FILE: &str = "queue.json.tmp";

/// All packages. Packages and items share one id space, so `igdl remove 12`
/// is unambiguous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Queue {
    pub version: u32,
    /// Next id to hand out (starts at 1).
    pub next_id: u64,
    pub packages: Vec<Package>,
}

impl Default for Queue {
    fn default() -> Self {
        Queue {
            version: QUEUE_VERSION,
            next_id: 1,
            packages: Vec::new(),
        }
    }
}

/// Which packages/items `igdl clear` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearFilter {
    /// Packages whose items are all done or skipped.
    Finished,
    /// Failed items (and packages left empty by that).
    Failed,
    /// Everything.
    All,
}

impl Queue {
    /// Adds a package with one `Queued` item per `(media, rel_path)` and returns
    /// the package id. Files whose `media.key` is already in the queue (in any
    /// state) are left out; returns `None` if nothing was left to add.
    pub fn add_package(
        &mut self,
        name: String,
        source: String,
        out_dir: PathBuf,
        files: Vec<(MediaFile, PathBuf)>,
    ) -> Option<u64> {
        let mut seen: HashSet<String> = self.items().map(|item| item.media.key.clone()).collect();
        let kept: Vec<(MediaFile, PathBuf)> = files
            .into_iter()
            .filter(|(media, _)| seen.insert(media.key.clone()))
            .collect();
        if kept.is_empty() {
            return None;
        }

        let package_id = self.next_id;
        let mut next = package_id.saturating_add(1);
        let items = kept
            .into_iter()
            .map(|(media, rel_path)| {
                let id = next;
                next = next.saturating_add(1);
                Item {
                    id,
                    media,
                    rel_path,
                    status: ItemStatus::Queued,
                    bytes_done: 0,
                    bytes_total: None,
                    error: None,
                    path: None,
                    note: None,
                }
            })
            .collect();
        self.next_id = next;
        self.packages.push(Package {
            id: package_id,
            name,
            source,
            out_dir,
            created_at: chrono::Utc::now().timestamp(),
            items,
        });
        Some(package_id)
    }

    /// The package with this id.
    pub fn package(&self, id: u64) -> Option<&Package> {
        self.packages.iter().find(|package| package.id == id)
    }

    /// The item with this id.
    pub fn item(&self, id: u64) -> Option<&Item> {
        self.items().find(|item| item.id == id)
    }

    /// The item with this id, mutably.
    pub fn item_mut(&mut self, id: u64) -> Option<&mut Item> {
        self.packages
            .iter_mut()
            .flat_map(|package| package.items.iter_mut())
            .find(|item| item.id == id)
    }

    /// The package containing item `item_id`.
    pub fn package_of(&self, item_id: u64) -> Option<&Package> {
        self.packages
            .iter()
            .find(|package| package.items.iter().any(|item| item.id == item_id))
    }

    /// Every item of every package, in queue order.
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.packages
            .iter()
            .flat_map(|package| package.items.iter())
    }

    /// Ids of `Queued` items, in queue order; limited to the given package ids
    /// unless `packages` is empty.
    pub fn pending(&self, packages: &[u64]) -> Vec<u64> {
        self.packages
            .iter()
            .filter(|package| packages.is_empty() || packages.contains(&package.id))
            .flat_map(|package| package.items.iter())
            .filter(|item| item.status == ItemStatus::Queued)
            .map(|item| item.id)
            .collect()
    }

    /// Turns `Downloading` items (left over from an interrupted run) back into
    /// `Queued`; returns how many changed.
    pub fn reset_interrupted(&mut self) -> usize {
        let mut changed = 0;
        for item in self.items_mut() {
            if item.status == ItemStatus::Downloading {
                item.status = ItemStatus::Queued;
                changed += 1;
            }
        }
        changed
    }

    /// Re-queues `Failed` items (clearing their error) among `ids` (package or
    /// item ids; empty = all). Returns how many changed.
    pub fn retry(&mut self, ids: &[u64]) -> usize {
        let mut changed = 0;
        for package in &mut self.packages {
            let whole_package = ids.is_empty() || ids.contains(&package.id);
            for item in &mut package.items {
                if item.status == ItemStatus::Failed && (whole_package || ids.contains(&item.id)) {
                    item.status = ItemStatus::Queued;
                    item.error = None;
                    changed += 1;
                }
            }
        }
        changed
    }

    /// Removes packages and/or items by id; packages left empty are dropped.
    /// Unknown ids are ignored. Returns the number of items removed.
    pub fn remove(&mut self, ids: &[u64]) -> usize {
        let mut removed = 0;
        self.packages.retain_mut(|package| {
            let before = package.items.len();
            if ids.contains(&package.id) {
                package.items.clear();
            } else {
                package.items.retain(|item| !ids.contains(&item.id));
            }
            removed += before - package.items.len();
            !package.items.is_empty()
        });
        removed
    }

    /// Removes according to `filter`; returns the number of items removed.
    pub fn clear(&mut self, filter: ClearFilter) -> usize {
        let mut removed = 0;
        match filter {
            ClearFilter::Finished => self.packages.retain(|package| {
                let finished = package.items.iter().all(|item| item.status.is_finished());
                if finished {
                    removed += package.items.len();
                }
                !finished
            }),
            ClearFilter::Failed => self.packages.retain_mut(|package| {
                let before = package.items.len();
                package
                    .items
                    .retain(|item| item.status != ItemStatus::Failed);
                removed += before - package.items.len();
                !package.items.is_empty()
            }),
            ClearFilter::All => {
                removed = self.items().count();
                self.packages.clear();
            }
        }
        removed
    }

    /// Whether any item (any status) has this media key.
    pub fn contains_key(&self, key: &str) -> bool {
        self.items().any(|item| item.media.key == key)
    }

    /// Status totals over the whole queue.
    pub fn counts(&self) -> StatusCounts {
        StatusCounts::of(self.items())
    }

    fn items_mut(&mut self) -> impl Iterator<Item = &mut Item> {
        self.packages
            .iter_mut()
            .flat_map(|package| package.items.iter_mut())
    }
}

/// Number of items per status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusCounts {
    pub queued: usize,
    pub downloading: usize,
    pub done: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl StatusCounts {
    /// Counts the given items.
    pub fn of<'a>(items: impl IntoIterator<Item = &'a Item>) -> Self {
        let mut counts = StatusCounts::default();
        for item in items {
            match item.status {
                ItemStatus::Queued => counts.queued += 1,
                ItemStatus::Downloading => counts.downloading += 1,
                ItemStatus::Done => counts.done += 1,
                ItemStatus::Skipped => counts.skipped += 1,
                ItemStatus::Failed => counts.failed += 1,
            }
        }
        counts
    }

    /// Number of items counted.
    pub fn total(&self) -> usize {
        self.queued + self.downloading + self.done + self.skipped + self.failed
    }
}

/// Owns the data directory's `queue.json` and holds `queue.lock` exclusively for
/// its lifetime, so two `igdl` processes never write the queue concurrently.
pub struct QueueStore {
    dir: PathBuf,
    /// Holds the exclusive lock; released when the store is dropped.
    _lock: File,
}

impl std::fmt::Debug for QueueStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl QueueStore {
    /// Creates `data_dir` if needed and takes the lock. A held lock is an
    /// [`crate::error::Error::Config`] ("another igdl process is using the queue").
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir).map_err(|err| {
            Error::io(
                format!("cannot create the data directory {}", data_dir.display()),
                err,
            )
        })?;
        let lock_path = data_dir.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| Error::io(format!("cannot open {}", lock_path.display()), err))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Config(format!(
                    "another igdl process is using the queue in {}; wait for it to finish",
                    data_dir.display()
                )));
            }
            Err(TryLockError::Error(err)) => {
                return Err(Error::io(
                    format!("cannot lock {}", lock_path.display()),
                    err,
                ));
            }
        }
        Ok(QueueStore {
            dir: data_dir.to_path_buf(),
            _lock: lock,
        })
    }

    /// Reads `queue.json` without taking the lock (for `igdl list`). A missing
    /// file is an empty queue.
    pub fn read_unlocked(data_dir: &Path) -> Result<Queue> {
        load_from(&data_dir.join(QUEUE_FILE))
    }

    /// The data directory this store manages.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of `queue.json`.
    pub fn queue_path(&self) -> PathBuf {
        self.dir.join(QUEUE_FILE)
    }

    /// Loads the queue (empty if the file does not exist yet).
    pub fn load(&self) -> Result<Queue> {
        load_from(&self.queue_path())
    }

    /// Saves atomically (write temp file, then rename over `queue.json`).
    pub fn save(&self, queue: &Queue) -> Result<()> {
        let path = self.queue_path();
        let tmp = self.dir.join(QUEUE_TMP_FILE);
        let mut json = serde_json::to_vec_pretty(queue).map_err(|err| {
            Error::io(
                format!("cannot serialise the queue for {}", path.display()),
                std::io::Error::other(err),
            )
        })?;
        json.push(b'\n');

        if let Err(err) = write_synced(&tmp, &json) {
            // Best effort: do not leave a half-written temp file behind.
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::io(format!("cannot write {}", tmp.display()), err));
        }
        if let Err(err) = fsutil::rename_with_retry(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::io(format!("cannot replace {}", path.display()), err));
        }
        sync_dir(&self.dir);
        Ok(())
    }
}

/// Writes `data` to a fresh file at `path` and flushes it to disk.
fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// Makes the rename durable on Unix; directories cannot be opened like this on Windows.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(handle) = File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Reads and validates a queue file; a missing file is an empty queue.
fn load_from(path: &Path) -> Result<Queue> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Queue::default()),
        Err(err) => return Err(Error::io(format!("cannot read {}", path.display()), err)),
    };
    let corrupt = |err: serde_json::Error| {
        Error::Config(format!(
            "{} is corrupt ({err}); move it away to start with an empty queue",
            path.display()
        ))
    };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(corrupt)?;
    // Check the version before the shape, so a newer format gets a useful message.
    if let Some(version) = value.get("version").and_then(serde_json::Value::as_u64)
        && version > u64::from(QUEUE_VERSION)
    {
        return Err(Error::Config(format!(
            "{} uses queue format {version}, but this igdl only understands format {QUEUE_VERSION}; upgrade igdl",
            path.display()
        )));
    }
    serde_json::from_value(value).map_err(corrupt)
}
