//! Resolving a [`Target`] into the files to download.

use crate::error::Result;
use crate::instagram::Client;
use crate::links::{ProfileSection, Target};
use crate::model::{MediaFile, MediaKind};

/// Crawl filters and limits.
#[derive(Debug, Clone)]
pub struct CrawlOptions {
    /// Sections crawled for plain profile links (`Target::Profile { section: None }`).
    pub sections: Vec<ProfileSection>,
    /// Max posts (not files) per profile section.
    pub limit: Option<usize>,
    /// Only posts taken at or after this unix time.
    pub since: Option<i64>,
    /// Only posts taken at or before this unix time.
    pub until: Option<i64>,
    /// Only images or only videos.
    pub only: Option<MediaKind>,
    /// Stop paginating a profile section at the first (non-pinned) post whose
    /// files are all archived (`--update`).
    pub stop_at_archived: bool,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        CrawlOptions {
            sections: vec![ProfileSection::Posts],
            limit: None,
            since: None,
            until: None,
            only: None,
            stop_at_archived: false,
        }
    }
}

/// The outcome of resolving one link.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// Package name, e.g. `natgeo · posts, stories`.
    pub name: String,
    pub files: Vec<MediaFile>,
    /// Human-readable notes about things left out (filters, unavailable sections).
    pub notes: Vec<String>,
}

/// Resolves `target` into files. `is_archived(key)` tells whether a file key is
/// already downloaded (used by `stop_at_archived`). Progress is logged with `tracing`.
pub async fn resolve(
    client: &Client,
    target: &Target,
    options: &CrawlOptions,
    is_archived: &(dyn Fn(&str) -> bool + Send + Sync),
) -> Result<Resolved> {
    let _ = (client, target, options, is_archived);
    todo!("instagram agent")
}

/// Fetches a fresh signed URL for `media` (whose URL expired) via
/// `media/{post_id}/info/`, matching the file by `key`.
pub async fn refresh_url(client: &Client, media: &MediaFile) -> Result<String> {
    let _ = (client, media);
    todo!("instagram agent")
}
