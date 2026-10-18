//! API media items → [`crate::model::MediaFile`]s.
//!
//! A carousel (`media_type` 8) yields one file per child; anything else yields one
//! file. Videos use the largest `video_versions` rendition, images the largest
//! `image_versions2` candidate. Items without a usable URL are skipped with a note.

use crate::instagram::models::{MediaItem, Version};
use crate::model::{Category, MediaFile, MediaKind};

/// `media_type` of a carousel post.
pub const MEDIA_TYPE_CAROUSEL: i64 = 8;
/// `media_type` of a video.
pub const MEDIA_TYPE_VIDEO: i64 = 2;

/// Where an item was found; decides [`MediaFile::category`] and fallbacks.
#[derive(Debug, Clone, Copy)]
pub struct ItemContext<'a> {
    /// [`Category::Post`] means "post context": items with `product_type ==
    /// "clips"` become [`Category::Reel`]. Every other category is used as is.
    pub category: Category,
    /// Owner username used when the item carries none (e.g. the profile crawled).
    pub fallback_owner: &'a str,
    /// Owner id used when the item carries none.
    pub fallback_owner_id: Option<&'a str>,
    /// Highlight title for highlight items.
    pub highlight: Option<&'a str>,
}

impl<'a> ItemContext<'a> {
    /// A context with no fallbacks and no highlight title.
    pub fn new(category: Category) -> Self {
        ItemContext {
            category,
            fallback_owner: "",
            fallback_owner_id: None,
            highlight: None,
        }
    }

    /// Sets the fallback owner username.
    pub fn owner(mut self, username: &'a str) -> Self {
        self.fallback_owner = username;
        self
    }

    /// Sets the highlight title.
    pub fn highlight(mut self, title: &'a str) -> Self {
        self.highlight = Some(title);
        self
    }
}

/// The files of one item plus notes about parts that could not be used.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extracted {
    pub files: Vec<MediaFile>,
    pub notes: Vec<String>,
}

/// Turns one API media item into its downloadable files.
pub fn media_files(item: &MediaItem, ctx: &ItemContext<'_>) -> Extracted {
    let post_id = item.media_pk();
    if post_id.is_empty() {
        return Extracted {
            files: Vec::new(),
            notes: vec!["skipped an item without a media id".to_string()],
        };
    }
    let category = match ctx.category {
        Category::Post if item.is_clip() => Category::Reel,
        other => other,
    };
    let author = item.author();
    let owner = author
        .map(|u| u.username.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| ctx.fallback_owner.to_string());
    let owner_id = author
        .and_then(|u| u.user_id())
        .or_else(|| ctx.fallback_owner_id.map(str::to_string));
    // Story and highlight items carry a `code`, but it is not a post shortcode.
    let shortcode = match category {
        Category::Story | Category::Highlight => None,
        _ => item.code.clone(),
    };
    let base = MediaFile {
        key: post_id.clone(),
        post_id: post_id.clone(),
        shortcode,
        index: 1,
        count: 1,
        kind: MediaKind::Image,
        category,
        url: String::new(),
        width: None,
        height: None,
        owner,
        owner_id,
        taken_at: item.taken_at,
        caption: item.caption_text(),
        highlight: ctx.highlight.map(str::to_string),
        ext: String::new(),
    };

    let mut out = Extracted::default();
    let label = item_label(item, &post_id);
    if item.media_type == MEDIA_TYPE_CAROUSEL || !item.carousel_media.is_empty() {
        let count = u32::try_from(item.carousel_media.len()).unwrap_or(u32::MAX);
        if count == 0 {
            out.notes
                .push(format!("skipped {label}: carousel without any media"));
        }
        for (position, child) in item.carousel_media.iter().enumerate() {
            let index = u32::try_from(position + 1).unwrap_or(u32::MAX);
            let child_pk = child.media_pk();
            let key = if child_pk.is_empty() {
                format!("{post_id}_{index}")
            } else {
                child_pk
            };
            match best_rendition(child) {
                Some((kind, version)) => out.files.push(MediaFile {
                    key,
                    index,
                    count,
                    ..with_rendition(&base, kind, version)
                }),
                None => out.notes.push(format!(
                    "skipped {label} item {index}/{count}: no downloadable URL"
                )),
            }
        }
    } else {
        match best_rendition(item) {
            Some((kind, version)) => out.files.push(with_rendition(&base, kind, version)),
            None => out
                .notes
                .push(format!("skipped {label}: no downloadable URL")),
        }
    }
    out
}

/// `post CODE` or `item PK`, for notes.
fn item_label(item: &MediaItem, pk: &str) -> String {
    match item.code.as_deref() {
        Some(code) => format!("post {code}"),
        None => format!("item {pk}"),
    }
}

/// `base` with the URL, size, kind and extension of `version`.
fn with_rendition(base: &MediaFile, kind: MediaKind, version: &Version) -> MediaFile {
    MediaFile {
        kind,
        url: version.url.clone(),
        width: version.width,
        height: version.height,
        ext: extension_for(&version.url, kind),
        ..base.clone()
    }
}

/// The rendition to download: the best video when there is one, else (for
/// non-videos) the best image candidate.
pub fn best_rendition(item: &MediaItem) -> Option<(MediaKind, &Version)> {
    if let Some(video) = best_version(&item.video_versions) {
        return Some((MediaKind::Video, video));
    }
    if item.media_type == MEDIA_TYPE_VIDEO {
        // A video without video URLs: the cover image alone would be misleading.
        return None;
    }
    best_version(&item.image_versions2.candidates).map(|image| (MediaKind::Image, image))
}

/// The version with the largest `width * height` (the first one on ties),
/// ignoring entries without a URL.
pub fn best_version(versions: &[Version]) -> Option<&Version> {
    versions
        .iter()
        .filter(|v| !v.url.trim().is_empty())
        .fold(None, |best: Option<&Version>, v| match best {
            Some(b) if b.area() >= v.area() => Some(b),
            _ => Some(v),
        })
}

/// File extension for `url`: the path's extension when it is 1–5 lowercase
/// alphanumeric characters, else `mp4` for videos and `jpg` for images.
pub fn extension_for(url: &str, kind: MediaKind) -> String {
    let from_path = url::Url::parse(url).ok().and_then(|parsed| {
        let segment = parsed.path_segments()?.next_back()?.to_string();
        let (_, ext) = segment.rsplit_once('.')?;
        let ext = ext.to_ascii_lowercase();
        let valid =
            !ext.is_empty() && ext.len() <= 5 && ext.bytes().all(|b| b.is_ascii_alphanumeric());
        valid.then_some(ext)
    });
    from_path.unwrap_or_else(|| {
        match kind {
            MediaKind::Video => "mp4",
            MediaKind::Image => "jpg",
        }
        .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instagram::models::parse;
    use serde_json::json;

    fn item(value: serde_json::Value) -> MediaItem {
        parse(value, "item").unwrap()
    }

    #[test]
    fn single_image_picks_largest_candidate() {
        let it = item(json!({
            "pk": 10, "id": "10_1", "code": "CODE1", "media_type": 1, "taken_at": 1700000000,
            "user": {"pk": 1, "username": "alice"},
            "caption": {"text": "hello"},
            "image_versions2": {"candidates": [
                {"url": "https://cdn.test/s.jpg", "width": 320, "height": 320},
                {"url": "https://cdn.test/l.webp?x=1", "width": 1080, "height": 1350},
                {"url": "https://cdn.test/m.jpg", "width": 640, "height": 800}
            ]}
        }));
        let out = media_files(&it, &ItemContext::new(Category::Post));
        assert!(out.notes.is_empty());
        let file = &out.files[0];
        assert_eq!(file.url, "https://cdn.test/l.webp?x=1");
        assert_eq!(file.ext, "webp");
        assert_eq!(file.kind, MediaKind::Image);
        assert_eq!((file.key.as_str(), file.post_id.as_str()), ("10", "10"));
        assert_eq!(file.owner, "alice");
        assert_eq!(file.owner_id.as_deref(), Some("1"));
        assert_eq!(file.caption.as_deref(), Some("hello"));
        assert_eq!(file.shortcode.as_deref(), Some("CODE1"));
        assert_eq!((file.index, file.count), (1, 1));
        assert_eq!(file.category, Category::Post);
    }

    #[test]
    fn video_preferred_and_clips_become_reels() {
        let it = item(json!({
            "pk": "11", "media_type": 2, "product_type": "clips",
            "image_versions2": {"candidates": [{"url": "https://cdn.test/cover.jpg", "width": 2000, "height": 2000}]},
            "video_versions": [
                {"url": "https://cdn.test/v1", "width": 480, "height": 854},
                {"url": "https://cdn.test/v2", "width": 720, "height": 1280}
            ]
        }));
        let out = media_files(&it, &ItemContext::new(Category::Post).owner("bob"));
        let file = &out.files[0];
        assert_eq!(file.kind, MediaKind::Video);
        assert_eq!(file.url, "https://cdn.test/v2");
        assert_eq!(file.ext, "mp4");
        assert_eq!(file.category, Category::Reel);
        assert_eq!(file.owner, "bob");
        // Tagged context keeps its category even for clips.
        let tagged = media_files(&it, &ItemContext::new(Category::Tagged));
        assert_eq!(tagged.files[0].category, Category::Tagged);
    }

    #[test]
    fn carousel_children_get_keys_and_indices() {
        let it = item(json!({
            "pk": 20, "code": "CAR", "media_type": 8,
            "carousel_media": [
                {"pk": 21, "media_type": 1, "image_versions2": {"candidates": [{"url": "https://cdn.test/a.jpg", "width": 1, "height": 1}]}},
                {"pk": 22, "media_type": 2, "video_versions": [{"url": "https://cdn.test/b.mp4", "width": 1, "height": 1}]},
                {"pk": 23, "media_type": 1}
            ]
        }));
        let out = media_files(&it, &ItemContext::new(Category::Post));
        assert_eq!(out.files.len(), 2);
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].contains("3/3"));
        let keys: Vec<_> = out
            .files
            .iter()
            .map(|f| (f.key.as_str(), f.post_id.as_str(), f.index, f.count))
            .collect();
        assert_eq!(keys, vec![("21", "20", 1, 3), ("22", "20", 2, 3)]);
    }

    #[test]
    fn stories_have_no_shortcode_and_video_without_url_is_skipped() {
        let it = item(json!({"pk": 30, "code": "STORYCODE", "media_type": 2,
            "image_versions2": {"candidates": [{"url": "https://cdn.test/c.jpg"}]}}));
        let out = media_files(&it, &ItemContext::new(Category::Story));
        assert!(out.files.is_empty());
        assert_eq!(out.notes.len(), 1);
        let it = item(json!({"pk": 31, "code": "STORYCODE", "media_type": 1,
            "image_versions2": {"candidates": [{"url": "https://cdn.test/c.jpg"}]}}));
        let out = media_files(
            &it,
            &ItemContext::new(Category::Highlight).highlight("Trip"),
        );
        assert_eq!(out.files[0].shortcode, None);
        assert_eq!(out.files[0].highlight.as_deref(), Some("Trip"));
    }

    #[test]
    fn items_without_id_are_skipped() {
        let it = item(json!({"media_type": 1,
            "image_versions2": {"candidates": [{"url": "https://cdn.test/c.jpg"}]}}));
        let out = media_files(&it, &ItemContext::new(Category::Post));
        assert!(out.files.is_empty());
        assert_eq!(out.notes.len(), 1);
    }

    #[test]
    fn extensions() {
        assert_eq!(
            extension_for("https://x.test/a/b.HEIC?q=1", MediaKind::Image),
            "heic"
        );
        assert_eq!(extension_for("https://x.test/a/b", MediaKind::Video), "mp4");
        assert_eq!(
            extension_for("https://x.test/a/b.toolong", MediaKind::Image),
            "jpg"
        );
        assert_eq!(
            extension_for("https://x.test/a/b.j-g", MediaKind::Image),
            "jpg"
        );
        assert_eq!(extension_for("not a url", MediaKind::Image), "jpg");
    }
}
