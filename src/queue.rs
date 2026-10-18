//! The persistent download list (`queue.json`) and its exclusive lock.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{MediaFile, Package};

/// Current `queue.json` format version.
pub const QUEUE_VERSION: u32 = 1;

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
        let _ = (name, source, out_dir, files);
        todo!("queue agent")
    }
}

/// Owns the data directory's `queue.json` and holds `queue.lock` exclusively for
/// its lifetime, so two `igdl` processes never write the queue concurrently.
pub struct QueueStore {
    _private: (),
}

impl QueueStore {
    /// Creates `data_dir` if needed and takes the lock. A held lock is an
    /// [`crate::error::Error::Config`] ("another igdl process is using the queue").
    pub fn open(data_dir: &Path) -> Result<Self> {
        let _ = data_dir;
        todo!("queue agent")
    }

    /// Reads `queue.json` without taking the lock (for `igdl list`). A missing
    /// file is an empty queue.
    pub fn read_unlocked(data_dir: &Path) -> Result<Queue> {
        let _ = data_dir;
        todo!("queue agent")
    }

    /// Loads the queue (empty if the file does not exist yet).
    pub fn load(&self) -> Result<Queue> {
        todo!("queue agent")
    }

    /// Saves atomically (write temp file, then rename over `queue.json`).
    pub fn save(&self, queue: &Queue) -> Result<()> {
        let _ = queue;
        todo!("queue agent")
    }
}
