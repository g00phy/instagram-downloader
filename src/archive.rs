//! `archive.txt`: one media key per line for every finished file.

use std::path::Path;

use crate::error::Result;

/// Set of finished media keys backed by an append-only file.
pub struct Archive {
    _private: (),
}

impl Archive {
    /// Opens (or creates on first insert) the archive file and loads its keys.
    pub fn open(path: &Path) -> Result<Self> {
        let _ = path;
        todo!("queue agent")
    }

    /// An archive that remembers nothing and writes nothing (`--no-archive`).
    pub fn disabled() -> Self {
        todo!("queue agent")
    }

    pub fn contains(&self, key: &str) -> bool {
        let _ = key;
        todo!("queue agent")
    }

    /// Records `key`, appending it to the file immediately (no-op if present).
    pub fn insert(&mut self, key: &str) -> Result<()> {
        let _ = key;
        todo!("queue agent")
    }

    pub fn len(&self) -> usize {
        todo!("queue agent")
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
