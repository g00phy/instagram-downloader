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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_contents_are_not_reported() {
        let link = "https://www.instagram.com/p/C0ffeeXYZ12/";
        let mut diff = ClipboardDiff::new(Some(link.into()));
        assert!(diff.update(Some(link.into())).is_empty());
    }

    #[test]
    fn new_text_yields_its_instagram_links_once() {
        let mut diff = ClipboardDiff::default();
        let text = "look: https://instagram.com/reel/C0ffeeXYZ12/?igsh=x and https://example.com";
        assert_eq!(diff.update(Some(text.into())).len(), 1);
        assert!(diff.update(Some(text.into())).is_empty());
        assert!(diff.update(None).is_empty());
        assert!(diff.update(Some("no links here".into())).is_empty());
    }
}
