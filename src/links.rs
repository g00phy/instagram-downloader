//! Turning user input (URLs, `@handles`, links files, clipboard text) into [`Target`]s.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Base64 alphabet Instagram uses for shortcodes.
const SHORTCODE_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Private posts carry this many extra characters after the real shortcode.
const PRIVATE_SHORTCODE_SUFFIX: usize = 28;

/// A part of a profile that can be crawled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSection {
    Posts,
    Reels,
    Tagged,
    Stories,
    Highlights,
}

impl ProfileSection {
    /// All sections, in crawl order.
    pub const ALL: [ProfileSection; 5] = [
        ProfileSection::Posts,
        ProfileSection::Reels,
        ProfileSection::Tagged,
        ProfileSection::Stories,
        ProfileSection::Highlights,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProfileSection::Posts => "posts",
            ProfileSection::Reels => "reels",
            ProfileSection::Tagged => "tagged",
            ProfileSection::Stories => "stories",
            ProfileSection::Highlights => "highlights",
        }
    }
}

impl fmt::Display for ProfileSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProfileSection {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        ProfileSection::ALL
            .into_iter()
            .find(|section| section.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| {
                Error::Config(format!(
                    "unknown profile section '{s}' (expected posts, reels, tagged, stories or highlights)"
                ))
            })
    }
}

/// What a link points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// `/p/<code>/`, `/reel/<code>/`, `/tv/<code>/`. `index` comes from
    /// `?img_index=N` (1-based) and selects a single carousel item.
    Post {
        shortcode: String,
        index: Option<u32>,
    },
    /// `/<user>/` (section `None`: use the crawl options' sections),
    /// `/<user>/reels/`, `/<user>/tagged/`.
    Profile {
        username: String,
        section: Option<ProfileSection>,
    },
    /// `/stories/<user>/` or one story item `/stories/<user>/<id>/`.
    Stories {
        username: String,
        story_id: Option<String>,
    },
    /// `/stories/highlights/<id>/` (numeric id without the `highlight:` prefix).
    Highlight { id: String },
}

impl Target {
    /// Canonical `https://www.instagram.com/...` URL for this target.
    pub fn canonical_url(&self) -> String {
        todo!("links agent")
    }

    /// Default package name, e.g. `post C0ffeeXYZ12`, `natgeo · posts`,
    /// `natgeo · stories`, `highlight 17912345678901234`.
    pub fn describe(&self) -> String {
        todo!("links agent")
    }
}

/// Parses one link: full/partial Instagram URLs (with or without scheme, `www.`,
/// `m.`, `instagr.am`), `@username` or a bare username. Tracking parameters are
/// ignored. Anything else is [`Error::Unsupported`].
pub fn parse_link(input: &str) -> Result<Target> {
    let _ = input;
    todo!("links agent")
}

/// Links-file semantics: one entry per line, blank lines and `#` comments skipped,
/// Markdown (`[t](url)`, `<url>`, `- url`, `1. url`) unwrapped, surrounding
/// whitespace/quotes trimmed. Entries are returned as written (not parsed).
pub fn read_links(text: &str) -> Vec<String> {
    let _ = text;
    todo!("links agent")
}

/// Finds Instagram URLs anywhere in free text (clipboard contents), in order,
/// without duplicates. Bare usernames are *not* matched here.
pub fn extract_links(text: &str) -> Vec<String> {
    let _ = text;
    todo!("links agent")
}

/// Decodes a shortcode to its numeric media id. Private-post shortcodes longer
/// than 28 characters have their 28-character suffix removed first.
pub fn shortcode_to_media_id(shortcode: &str) -> Result<u64> {
    let code = if shortcode.len() > PRIVATE_SHORTCODE_SUFFIX {
        &shortcode[..shortcode.len() - PRIVATE_SHORTCODE_SUFFIX]
    } else {
        shortcode
    };
    if code.is_empty() {
        return Err(Error::Unsupported("empty shortcode".into()));
    }
    let mut id: u64 = 0;
    for byte in code.bytes() {
        let digit = SHORTCODE_ALPHABET
            .iter()
            .position(|&c| c == byte)
            .ok_or_else(|| Error::Unsupported(format!("invalid shortcode '{shortcode}'")))?;
        id = id
            .checked_mul(64)
            .and_then(|v| v.checked_add(digit as u64))
            .ok_or_else(|| Error::Unsupported(format!("shortcode '{shortcode}' is too long")))?;
    }
    Ok(id)
}

/// Encodes a numeric media id as a shortcode.
pub fn media_id_to_shortcode(mut id: u64) -> String {
    if id == 0 {
        return "A".to_string();
    }
    let mut out = Vec::new();
    while id > 0 {
        out.push(SHORTCODE_ALPHABET[(id % 64) as usize]);
        id /= 64;
    }
    out.reverse();
    // The alphabet is ASCII, so this cannot fail.
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcode_round_trip() {
        // "B_K4CykAOtf" read as base-64 digits over SHORTCODE_ALPHABET.
        assert_eq!(
            shortcode_to_media_id("B_K4CykAOtf").unwrap(),
            2_290_889_842_806_025_055
        );
        assert_eq!(
            media_id_to_shortcode(2_290_889_842_806_025_055),
            "B_K4CykAOtf"
        );
    }

    #[test]
    fn private_shortcode_suffix_is_ignored() {
        let long = format!("B_K4CykAOtf{}", "x".repeat(28));
        assert_eq!(
            shortcode_to_media_id(&long).unwrap(),
            2_290_889_842_806_025_055
        );
    }

    #[test]
    fn invalid_shortcodes_are_rejected() {
        assert!(shortcode_to_media_id("").is_err());
        assert!(shortcode_to_media_id("abc!").is_err());
    }

    #[test]
    fn sections_parse_case_insensitively() {
        assert_eq!(
            "Stories".parse::<ProfileSection>().unwrap(),
            ProfileSection::Stories
        );
        assert!("igtv".parse::<ProfileSection>().is_err());
    }
}
