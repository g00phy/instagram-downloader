//! Lenient serde models of Instagram API responses.
//!
//! Instagram changes field types and nullability freely, so every struct uses
//! `#[serde(default)]`, ids accept numbers or strings (always stored as
//! [`String`]), `null` collections become empty, and unknown fields are ignored.
//! Pages keep their items as raw [`serde_json::Value`]s so that one malformed
//! item can be skipped without losing the whole page (see [`parse_item`]).

use std::collections::HashMap;

use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// Deserializes an id given as a string or a number (`null` → empty string).
pub fn de_id<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<String, D::Error> {
    Ok(id_from_value(&Value::deserialize(deserializer)?).unwrap_or_default())
}

/// Like [`de_id`] but keeps "absent" distinct: `null`/empty → `None`.
pub fn de_opt_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Ok(id_from_value(&Value::deserialize(deserializer)?))
}

/// An integer given as a number (integer or float) or a numeric string; anything
/// else becomes `0`.
pub fn de_i64<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<i64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(match &value {
        Value::Number(n) => n
            .as_i64()
            // Truncation of fractional timestamps is intended.
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or_default(),
        Value::String(s) => s.trim().parse().unwrap_or_default(),
        _ => 0,
    })
}

/// A dimension (`width`/`height`) that may be missing, `null`, a float or a string.
pub fn de_opt_u32<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<u32>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(match &value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .and_then(|v| u32::try_from(v).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

/// A boolean that may be `null` (→ `false`).
pub fn de_bool<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<bool, D::Error> {
    Ok(matches!(
        Value::deserialize(deserializer)?,
        Value::Bool(true)
    ))
}

/// A value whose `null` means "default" (empty vector, empty struct, …).
pub fn de_nullable<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// String form of an id value: strings are trimmed, numbers printed; anything
/// else (or an empty string) is `None`.
pub fn id_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Deserializes `value` into `T`, mapping failures to [`Error::Parse`] with a
/// short message naming `what` (serde messages may quote values, so they are
/// truncated and redacted).
pub fn parse<T: DeserializeOwned>(value: Value, what: &str) -> Result<T> {
    serde_json::from_value(value).map_err(|err| {
        let detail = crate::redact::redact(&err.to_string());
        Error::Parse(format!(
            "unexpected {what} response: {}",
            truncate(&detail, 160)
        ))
    })
}

/// Parses one media item, `None` when it is not a usable object.
pub fn parse_item(value: &Value) -> Option<MediaItem> {
    if !value.is_object() {
        return None;
    }
    MediaItem::deserialize(value).ok()
}

/// Truncates `text` to at most `max` characters, appending `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// `user` / `owner` object of a media item or reel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserRef {
    #[serde(deserialize_with = "de_opt_id")]
    pub pk: Option<String>,
    #[serde(deserialize_with = "de_opt_id")]
    pub id: Option<String>,
    #[serde(deserialize_with = "de_nullable")]
    pub username: String,
    pub full_name: Option<String>,
}

impl UserRef {
    /// `pk`, falling back to `id`.
    pub fn user_id(&self) -> Option<String> {
        self.pk.clone().or_else(|| self.id.clone())
    }
}

/// `caption` of a media item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caption {
    #[serde(deserialize_with = "de_nullable")]
    pub text: String,
}

/// One rendition of an image or video.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Version {
    #[serde(deserialize_with = "de_nullable")]
    pub url: String,
    #[serde(deserialize_with = "de_opt_u32")]
    pub width: Option<u32>,
    #[serde(deserialize_with = "de_opt_u32")]
    pub height: Option<u32>,
}

impl Version {
    /// `width * height`, 0 when unknown.
    pub fn area(&self) -> u64 {
        u64::from(self.width.unwrap_or(0)) * u64::from(self.height.unwrap_or(0))
    }
}

/// `image_versions2`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageVersions {
    #[serde(deserialize_with = "de_nullable")]
    pub candidates: Vec<Version>,
}

/// A post, reel, story item or carousel child.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaItem {
    #[serde(deserialize_with = "de_id")]
    pub pk: String,
    /// `"{pk}_{owner_pk}"`.
    #[serde(deserialize_with = "de_id")]
    pub id: String,
    /// Shortcode.
    #[serde(deserialize_with = "de_opt_id")]
    pub code: Option<String>,
    /// 1 image, 2 video, 8 carousel.
    #[serde(deserialize_with = "de_i64")]
    pub media_type: i64,
    #[serde(deserialize_with = "de_i64")]
    pub taken_at: i64,
    pub user: Option<UserRef>,
    pub owner: Option<UserRef>,
    pub caption: Option<Caption>,
    #[serde(deserialize_with = "de_nullable")]
    pub image_versions2: ImageVersions,
    #[serde(deserialize_with = "de_nullable")]
    pub video_versions: Vec<Version>,
    #[serde(deserialize_with = "de_nullable")]
    pub carousel_media: Vec<MediaItem>,
    pub product_type: Option<String>,
    #[serde(deserialize_with = "de_nullable")]
    pub timeline_pinned_user_ids: Vec<Value>,
    #[serde(deserialize_with = "de_nullable")]
    pub clips_tab_pinned_user_ids: Vec<Value>,
}

impl MediaItem {
    /// The media pk; derived from `id` (`"{pk}_{owner}"`) when `pk` is missing.
    pub fn media_pk(&self) -> String {
        if !self.pk.is_empty() {
            return self.pk.clone();
        }
        self.id.split('_').next().unwrap_or_default().to_string()
    }

    /// Pinned to the profile grid or the reels tab (ignored by `--update`/`--since`).
    pub fn is_pinned(&self) -> bool {
        !self.timeline_pinned_user_ids.is_empty() || !self.clips_tab_pinned_user_ids.is_empty()
    }

    /// `user`, falling back to `owner`.
    pub fn author(&self) -> Option<&UserRef> {
        self.user.as_ref().or(self.owner.as_ref())
    }

    /// Whether this is a reel (`product_type == "clips"`).
    pub fn is_clip(&self) -> bool {
        self.product_type.as_deref() == Some("clips")
    }

    /// Non-empty caption text.
    pub fn caption_text(&self) -> Option<String> {
        self.caption
            .as_ref()
            .map(|c| c.text.clone())
            .filter(|t| !t.is_empty())
    }
}

/// `data.user` of `web_profile_info`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ProfileUser {
    #[serde(deserialize_with = "de_opt_id")]
    pub id: Option<String>,
    #[serde(deserialize_with = "de_opt_id")]
    pub pk: Option<String>,
    #[serde(deserialize_with = "de_nullable")]
    pub username: String,
    pub full_name: Option<String>,
    #[serde(deserialize_with = "de_bool")]
    pub is_private: bool,
    #[serde(deserialize_with = "de_bool")]
    pub followed_by_viewer: bool,
}

/// `/api/v1/users/web_profile_info/` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebProfileInfo {
    #[serde(deserialize_with = "de_nullable")]
    pub data: WebProfileData,
}

/// `data` of [`WebProfileInfo`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebProfileData {
    pub user: Option<ProfileUser>,
}

/// `{ "user": … }` responses (`users/{id}/info/`, `accounts/current_user/`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserInfo {
    pub user: Option<ProfileUser>,
}

/// `/api/v1/media/{id}/info/` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MediaInfo {
    #[serde(deserialize_with = "de_nullable")]
    pub items: Vec<Value>,
}

/// Posts feed (`feed/user/{id}/`) and tagged feed (`usertags/{id}/feed/`) page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FeedPage {
    #[serde(deserialize_with = "de_nullable")]
    pub items: Vec<Value>,
    #[serde(deserialize_with = "de_bool")]
    pub more_available: bool,
    #[serde(deserialize_with = "de_opt_id")]
    pub next_max_id: Option<String>,
}

/// `POST /api/v1/clips/user/` page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClipsPage {
    #[serde(deserialize_with = "de_nullable")]
    pub items: Vec<ClipsEntry>,
    #[serde(deserialize_with = "de_nullable")]
    pub paging_info: PagingInfo,
}

/// One entry of [`ClipsPage::items`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClipsEntry {
    pub media: Value,
}

/// `paging_info` of [`ClipsPage`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PagingInfo {
    #[serde(deserialize_with = "de_bool")]
    pub more_available: bool,
    #[serde(deserialize_with = "de_opt_id")]
    pub max_id: Option<String>,
}

/// A story reel or a highlight in a `feed/reels_media/` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Reel {
    /// User id for stories, `highlight:N` for highlights.
    #[serde(deserialize_with = "de_opt_id")]
    pub id: Option<String>,
    pub title: Option<String>,
    pub user: Option<UserRef>,
    #[serde(deserialize_with = "de_nullable")]
    pub items: Vec<Value>,
}

/// `/api/v1/feed/reels_media/` response: `reels_media[]` and/or `reels{id}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReelsMedia {
    #[serde(deserialize_with = "de_nullable")]
    pub reels_media: Vec<Reel>,
    #[serde(deserialize_with = "de_nullable")]
    pub reels: HashMap<String, Reel>,
}

impl ReelsMedia {
    /// The reel with id `reel_id` (from `reels{}` or `reels_media[]`).
    pub fn find(&self, reel_id: &str) -> Option<&Reel> {
        self.reels.get(reel_id).or_else(|| {
            self.reels_media
                .iter()
                .find(|reel| reel.id.as_deref() == Some(reel_id))
        })
    }

    /// The reel with id `reel_id`, else the first one when exactly one reel was
    /// requested.
    pub fn find_or_first(&self, reel_id: &str) -> Option<&Reel> {
        self.find(reel_id)
            .or_else(|| self.reels_media.first())
            .or_else(|| self.reels.values().next())
    }
}

/// `/api/v1/highlights/{id}/highlights_tray/` response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HighlightsTray {
    #[serde(deserialize_with = "de_nullable")]
    pub tray: Vec<TrayEntry>,
}

/// One highlight in [`HighlightsTray`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrayEntry {
    /// `highlight:N`.
    #[serde(deserialize_with = "de_opt_id")]
    pub id: Option<String>,
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_accept_numbers_and_strings() {
        let item: MediaItem = parse(
            json!({"pk": 123, "id": "123_9", "code": "Abc", "taken_at": "1700000000"}),
            "item",
        )
        .unwrap();
        assert_eq!(item.pk, "123");
        assert_eq!(item.taken_at, 1_700_000_000);
        let item: MediaItem = parse(json!({"pk": "456", "id": null}), "item").unwrap();
        assert_eq!(item.pk, "456");
        assert_eq!(item.id, "");
    }

    #[test]
    fn nulls_become_defaults() {
        let item: MediaItem = parse(
            json!({
                "id": "77_1",
                "caption": null,
                "video_versions": null,
                "carousel_media": null,
                "image_versions2": null,
                "user": null,
                "timeline_pinned_user_ids": null,
                "unknown_field": {"x": 1}
            }),
            "item",
        )
        .unwrap();
        assert_eq!(item.media_pk(), "77");
        assert!(item.video_versions.is_empty());
        assert!(item.caption_text().is_none());
        assert!(!item.is_pinned());
    }

    #[test]
    fn pinned_detection() {
        let item: MediaItem =
            parse(json!({"pk": 1, "clips_tab_pinned_user_ids": [5]}), "item").unwrap();
        assert!(item.is_pinned());
    }

    #[test]
    fn reels_lookup() {
        let media: ReelsMedia = parse(
            json!({"reels": {"highlight:5": {"id": "highlight:5", "title": "Trip", "items": []}},
                   "reels_media": [{"id": 42, "items": [{}]}]}),
            "reels",
        )
        .unwrap();
        assert_eq!(
            media.find("highlight:5").unwrap().title.as_deref(),
            Some("Trip")
        );
        assert_eq!(media.find("42").unwrap().items.len(), 1);
        assert!(media.find("7").is_none());
        assert!(media.find_or_first("7").is_some());
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "hé…");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
