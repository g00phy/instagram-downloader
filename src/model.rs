//! Data types shared between the crawler, the queue and the downloader.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::ErrorKind;

/// Whether a file is a picture or a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    /// `image` or `video`.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
        }
    }
}

/// Where a file came from; drives the `{category}` template placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Post,
    Reel,
    Tagged,
    Story,
    Highlight,
}

impl Category {
    /// Folder name used by `{category}`: `posts`, `reels`, `tagged`, `stories`, `highlights`.
    pub fn dir_name(self) -> &'static str {
        match self {
            Category::Post => "posts",
            Category::Reel => "reels",
            Category::Tagged => "tagged",
            Category::Story => "stories",
            Category::Highlight => "highlights",
        }
    }
}

/// One downloadable file, as resolved by the crawler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFile {
    /// Stable unique id of this file: the media pk, or the child's pk inside a
    /// carousel. Used as the archive key.
    pub key: String,
    /// pk of the post / story item the file belongs to (the carousel parent).
    /// `media/{post_id}/info/` re-resolves the file when its URL expires.
    pub post_id: String,
    /// Post shortcode (`C0ffeeXYZ12`); stories have none.
    pub shortcode: Option<String>,
    /// 1-based position inside the post (1 for single-media posts).
    pub index: u32,
    /// Number of files in the post.
    pub count: u32,
    pub kind: MediaKind,
    pub category: Category,
    /// Signed CDN URL. Expires (see the `oe` query parameter).
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Username of the post owner.
    pub owner: String,
    pub owner_id: Option<String>,
    /// Unix timestamp (seconds, UTC) the post was taken.
    pub taken_at: i64,
    pub caption: Option<String>,
    /// Highlight title, for highlight items.
    pub highlight: Option<String>,
    /// File extension without the dot (`jpg`, `mp4`, `webp`, `heic`).
    pub ext: String,
}

/// Lifecycle of a queued file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Queued,
    Downloading,
    Done,
    Skipped,
    Failed,
}

impl ItemStatus {
    /// `true` for `Done` and `Skipped`.
    pub fn is_finished(self) -> bool {
        matches!(self, ItemStatus::Done | ItemStatus::Skipped)
    }
}

/// Why an item failed; the message is already redacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemError {
    pub kind: ErrorKind,
    pub message: String,
}

/// One file in the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Queue-wide id (shared id space with packages).
    pub id: u64,
    pub media: MediaFile,
    /// Output path relative to the package's `out_dir`, rendered at grab time.
    pub rel_path: PathBuf,
    pub status: ItemStatus,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub error: Option<ItemError>,
    /// Absolute path of the finished file.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Human-readable reason for a skip.
    #[serde(default)]
    pub note: Option<String>,
}

/// A group of items created from one link (JDownloader's "package").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    /// Queue-wide id (shared id space with items).
    pub id: u64,
    /// Display name, e.g. `natgeo · posts` or `post C0ffeeXYZ12`.
    pub name: String,
    /// The link the user supplied.
    pub source: String,
    /// Absolute download root for this package.
    pub out_dir: PathBuf,
    /// Unix timestamp the package was added.
    pub created_at: i64,
    pub items: Vec<Item>,
}
