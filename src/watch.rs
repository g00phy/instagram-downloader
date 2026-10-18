//! Clipboard observer for `igdl watch` (JDownloader's "clipboard monitoring").

use crate::error::{Error, Result};
use crate::links;

/// Remembers the last clipboard text and reports links in new text only.
#[derive(Debug, Default)]
pub struct ClipboardDiff {
    last: Option<String>,
}

impl ClipboardDiff {
    /// Starts from the current clipboard contents, which are not reported.
    pub fn new(current: Option<String>) -> Self {
        ClipboardDiff { last: current }
    }

    /// Instagram links in `text` if it differs from the previous contents.
    pub fn update(&mut self, text: Option<String>) -> Vec<String> {
        let Some(text) = text else {
            return Vec::new();
        };
        if self.last.as_deref() == Some(text.as_str()) {
            return Vec::new();
        }
        let found = links::extract_links(&text);
        self.last = Some(text);
        found
    }
}

/// The system clipboard plus change tracking.
pub struct Clipboard {
    inner: arboard::Clipboard,
    diff: ClipboardDiff,
}

impl Clipboard {
    /// Opens the system clipboard (fails on headless machines).
    pub fn open() -> Result<Self> {
        let mut inner = arboard::Clipboard::new()
            .map_err(|err| Error::Config(format!("clipboard unavailable: {err}")))?;
        let current = inner.get_text().ok();
        Ok(Clipboard {
            inner,
            diff: ClipboardDiff::new(current),
        })
    }

    /// Links copied since the last call (non-text contents are ignored).
    pub fn new_links(&mut self) -> Vec<String> {
        let text = self.inner.get_text().ok();
        self.diff.update(text)
    }
}
