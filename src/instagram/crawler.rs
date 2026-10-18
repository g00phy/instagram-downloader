//! Resolving a [`Target`] into the files to download.

use std::collections::HashSet;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::instagram::Client;
use crate::instagram::client::UserSummary;
use crate::instagram::extract::{ItemContext, media_files};
use crate::instagram::models::{
    self, ClipsPage, FeedPage, HighlightsTray, MediaInfo, MediaItem, ReelsMedia, parse_item,
};
use crate::links::{ProfileSection, Target, shortcode_to_media_id};
use crate::model::{Category, MediaFile, MediaKind};

/// Posts requested per feed page.
const PAGE_SIZE: &str = "12";

/// Highlights fetched per `reels_media` request.
const HIGHLIGHT_CHUNK: usize = 5;

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
///
/// For profiles, `limit`, `since`, `until` and `stop_at_archived` apply to the
/// paginated sections (posts, reels, tagged); `limit` also caps the number of
/// highlights. `only` applies to every file. Files are deduplicated by key.
pub async fn resolve(
    client: &Client,
    target: &Target,
    options: &CrawlOptions,
    is_archived: &(dyn Fn(&str) -> bool + Send + Sync),
) -> Result<Resolved> {
    let mut out = Collector::new(options.only);
    let name = match target {
        Target::Post { shortcode, index } => {
            resolve_post(client, shortcode, *index, &mut out).await?
        }
        Target::Profile { username, section } => {
            let sections = match section {
                Some(section) => vec![*section],
                None => options.sections.clone(),
            };
            resolve_profile(client, username, &sections, options, is_archived, &mut out).await?
        }
        Target::Stories { username, story_id } => {
            let user = client.user_by_username(username).await?;
            ensure_visible(client, &user)?;
            crawl_stories(client, &user, story_id.as_deref(), &mut out).await?;
            format!("{} · stories", user.username)
        }
        Target::Highlight { id } => resolve_highlight(client, id, &mut out).await?,
    };
    Ok(out.finish(name))
}

/// Fetches a fresh signed URL for `media` (whose URL expired) via
/// `media/{post_id}/info/`, matching the file by `key`.
pub async fn refresh_url(client: &Client, media: &MediaFile) -> Result<String> {
    let gone = || {
        Error::NotFound(format!(
            "media {} of @{} is no longer available",
            media.key, media.owner
        ))
    };
    let item = match fetch_media_item(client, &media.post_id).await {
        Err(Error::NotFound(_)) => return Err(gone()),
        other => other?,
    };
    let extracted = media_files(&item, &ItemContext::new(media.category));
    extracted
        .files
        .iter()
        .find(|file| file.key == media.key)
        .or_else(|| {
            extracted
                .files
                .iter()
                .find(|file| file.index == media.index)
        })
        .map(|file| file.url.clone())
        .ok_or_else(gone)
}

/// Accumulates files (with the `only` filter and dedupe) and notes.
struct Collector {
    only: Option<MediaKind>,
    files: Vec<MediaFile>,
    seen: HashSet<String>,
    notes: Vec<String>,
    left_out: usize,
}

impl Collector {
    fn new(only: Option<MediaKind>) -> Self {
        Collector {
            only,
            files: Vec::new(),
            seen: HashSet::new(),
            notes: Vec::new(),
            left_out: 0,
        }
    }

    /// Drops files of the wrong kind (counted for the final note).
    fn filter_kind(&mut self, mut files: Vec<MediaFile>) -> Vec<MediaFile> {
        if let Some(kind) = self.only {
            let before = files.len();
            files.retain(|file| file.kind == kind);
            self.left_out += before - files.len();
        }
        files
    }

    /// Adds already-filtered files, skipping keys seen before.
    fn push(&mut self, files: Vec<MediaFile>) {
        for file in files {
            if self.seen.insert(file.key.clone()) {
                self.files.push(file);
            } else {
                tracing::debug!(key = %file.key, "skipping duplicate file");
            }
        }
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    fn finish(mut self, name: String) -> Resolved {
        if self.left_out > 0
            && let Some(kind) = self.only
        {
            let other = match kind {
                MediaKind::Image => "video",
                MediaKind::Video => "image",
            };
            self.notes.push(format!(
                "left out {} {other} file(s) (only {}s requested)",
                self.left_out,
                kind.as_str()
            ));
        }
        Resolved {
            name,
            files: self.files,
            notes: self.notes,
        }
    }
}

/// `items[0]` of `/api/v1/media/{id}/info/`.
async fn fetch_media_item(client: &Client, media_id: &str) -> Result<MediaItem> {
    if media_id.is_empty() || !media_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Parse(format!("invalid media id '{media_id}'")));
    }
    let value = client
        .get_json(&format!("/api/v1/media/{media_id}/info/"))
        .await?;
    let info: MediaInfo = models::parse(value, "media info")?;
    info.items
        .first()
        .and_then(parse_item)
        .ok_or_else(|| Error::NotFound(format!("media {media_id}")))
}

async fn resolve_post(
    client: &Client,
    code: &str,
    index: Option<u32>,
    out: &mut Collector,
) -> Result<String> {
    let media_id = shortcode_to_media_id(code)?;
    let item = match fetch_media_item(client, &media_id.to_string()).await {
        Err(Error::NotFound(_)) => {
            return Err(Error::NotFound(format!(
                "post {code} (deleted, or from a private account you don't follow)"
            )));
        }
        other => other?,
    };
    tracing::info!(shortcode = code, "fetched post");
    let extracted = media_files(&item, &ItemContext::new(Category::Post));
    out.notes.extend(extracted.notes);
    let count = item.carousel_media.len().max(1);
    let mut files = extracted.files;
    if let Some(n) = index {
        files.retain(|file| file.index == n);
        if files.is_empty() {
            return Err(Error::NotFound(format!(
                "item {n} of post {code} (it has {count} item(s))"
            )));
        }
    }
    let files = out.filter_kind(files);
    out.push(files);
    let owner = item
        .author()
        .map(|u| u.username.as_str())
        .unwrap_or_default();
    Ok(if owner.is_empty() {
        format!("post {code}")
    } else {
        format!("{owner} · post {code}")
    })
}

/// `Private` unless the profile is public, followed, or the viewer's own.
fn ensure_visible(client: &Client, user: &UserSummary) -> Result<()> {
    let is_self = client.viewer_id().as_deref() == Some(user.id.as_str());
    if user.is_private && !user.followed_by_viewer && !is_self {
        return Err(Error::Private(format!(
            "@{} is private and you don't follow it",
            user.username
        )));
    }
    Ok(())
}

async fn resolve_profile(
    client: &Client,
    username: &str,
    sections: &[ProfileSection],
    options: &CrawlOptions,
    is_archived: &(dyn Fn(&str) -> bool + Send + Sync),
    out: &mut Collector,
) -> Result<String> {
    let mut unique: Vec<ProfileSection> = Vec::new();
    for section in sections {
        if !unique.contains(section) {
            unique.push(*section);
        }
    }
    if unique.is_empty() {
        return Err(Error::Config("no profile sections selected".into()));
    }
    let user = client.user_by_username(username).await?;
    ensure_visible(client, &user)?;
    for section in &unique {
        let result = match section {
            ProfileSection::Posts => {
                crawl_feed(client, &user, Feed::Posts, options, is_archived, out).await
            }
            ProfileSection::Reels => {
                crawl_feed(client, &user, Feed::Reels, options, is_archived, out).await
            }
            ProfileSection::Tagged => {
                crawl_feed(client, &user, Feed::Tagged, options, is_archived, out).await
            }
            ProfileSection::Stories => crawl_stories(client, &user, None, out).await,
            ProfileSection::Highlights => crawl_highlights(client, &user, options, out).await,
        };
        match result {
            // One missing or malformed section should not sink the others.
            Err(err @ (Error::NotFound(_) | Error::Parse(_))) if unique.len() > 1 => {
                out.note(format!("{section}: skipped ({err})"));
            }
            other => other?,
        }
    }
    let names: Vec<&str> = unique.iter().map(|s| s.as_str()).collect();
    Ok(format!("{} · {}", user.username, names.join(", ")))
}

/// A paginated profile section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    Posts,
    Reels,
    Tagged,
}

impl Feed {
    fn name(self) -> &'static str {
        match self {
            Feed::Posts => "posts",
            Feed::Reels => "reels",
            Feed::Tagged => "tagged",
        }
    }

    fn category(self) -> Category {
        match self {
            Feed::Posts => Category::Post,
            Feed::Reels => Category::Reel,
            Feed::Tagged => Category::Tagged,
        }
    }
}

/// One page of a paginated section.
struct Page {
    items: Vec<Value>,
    more_available: bool,
    next_max_id: Option<String>,
}

async fn fetch_feed_page(
    client: &Client,
    feed: Feed,
    user_id: &str,
    max_id: Option<&str>,
) -> Result<Page> {
    let cursor = max_id
        .map(|m| {
            format!(
                "&max_id={}",
                url::form_urlencoded::byte_serialize(m.as_bytes()).collect::<String>()
            )
        })
        .unwrap_or_default();
    match feed {
        Feed::Posts | Feed::Tagged => {
            let path = if feed == Feed::Posts {
                format!("/api/v1/feed/user/{user_id}/?count={PAGE_SIZE}{cursor}")
            } else {
                format!("/api/v1/usertags/{user_id}/feed/?count={PAGE_SIZE}{cursor}")
            };
            let page: FeedPage = models::parse(client.get_json(&path).await?, feed.name())?;
            Ok(Page {
                items: page.items,
                more_available: page.more_available,
                next_max_id: page.next_max_id,
            })
        }
        Feed::Reels => {
            let mut form = vec![("target_user_id", user_id), ("page_size", PAGE_SIZE)];
            if let Some(max_id) = max_id {
                form.push(("max_id", max_id));
            }
            let value = client.post_form("/api/v1/clips/user/", &form).await?;
            let page: ClipsPage = models::parse(value, "reels")?;
            Ok(Page {
                items: page.items.into_iter().map(|entry| entry.media).collect(),
                more_available: page.paging_info.more_available,
                next_max_id: page.paging_info.max_id,
            })
        }
    }
}

/// Crawls posts / reels / tagged with pagination, filters and limits.
async fn crawl_feed(
    client: &Client,
    user: &UserSummary,
    feed: Feed,
    options: &CrawlOptions,
    is_archived: &(dyn Fn(&str) -> bool + Send + Sync),
    out: &mut Collector,
) -> Result<()> {
    if options.limit == Some(0) {
        return Ok(());
    }
    let section = feed.name();
    let mut kept_posts = 0usize;
    let mut max_id: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page_number = 0u32;
    let mut unreadable = 0usize;
    'pages: loop {
        page_number += 1;
        let page = fetch_feed_page(client, feed, &user.id, max_id.as_deref()).await?;
        tracing::info!(
            user = %user.username,
            section,
            page = page_number,
            items = page.items.len(),
            "fetched page"
        );
        let total = page.items.len();
        for (position, value) in page.items.iter().enumerate() {
            let Some(item) = parse_item(value) else {
                unreadable += 1;
                continue;
            };
            let pinned = item.is_pinned();
            if options.until.is_some_and(|until| item.taken_at > until) {
                continue;
            }
            if options.since.is_some_and(|since| item.taken_at < since) {
                if pinned {
                    continue;
                }
                tracing::debug!(section, "reached posts older than --since");
                break 'pages;
            }
            let ctx = ItemContext {
                category: feed.category(),
                fallback_owner: &user.username,
                fallback_owner_id: Some(&user.id),
                highlight: None,
            };
            let extracted = media_files(&item, &ctx);
            out.notes.extend(extracted.notes);
            let files = out.filter_kind(extracted.files);
            if files.is_empty() {
                continue;
            }
            if options.stop_at_archived && !pinned && files.iter().all(|f| is_archived(&f.key)) {
                let label = item.code.clone().unwrap_or_else(|| item.media_pk());
                out.note(format!(
                    "{section}: stopped at {label}, the first post already downloaded"
                ));
                break 'pages;
            }
            out.push(files);
            kept_posts += 1;
            if options.limit.is_some_and(|limit| kept_posts >= limit) {
                if position + 1 < total || page.more_available {
                    out.note(format!("{section}: limited to {kept_posts} post(s)"));
                }
                break 'pages;
            }
        }
        if !page.more_available {
            break;
        }
        match page.next_max_id {
            Some(next) if seen_cursors.insert(next.clone()) => max_id = Some(next),
            _ => {
                tracing::debug!(section, "pagination cursor missing or repeated; stopping");
                break;
            }
        }
    }
    if unreadable > 0 {
        out.note(format!(
            "{section}: skipped {unreadable} unreadable item(s)"
        ));
    }
    Ok(())
}

/// Current stories of `user`; `story_id` keeps a single item.
async fn crawl_stories(
    client: &Client,
    user: &UserSummary,
    story_id: Option<&str>,
    out: &mut Collector,
) -> Result<()> {
    let value = client
        .get_json(&format!("/api/v1/feed/reels_media/?reel_ids={}", user.id))
        .await?;
    let media: ReelsMedia = models::parse(value, "stories")?;
    let reel = media.find_or_first(&user.id);
    let items = reel.map(|r| r.items.as_slice()).unwrap_or_default();
    tracing::info!(user = %user.username, section = "stories", items = items.len(), "fetched page");
    let missing_story = |id: &str| {
        Error::NotFound(format!(
            "story {id} of @{}: stories expire after 24 hours, so it may be gone",
            user.username
        ))
    };
    if items.is_empty() {
        if let Some(id) = story_id {
            return Err(missing_story(id));
        }
        out.note(format!("@{} has no current stories", user.username));
        return Ok(());
    }
    let owner = reel
        .and_then(|r| r.user.as_ref())
        .map(|u| u.username.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| user.username.clone());
    let mut matched = false;
    for value in items {
        let Some(item) = parse_item(value) else {
            out.note("stories: skipped an unreadable item");
            continue;
        };
        if let Some(id) = story_id
            && item.media_pk() != id
        {
            continue;
        }
        matched = true;
        let ctx = ItemContext {
            category: Category::Story,
            fallback_owner: &owner,
            fallback_owner_id: Some(&user.id),
            highlight: None,
        };
        let extracted = media_files(&item, &ctx);
        out.notes.extend(extracted.notes);
        let files = out.filter_kind(extracted.files);
        out.push(files);
    }
    match story_id {
        Some(id) if !matched => Err(missing_story(id)),
        _ => Ok(()),
    }
}

/// `highlight:N` for a bare or prefixed highlight id.
fn highlight_reel_id(id: &str) -> String {
    let id = id.trim();
    if id.starts_with("highlight:") {
        id.to_string()
    } else {
        format!("highlight:{id}")
    }
}

/// `/api/v1/feed/reels_media/?reel_ids=A&reel_ids=B…`.
async fn fetch_reels(client: &Client, reel_ids: &[&str]) -> Result<ReelsMedia> {
    // The serializer is not `Send`: finish it before the `.await`.
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(reel_ids.iter().map(|id| ("reel_ids", *id)))
        .finish();
    let value = client
        .get_json(&format!("/api/v1/feed/reels_media/?{query}"))
        .await?;
    models::parse(value, "highlights")
}

/// Adds the items of one highlight reel.
fn push_highlight_items(
    out: &mut Collector,
    items: &[Value],
    title: &str,
    owner: &str,
    owner_id: Option<&str>,
) {
    for value in items {
        let Some(item) = parse_item(value) else {
            out.note(format!("highlight '{title}': skipped an unreadable item"));
            continue;
        };
        let ctx = ItemContext {
            category: Category::Highlight,
            fallback_owner: owner,
            fallback_owner_id: owner_id,
            highlight: (!title.is_empty()).then_some(title),
        };
        let extracted = media_files(&item, &ctx);
        out.notes.extend(extracted.notes);
        let files = out.filter_kind(extracted.files);
        out.push(files);
    }
}

/// Every highlight of `user` (at most `options.limit` of them).
async fn crawl_highlights(
    client: &Client,
    user: &UserSummary,
    options: &CrawlOptions,
    out: &mut Collector,
) -> Result<()> {
    let value = client
        .get_json(&format!("/api/v1/highlights/{}/highlights_tray/", user.id))
        .await?;
    let tray: HighlightsTray = models::parse(value, "highlights tray")?;
    let mut entries: Vec<(String, String)> = tray
        .tray
        .into_iter()
        .filter_map(|entry| {
            let id = highlight_reel_id(&entry.id?);
            let title = entry.title.unwrap_or_default();
            Some((id, title))
        })
        .collect();
    tracing::info!(
        user = %user.username,
        section = "highlights",
        highlights = entries.len(),
        "fetched highlights tray"
    );
    if entries.is_empty() {
        out.note(format!("@{} has no highlights", user.username));
        return Ok(());
    }
    if let Some(limit) = options.limit
        && entries.len() > limit
    {
        out.note(format!(
            "highlights: limited to {limit} of {}",
            entries.len()
        ));
        entries.truncate(limit);
    }
    for (chunk_number, chunk) in entries.chunks(HIGHLIGHT_CHUNK).enumerate() {
        let ids: Vec<&str> = chunk.iter().map(|(id, _)| id.as_str()).collect();
        let media = fetch_reels(client, &ids).await?;
        tracing::info!(
            user = %user.username,
            section = "highlights",
            page = chunk_number + 1,
            highlights = chunk.len(),
            "fetched page"
        );
        for (id, tray_title) in chunk {
            let Some(reel) = media.find(id) else {
                out.note(format!("highlight '{tray_title}' returned no items"));
                continue;
            };
            let title = if tray_title.is_empty() {
                reel.title.clone().unwrap_or_default()
            } else {
                tray_title.clone()
            };
            push_highlight_items(out, &reel.items, &title, &user.username, Some(&user.id));
        }
    }
    Ok(())
}

/// A single highlight link.
async fn resolve_highlight(client: &Client, id: &str, out: &mut Collector) -> Result<String> {
    let reel_id = highlight_reel_id(id);
    let numeric = reel_id.trim_start_matches("highlight:").to_string();
    let not_found = || Error::NotFound(format!("highlight {numeric}"));
    let media = match fetch_reels(client, &[&reel_id]).await {
        Err(Error::NotFound(_)) => return Err(not_found()),
        other => other?,
    };
    let reel = media.find_or_first(&reel_id).ok_or_else(not_found)?;
    if reel.items.is_empty() {
        return Err(not_found());
    }
    tracing::info!(highlight = %numeric, items = reel.items.len(), "fetched highlight");
    let title = reel.title.clone().unwrap_or_default();
    let owner = reel
        .user
        .as_ref()
        .map(|u| u.username.clone())
        .unwrap_or_default();
    let owner_id = reel.user.as_ref().and_then(|u| u.user_id());
    push_highlight_items(out, &reel.items, &title, &owner, owner_id.as_deref());
    let label = if title.is_empty() { &numeric } else { &title };
    Ok(if owner.is_empty() {
        format!("highlight {label}")
    } else {
        format!("{owner} · highlight {label}")
    })
}
