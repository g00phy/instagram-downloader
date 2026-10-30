//! Resolving a [`Target`] into the files to download.
//!
//! * Posts: `GET /api/v1/media/{id}/info/`.
//! * Profiles: the profile page (id, tokens), then the web app's GraphQL
//!   operations: the posts grid (items in the v1 media format), the reels and
//!   tagged tabs (shortcodes, then `media/{id}/info/` per post) and the
//!   highlights tray.
//! * Stories and highlight items: `feed/reels_media/` first, the
//!   `PolarisStoriesV3ReelPageGalleryQuery` operation when that endpoint answers
//!   with a 5xx, a redirect or HTML.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::instagram::Client;
use crate::instagram::client::{ApiRequest, CallOptions, UserSummary};
use crate::instagram::doc_ids::{self, Operation};
use crate::instagram::extract::{ItemContext, media_files};
use crate::instagram::models::{self, MediaInfo, MediaItem, Reel, ReelsMedia, parse_item};
use crate::links::{ProfileSection, Target, shortcode_to_media_id};
use crate::model::{Category, MediaFile, MediaKind};

/// Posts requested per GraphQL page (the web app's value).
const PAGE_SIZE: u32 = 12;

/// Highlights fetched per reels request.
const HIGHLIGHT_CHUNK: usize = 5;

/// Most pages read from one stories connection.
const MAX_STORY_PAGES: usize = 50;

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
    let mut session = Session::new(client);
    let name = match target {
        Target::Post { shortcode, index } => {
            resolve_post(client, shortcode, *index, &mut out).await?
        }
        Target::Profile { username, section } => {
            let sections = match section {
                Some(section) => vec![*section],
                None => options.sections.clone(),
            };
            resolve_profile(
                &mut session,
                username,
                &sections,
                options,
                is_archived,
                &mut out,
            )
            .await?
        }
        Target::Stories { username, story_id } => {
            let user = client.user_by_username(username).await?;
            crawl_stories(&mut session, &user, story_id.as_deref(), &mut out).await?;
            format!("{} · stories", user.username)
        }
        Target::Highlight { id } => resolve_highlight(&mut session, id, &mut out).await?,
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

/// State of one `resolve` call.
struct Session<'a> {
    client: &'a Client,
    /// `feed/reels_media/` failed in a way the GraphQL fallback covers: skip it.
    rest_reels_failed: bool,
    /// Reels seen in the posts grid, by shortcode (saves a `media/info` call
    /// each when the reels tab is crawled too).
    known_clips: HashMap<String, MediaItem>,
    keep_clips: bool,
}

impl<'a> Session<'a> {
    fn new(client: &'a Client) -> Self {
        Session {
            client,
            rest_reels_failed: false,
            known_clips: HashMap::new(),
            keep_clips: false,
        }
    }

    /// The media item of a shortcode: a reel already seen in the posts grid,
    /// else `media/{id}/info/`.
    async fn media_by_code(&mut self, code: &str) -> Result<MediaItem> {
        if let Some(item) = self.known_clips.get(code) {
            return Ok(item.clone());
        }
        let media_id = shortcode_to_media_id(code)?;
        fetch_media_item(self.client, &media_id.to_string()).await
    }

    /// Story or highlight reels: `feed/reels_media/` first, the stories
    /// GraphQL operation when that fails with a 5xx, a redirect or HTML.
    async fn reels(&mut self, reel_ids: &[&str], username: Option<&str>) -> Result<ReelsMedia> {
        if !self.rest_reels_failed {
            match rest_reels(self.client, reel_ids).await {
                Ok(media) => return Ok(media),
                Err(err) if rest_reels_fallback(&err) => {
                    tracing::info!(error = %err, "feed/reels_media failed; using the web app's stories query instead");
                    self.rest_reels_failed = true;
                }
                Err(err) => return Err(err),
            }
        }
        graphql_reels(self.client, reel_ids, username).await
    }
}

/// `items[0]` of `/api/v1/media/{id}/info/`. A redirect to the home page or
/// the login page means the cookies were not accepted.
async fn fetch_media_item(client: &Client, media_id: &str) -> Result<MediaItem> {
    if media_id.is_empty() || !media_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Parse(format!("invalid media id '{media_id}'")));
    }
    let path = format!("/api/v1/media/{media_id}/info/");
    let options = CallOptions {
        home_redirect_is_auth: true,
        ..CallOptions::default()
    };
    let value = client.api(&ApiRequest::get(&path).options(options)).await?;
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

/// Whether `user` is the account the cookies belong to.
fn is_viewer(client: &Client, user: &UserSummary) -> bool {
    client.viewer_id().as_deref() == Some(user.id.as_str())
}

/// The error for a section Instagram would not show.
fn private_error(user: &UserSummary, section: &str, detail: Option<&str>) -> Error {
    if user.is_private {
        Error::Private(format!(
            "@{} is private and you don't follow it (or it has no {section})",
            user.username
        ))
    } else {
        let detail = detail.map(|d| format!(": {d}")).unwrap_or_default();
        Error::Private(format!(
            "Instagram refused to show @{}'s {section}{detail}",
            user.username
        ))
    }
}

async fn resolve_profile(
    session: &mut Session<'_>,
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
    let user = session.client.user_by_username(username).await?;
    session.keep_clips =
        unique.contains(&ProfileSection::Posts) && unique.contains(&ProfileSection::Reels);
    for section in &unique {
        let result = match section {
            ProfileSection::Posts => {
                crawl_feed(session, &user, Feed::Posts, options, is_archived, out).await
            }
            ProfileSection::Reels => {
                crawl_feed(session, &user, Feed::Reels, options, is_archived, out).await
            }
            ProfileSection::Tagged => {
                crawl_feed(session, &user, Feed::Tagged, options, is_archived, out).await
            }
            ProfileSection::Stories => crawl_stories(session, &user, None, out).await,
            ProfileSection::Highlights => crawl_highlights(session, &user, options, out).await,
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

    fn operation(self) -> &'static Operation {
        match self {
            Feed::Posts => &doc_ids::POSTS,
            Feed::Reels => &doc_ids::REELS,
            Feed::Tagged => &doc_ids::TAGGED,
        }
    }

    /// The operation's variables (gallery-dl 1.32.17's `user_feed`,
    /// `user_reels`, `user_tagged`).
    fn variables(self, user: &UserSummary, after: Option<&str>) -> Value {
        match self {
            Feed::Posts => json!({
                "after": after,
                "before": null,
                "data": {
                    "count": PAGE_SIZE,
                    "include_reel_media_seen_timestamp": true,
                    "include_relationship_info": true,
                    "latest_besties_reel_media": true,
                    "latest_reel_media": true
                },
                "first": PAGE_SIZE,
                "include_multi_captions": true,
                "last": null,
                "username": user.username,
                "__relay_internal__pv__PolarisMultiCaptionCarouselEnabledrelayprovider": true,
                "__relay_internal__pv__PolarisShortDramaEnabledrelayprovider": true,
                "__relay_internal__pv__PolarisThreadsStyleCommentsEnabledrelayprovider": false,
                "__relay_internal__pv__PolarisReelsRecoDebugOverlayEnabledrelayprovider": false
            }),
            Feed::Reels => json!({
                "after": after,
                "data": {
                    "include_feed_video": true,
                    "page_size": PAGE_SIZE,
                    "target_user_id": user.id
                },
                "first": PAGE_SIZE,
                "id": user.id,
                "__relay_internal__pv__PolarisShortDramaEnabledrelayprovider": false
            }),
            Feed::Tagged => json!({
                "after": after,
                "before": null,
                "count": PAGE_SIZE,
                "first": PAGE_SIZE,
                "last": null,
                "user_id": user.id,
                "__relay_internal__pv__PolarisShortDramaEnabledrelayprovider": true
            }),
        }
    }
}

/// The shortcode of a reels / tagged connection node.
fn node_code(node: &Value) -> Option<String> {
    node.get("media")
        .and_then(|media| media.get("code"))
        .or_else(|| node.get("code"))
        .and_then(Value::as_str)
        .filter(|code| {
            !code.is_empty()
                && code
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .map(str::to_string)
}

/// Crawls posts / reels / tagged with pagination, filters and limits.
async fn crawl_feed(
    session: &mut Session<'_>,
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
    let viewer = is_viewer(session.client, user);
    let mut kept_posts = 0usize;
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    let mut page_number = 0u32;
    let mut unreadable = 0usize;
    let mut unavailable = 0usize;
    'pages: loop {
        page_number += 1;
        let variables = feed.variables(user, cursor.as_deref());
        let page = match session
            .client
            .graphql_connection(feed.operation(), &variables, Some(&user.username))
            .await
        {
            Err(Error::Private(detail)) if !viewer => {
                return Err(private_error(user, section, Some(&detail)));
            }
            other => other?,
        };
        tracing::info!(
            user = %user.username,
            section,
            page = page_number,
            items = page.nodes.len(),
            "fetched page"
        );
        if page_number == 1 && page.nodes.is_empty() && !page.has_next && user.is_private && !viewer
        {
            return Err(private_error(user, section, None));
        }
        let total = page.nodes.len();
        for (position, node) in page.nodes.iter().enumerate() {
            let item = match feed {
                Feed::Posts => match parse_item(node) {
                    Some(item) => {
                        if session.keep_clips
                            && item.is_clip()
                            && let Some(code) = &item.code
                        {
                            session.known_clips.insert(code.clone(), item.clone());
                        }
                        item
                    }
                    None => {
                        unreadable += 1;
                        continue;
                    }
                },
                Feed::Reels | Feed::Tagged => {
                    let Some(code) = node_code(node) else {
                        unreadable += 1;
                        continue;
                    };
                    match session.media_by_code(&code).await {
                        Ok(item) => item,
                        Err(Error::NotFound(_) | Error::Unsupported(_)) => {
                            tracing::debug!(section, code = %code, "post not available");
                            unavailable += 1;
                            continue;
                        }
                        Err(err) => return Err(err),
                    }
                }
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
                if position + 1 < total || page.has_next {
                    out.note(format!("{section}: limited to {kept_posts} post(s)"));
                }
                break 'pages;
            }
        }
        if !page.has_next {
            break;
        }
        match page.end_cursor {
            Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
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
    if unavailable > 0 {
        out.note(format!(
            "{section}: skipped {unavailable} post(s) that are no longer available"
        ));
    }
    Ok(())
}

/// Whether a failed `feed/reels_media/` call is worth the GraphQL fallback:
/// a 5xx, a redirect, or a body that is not the expected JSON.
fn rest_reels_fallback(err: &Error) -> bool {
    match err {
        Error::Http { status, .. } => *status >= 500 || (300..400).contains(status),
        Error::Parse(_) => true,
        _ => false,
    }
}

/// `/api/v1/feed/reels_media/?reel_ids=A&reel_ids=B…` (one attempt: the
/// fallback is cheaper than retrying an endpoint that answers with 500s).
async fn rest_reels(client: &Client, reel_ids: &[&str]) -> Result<ReelsMedia> {
    // The serializer is not `Send`: finish it before the `.await`.
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(reel_ids.iter().map(|id| ("reel_ids", *id)))
        .finish();
    let path = format!("/api/v1/feed/reels_media/?{query}");
    let options = CallOptions {
        retry_transient: false,
        ..CallOptions::default()
    };
    let value = client.api(&ApiRequest::get(&path).options(options)).await?;
    models::parse(value, "stories")
}

/// The reels of `reel_ids` via `PolarisStoriesV3ReelPageGalleryQuery`.
async fn graphql_reels(
    client: &Client,
    reel_ids: &[&str],
    username: Option<&str>,
) -> Result<ReelsMedia> {
    let Some(first) = reel_ids.first() else {
        return Ok(ReelsMedia::default());
    };
    let mut reels: Vec<Reel> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors: HashSet<String> = HashSet::new();
    for _ in 0..MAX_STORY_PAGES {
        let variables = json!({
            "after": cursor,
            "first": 3,
            "initial_reel_id": first,
            "last": 2,
            "reel_ids": reel_ids,
            "__relay_internal__pv__PolarisCommunityNoteStoriesLabelEnabledrelayprovider": true
        });
        let page = client
            .graphql_connection(&doc_ids::STORIES, &variables, username)
            .await?;
        for node in page.nodes {
            match serde_json::from_value::<Reel>(node) {
                Ok(reel) => reels.push(reel),
                Err(err) => tracing::debug!(error = %err, "skipping an unreadable reel"),
            }
        }
        match page.end_cursor {
            Some(next) if page.has_next && seen_cursors.insert(next.clone()) => {
                cursor = Some(next);
            }
            _ => break,
        }
    }
    Ok(ReelsMedia {
        reels_media: reels,
        reels: HashMap::new(),
    })
}

/// Current stories of `user`; `story_id` keeps a single item.
async fn crawl_stories(
    session: &mut Session<'_>,
    user: &UserSummary,
    story_id: Option<&str>,
    out: &mut Collector,
) -> Result<()> {
    let media = session
        .reels(&[user.id.as_str()], Some(&user.username))
        .await?;
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
        let hidden = if user.is_private && !is_viewer(session.client, user) {
            " (it is private: stories show only to followers)"
        } else {
            ""
        };
        out.note(format!("@{} has no current stories{hidden}", user.username));
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

/// `(highlight:N, title)` of the highlights tray operation's result.
fn tray_entries(data: &Value) -> Vec<(String, String)> {
    data.get("highlights")
        .and_then(|highlights| highlights.get("edges"))
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|edge| {
                    let node = edge.get("node")?;
                    let id = models::id_from_value(node.get("id")?)?;
                    let title = node
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((highlight_reel_id(&id), title))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every highlight of `user` (at most `options.limit` of them).
async fn crawl_highlights(
    session: &mut Session<'_>,
    user: &UserSummary,
    options: &CrawlOptions,
    out: &mut Collector,
) -> Result<()> {
    let viewer = is_viewer(session.client, user);
    let variables = json!({"user_id": user.id});
    let data = match session
        .client
        .graphql(&doc_ids::HIGHLIGHTS_TRAY, &variables, Some(&user.username))
        .await
    {
        Err(Error::Private(detail)) if !viewer => {
            return Err(private_error(user, "highlights", Some(&detail)));
        }
        other => other?,
    };
    let mut entries = tray_entries(&data);
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
        let media = session.reels(&ids, Some(&user.username)).await?;
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
async fn resolve_highlight(
    session: &mut Session<'_>,
    id: &str,
    out: &mut Collector,
) -> Result<String> {
    let reel_id = highlight_reel_id(id);
    let numeric = reel_id.trim_start_matches("highlight:").to_string();
    let not_found = || Error::NotFound(format!("highlight {numeric}"));
    let media = match session.reels(&[&reel_id], None).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_codes() {
        assert_eq!(
            node_code(&json!({"media": {"code": "Abc_-1"}})).as_deref(),
            Some("Abc_-1")
        );
        assert_eq!(node_code(&json!({"code": "Xyz"})).as_deref(), Some("Xyz"));
        assert_eq!(node_code(&json!({"code": "../etc"})), None);
        assert_eq!(node_code(&json!({"media": {}})), None);
    }

    #[test]
    fn tray_entries_get_highlight_ids() {
        let data = json!({"highlights": {"edges": [
            {"node": {"id": "highlight:17", "title": "Trip"}},
            {"node": {"id": 18}},
            {"node": {"title": "no id"}}
        ]}});
        assert_eq!(
            tray_entries(&data),
            vec![
                ("highlight:17".to_string(), "Trip".to_string()),
                ("highlight:18".to_string(), String::new())
            ]
        );
        assert!(tray_entries(&json!({"highlights": null})).is_empty());
    }

    #[test]
    fn fallback_errors() {
        let http = |status| Error::Http {
            status,
            message: String::new(),
        };
        assert!(rest_reels_fallback(&http(500)));
        assert!(rest_reels_fallback(&http(302)));
        assert!(rest_reels_fallback(&Error::Parse("html".into())));
        assert!(!rest_reels_fallback(&http(404)));
        assert!(!rest_reels_fallback(&Error::RateLimited("x".into())));
        assert!(!rest_reels_fallback(&Error::Auth("x".into())));
    }

    #[test]
    fn variables_follow_gallery_dl() {
        let user = UserSummary {
            id: "1001".into(),
            username: "alice".into(),
            full_name: None,
            is_private: false,
            followed_by_viewer: false,
        };
        let posts = Feed::Posts.variables(&user, None);
        assert_eq!(posts["username"], "alice");
        assert_eq!(posts["after"], Value::Null);
        assert_eq!(posts["data"]["count"], 12);
        let reels = Feed::Reels.variables(&user, Some("c1"));
        assert_eq!(reels["data"]["target_user_id"], "1001");
        assert_eq!(reels["after"], "c1");
        assert_eq!(Feed::Tagged.variables(&user, None)["user_id"], "1001");
    }
}
