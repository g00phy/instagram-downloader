//! `archive.txt`: one media key per line for every finished file.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Set of finished media keys backed by an append-only file.
#[derive(Debug)]
pub struct Archive {
    /// `None` for [`Archive::disabled`].
    path: Option<PathBuf>,
    keys: HashSet<String>,
    /// The existing file does not end with a newline (edited by hand); the next
    /// append starts a new line first.
    needs_newline: bool,
}

impl Archive {
    /// Opens (or creates on first insert) the archive file and loads its keys.
    pub fn open(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(Error::io(format!("cannot read {}", path.display()), err)),
        };
        let keys = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Archive {
            path: Some(path.to_path_buf()),
            keys,
            needs_newline: !text.is_empty() && !text.ends_with('\n'),
        })
    }

    /// An archive that remembers nothing and writes nothing (`--no-archive`).
    pub fn disabled() -> Self {
        Archive {
            path: None,
            keys: HashSet::new(),
            needs_newline: false,
        }
    }

    /// Whether this archive is backed by a file (not [`Archive::disabled`]).
    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Whether `key` was recorded (always `false` when disabled).
    pub fn contains(&self, key: &str) -> bool {
        self.keys.contains(key.trim())
    }

    /// Records `key`, appending it to the file immediately (no-op if present).
    pub fn insert(&mut self, key: &str) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let key = key.trim();
        if key.is_empty() || self.keys.contains(key) {
            return Ok(());
        }
        if key.contains(['\n', '\r']) {
            return Err(Error::Config(
                "an archive key must not contain line breaks".to_string(),
            ));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|err| Error::io(format!("cannot create {}", parent.display()), err))?;
        }
        let mut line = String::with_capacity(key.len() + 2);
        if self.needs_newline {
            line.push('\n');
        }
        line.push_str(key);
        line.push('\n');
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(|err| Error::io(format!("cannot open {}", path.display()), err))?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|err| Error::io(format!("cannot write {}", path.display()), err))?;
        self.needs_newline = false;
        self.keys.insert(key.to_owned());
        Ok(())
    }

    /// Number of recorded keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
