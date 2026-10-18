//! Turning user input (URLs, `@handles`, links files, clipboard text) into [`Target`]s.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use url::Url;

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
        const BASE: &str = "https://www.instagram.com";
        match self {
            Target::Post { shortcode, index } => match index {
                Some(n) => format!("{BASE}/p/{shortcode}/?img_index={n}"),
                None => format!("{BASE}/p/{shortcode}/"),
            },
            Target::Profile { username, section } => match section {
                None | Some(ProfileSection::Posts) | Some(ProfileSection::Highlights) => {
                    format!("{BASE}/{username}/")
                }
                Some(ProfileSection::Reels) => format!("{BASE}/{username}/reels/"),
                Some(ProfileSection::Tagged) => format!("{BASE}/{username}/tagged/"),
                Some(ProfileSection::Stories) => format!("{BASE}/stories/{username}/"),
            },
            Target::Stories { username, story_id } => match story_id {
                Some(id) => format!("{BASE}/stories/{username}/{id}/"),
                None => format!("{BASE}/stories/{username}/"),
            },
            Target::Highlight { id } => format!("{BASE}/stories/highlights/{id}/"),
        }
    }

    /// Default package name, e.g. `post C0ffeeXYZ12`, `natgeo · posts`,
    /// `natgeo · stories`, `highlight 17912345678901234`.
    pub fn describe(&self) -> String {
        match self {
            Target::Post { shortcode, index } => match index {
                Some(n) => format!("post {shortcode} #{n}"),
                None => format!("post {shortcode}"),
            },
            Target::Profile { username, section } => match section {
                Some(section) => format!("{username} · {section}"),
                None => username.clone(),
            },
            Target::Stories { username, story_id } => match story_id {
                Some(id) => format!("{username} · story {id}"),
                None => format!("{username} · stories"),
            },
            Target::Highlight { id } => format!("highlight {id}"),
        }
    }
}

/// Parses one link: full/partial Instagram URLs (with or without scheme, `www.`,
/// `m.`, `instagr.am`), `@username` or a bare username. Tracking parameters are
/// ignored. Anything else is [`Error::Unsupported`].
pub fn parse_link(input: &str) -> Result<Target> {
    let link = unwrap_link(input);
    if link.is_empty() {
        return Err(unsupported("empty link"));
    }
    if let Some(name) = link.strip_prefix('@') {
        return normalize_username(name)
            .map(profile)
            .ok_or_else(|| unsupported(format!("invalid username '@{}'", shorten(name))));
    }
    if let Some(id) = strip_prefix_ignore_case(link, "highlight:") {
        return Ok(Target::Highlight {
            id: highlight_id(id)?,
        });
    }
    if link.chars().all(is_username_char) && !contains_instagram_domain(link) {
        return normalize_username(link).map(profile).ok_or_else(|| {
            unsupported(format!(
                "'{}' is not a valid username (at most {MAX_USERNAME_LEN} characters)",
                shorten(link)
            ))
        });
    }
    parse_url(link)
}

/// Links-file semantics: one entry per line, blank lines and `#` comments skipped,
/// Markdown (`[t](url)`, `<url>`, `- url`, `1. url`) unwrapped, surrounding
/// whitespace/quotes trimmed. Entries are returned as written (not parsed).
///
/// A line containing Instagram URLs yields those URLs (see [`extract_links`]);
/// any other line yields its whitespace- or comma-separated tokens (`@natgeo`,
/// `natgeo`), up to a `#` comment.
pub fn read_links(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_start_matches(BOM).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let urls = extract_links(line);
        if !urls.is_empty() {
            entries.extend(urls);
            continue;
        }
        let body = strip_list_markers(line);
        let body = markdown_target(body).unwrap_or(body);
        for token in body.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
            if token.starts_with('#') {
                break;
            }
            let entry =
                unwrap_link(token).trim_matches(|c: char| is_quote(c) || c == '<' || c == '>');
            if !entry.is_empty() {
                entries.push(entry.to_string());
            }
        }
    }
    entries
}

/// Finds Instagram URLs anywhere in free text (clipboard contents), in order,
/// without duplicates. Bare usernames are *not* matched here.
///
/// Matches `instagram.com/…` and `instagr.am/…` with an optional `http(s)://`
/// scheme and `www.`/`m.` prefix. A URL ends at whitespace or one of
/// ``"'<>()[]{}`|\``; trailing `.,;:!?*` are dropped and `&amp;` is decoded.
/// Results are normalised to `https://` with a lowercase host; links to the bare
/// home page are skipped.
pub fn extract_links(text: &str) -> Vec<String> {
    // ASCII lowercasing keeps every byte offset valid for `text` as well.
    let lower = text.to_ascii_lowercase();
    let mut hits: Vec<(usize, &str)> = LINK_DOMAINS
        .iter()
        .flat_map(|domain| lower.match_indices(domain).map(|(pos, _)| (pos, *domain)))
        .collect();
    hits.sort_unstable_by_key(|&(pos, _)| pos);

    let mut links: Vec<String> = Vec::new();
    let mut consumed = 0;
    for (pos, domain) in hits {
        if pos < consumed {
            continue;
        }
        if let Some((end, link)) = link_at(text, &lower, pos, domain) {
            consumed = end;
            if !links.contains(&link) {
                links.push(link);
            }
        }
    }
    links
}

/// Hosts [`parse_link`] accepts (compared lowercase).
const INSTAGRAM_HOSTS: [&str; 5] = [
    "instagram.com",
    "www.instagram.com",
    "m.instagram.com",
    "instagr.am",
    "www.instagr.am",
];

/// Registrable domains [`extract_links`] looks for.
const LINK_DOMAINS: [&str; 2] = ["instagram.com", "instagr.am"];

/// Longest username Instagram allows.
const MAX_USERNAME_LEN: usize = 30;

/// Byte-order mark some editors put at the start of text files.
const BOM: char = '\u{feff}';

fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
}

fn profile(username: String) -> Target {
    Target::Profile {
        username,
        section: None,
    }
}

/// Shortens user input quoted in error messages.
fn shorten(input: &str) -> String {
    const MAX_CHARS: usize = 60;
    if input.chars().count() <= MAX_CHARS {
        input.to_string()
    } else {
        let head: String = input.chars().take(MAX_CHARS).collect();
        format!("{head}…")
    }
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn is_username_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '_'
}

fn is_shortcode_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’')
}

/// Lowercased username if `name` has 1–30 characters from `A-Za-z0-9._`.
fn normalize_username(name: &str) -> Option<String> {
    let valid = (1..=MAX_USERNAME_LEN).contains(&name.len()) && name.chars().all(is_username_char);
    valid.then(|| name.to_ascii_lowercase())
}

fn contains_instagram_domain(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    LINK_DOMAINS.iter().any(|domain| lower.contains(domain))
}

/// Removes whitespace, quotes, `<…>` and Markdown `[text](url)` around one link,
/// repeatedly, so `"<https://…>"` and `[t](<https://…>)` unwrap too.
fn unwrap_link(input: &str) -> &str {
    const PAIRS: [(char, char); 6] = [
        ('"', '"'),
        ('\'', '\''),
        ('`', '`'),
        ('<', '>'),
        ('“', '”'),
        ('‘', '’'),
    ];
    let mut s = input.trim_start_matches(BOM).trim();
    loop {
        let before = s.len();
        if let Some(target) = markdown_target(s) {
            s = target;
        }
        for (open, close) in PAIRS {
            if let Some(inner) = s
                .strip_prefix(open)
                .and_then(|rest| rest.strip_suffix(close))
            {
                s = inner.trim();
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

/// The URL of a Markdown link `[text](url "title")`, if `s` is exactly one.
fn markdown_target(s: &str) -> Option<&str> {
    if !s.starts_with('[') {
        return None;
    }
    let inner = s.strip_suffix(')')?;
    let open = inner.rfind("](")?;
    let target = inner[open + 2..].trim();
    // Drop an optional link title after the URL.
    Some(target.split_whitespace().next().unwrap_or_default())
}

/// Strips Markdown list / quote / task markers: `- `, `* `, `+ `, `1. `, `1) `,
/// `> `, `[ ] `, `[x] ` (repeatedly, so `> - 1. x` works).
fn strip_list_markers(line: &str) -> &str {
    let mut s = line;
    loop {
        let before = s.len();
        if let Some(rest) = s.strip_prefix('>') {
            s = rest.trim_start();
        }
        if let Some(rest) = s.strip_prefix(['-', '*', '+'])
            && rest.starts_with(char::is_whitespace)
        {
            s = rest.trim_start();
        }
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && let Some(rest) = s[digits..].strip_prefix(['.', ')'])
            && rest.starts_with(char::is_whitespace)
        {
            s = rest.trim_start();
        }
        for task in ["[ ]", "[x]", "[X]"] {
            if let Some(rest) = s.strip_prefix(task)
                && rest.starts_with(char::is_whitespace)
            {
                s = rest.trim_start();
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

/// The URL whose registrable domain starts at byte `pos` of `text`, with the
/// byte offset where its raw span ends. `None` if `pos` is not a real link
/// (`notinstagram.com`, `instagram.company.com`, `x@instagram.com`, …).
fn link_at(text: &str, lower: &str, pos: usize, domain: &str) -> Option<(usize, String)> {
    let domain_end = pos + domain.len();
    if lower.as_bytes().get(domain_end) != Some(&b'/') {
        return None;
    }
    let host_start = ["www.", "m."]
        .iter()
        .find(|prefix| lower[..pos].ends_with(*prefix))
        .map_or(pos, |prefix| pos - prefix.len());
    let has_scheme = ["https://", "http://"]
        .iter()
        .any(|scheme| lower[..host_start].ends_with(scheme));
    if !has_scheme
        && let Some(prev) = text[..host_start].chars().next_back()
        && (prev.is_alphanumeric() || matches!(prev, '.' | '-' | '_' | '@' | '/'))
    {
        return None;
    }

    let rest = &text[domain_end..];
    let len = rest
        .find(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '|' | '\\'
                )
        })
        .unwrap_or(rest.len());
    let end = domain_end + len;
    let path = rest[..len].trim_end_matches(['.', ',', ';', ':', '!', '?', '*']);
    let path_only = path.split(['?', '#']).next().unwrap_or_default();
    if path_only == "/" {
        return None;
    }
    let host = &lower[host_start..domain_end];
    Some((end, format!("https://{host}{}", path.replace("&amp;", "&"))))
}

/// Prefixes `https://` unless `link` already has a scheme (`//host` gets `https:`).
fn with_scheme(link: &str) -> String {
    if let Some(rest) = link.strip_prefix("//") {
        return format!("https://{rest}");
    }
    let has_scheme = link.find("://").is_some_and(|pos| {
        let scheme = &link[..pos];
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if has_scheme {
        link.to_string()
    } else {
        format!("https://{link}")
    }
}

fn parse_url(link: &str) -> Result<Target> {
    let url = Url::parse(&with_scheme(link)).map_err(|_| {
        unsupported(format!(
            "'{}' is neither an Instagram link nor a username",
            shorten(link)
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(unsupported(format!(
            "unsupported URL scheme '{}'",
            url.scheme()
        )));
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !INSTAGRAM_HOSTS.contains(&host.as_str()) {
        return Err(unsupported(format!("not an Instagram link: {host}")));
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let img_index = url
        .query_pairs()
        .find(|(key, _)| key == "img_index")
        .and_then(|(_, value)| value.parse::<u32>().ok())
        .filter(|&n| n >= 1);
    target_from_segments(&segments, img_index)
}

fn target_from_segments(segments: &[&str], img_index: Option<u32>) -> Result<Target> {
    let Some((first, rest)) = segments.split_first() else {
        return Err(unsupported(
            "the Instagram home page is not a post, profile, story or highlight link",
        ));
    };
    let keyword = first.to_ascii_lowercase();
    match keyword.as_str() {
        "p" | "reel" | "reels" | "tv" => post_target(&keyword, rest, img_index),
        "stories" => stories_target(rest),
        other => match reserved_page_reason(other) {
            Some(reason) => Err(unsupported(reason)),
            None => profile_target(first, rest, img_index),
        },
    }
}

/// `/<kind>/<code>/…` with `kind` one of `p`, `reel`, `reels`, `tv`.
fn post_target(kind: &str, rest: &[&str], index: Option<u32>) -> Result<Target> {
    match rest.first() {
        None if kind == "reels" => Err(unsupported(
            "the reels feed is not supported; use instagram.com/<user>/reels/ for a profile's reels",
        )),
        None => Err(unsupported(format!("/{kind}/ link has no post shortcode"))),
        Some(code) if code.eq_ignore_ascii_case("audio") => {
            Err(unsupported("reel audio pages are not supported"))
        }
        Some(code) => Ok(Target::Post {
            shortcode: validate_shortcode(code)?,
            index,
        }),
    }
}

fn validate_shortcode(code: &str) -> Result<String> {
    if code.chars().all(is_shortcode_char) && shortcode_to_media_id(code).is_ok() {
        Ok(code.to_string())
    } else {
        Err(unsupported(format!(
            "invalid post shortcode '{}'",
            shorten(code)
        )))
    }
}

/// `/stories/<user>/[<id>/]` and `/stories/highlights/<id>/`.
fn stories_target(rest: &[&str]) -> Result<Target> {
    match rest {
        [] => Err(unsupported("stories link has no username")),
        [first, more @ ..] if first.eq_ignore_ascii_case("highlights") => match more.first() {
            Some(id) => Ok(Target::Highlight {
                id: highlight_id(id)?,
            }),
            None => Err(unsupported("highlight link has no id")),
        },
        [user, more @ ..] => {
            let username = normalize_username(user).ok_or_else(|| {
                unsupported(format!(
                    "invalid username '{}' in stories link",
                    shorten(user)
                ))
            })?;
            let story_id = match more.first() {
                Some(id) if is_numeric_id(id) => Some((*id).to_string()),
                Some(id) => {
                    return Err(unsupported(format!("invalid story id '{}'", shorten(id))));
                }
                None => None,
            };
            Ok(Target::Stories { username, story_id })
        }
    }
}

/// `/<user>/`, `/<user>/reels/`, `/<user>/tagged/`, `/<user>/p|reel|tv/<code>/`.
fn profile_target(user: &str, rest: &[&str], index: Option<u32>) -> Result<Target> {
    if let Some((kind, after)) = rest.split_first() {
        let kind = kind.to_ascii_lowercase();
        if matches!(kind.as_str(), "p" | "reel" | "tv") {
            return post_target(&kind, after, index);
        }
    }
    let username = normalize_username(user).ok_or_else(|| {
        unsupported(format!(
            "'{}' is not a valid Instagram username",
            shorten(user)
        ))
    })?;
    let section = match rest {
        [] => None,
        [tab] if tab.eq_ignore_ascii_case("reels") => Some(ProfileSection::Reels),
        [tab] if tab.eq_ignore_ascii_case("tagged") => Some(ProfileSection::Tagged),
        [tab, ..] if tab.eq_ignore_ascii_case("saved") => {
            return Err(unsupported("saved collections are not supported"));
        }
        [tab, ..] => {
            return Err(unsupported(format!(
                "instagram.com/{username}/{}/ is not supported (use the profile, /reels/ or /tagged/)",
                shorten(tab)
            )));
        }
    };
    Ok(Target::Profile { username, section })
}

/// Why a first path segment that is an Instagram page (not a username) is unsupported.
fn reserved_page_reason(segment: &str) -> Option<String> {
    let reason = match segment {
        "explore" | "locations" | "topics" | "directory" => {
            "explore, hashtag and location pages are not supported"
        }
        "accounts" | "challenge" | "session" | "oauth" | "emails" => {
            "account and login pages are not supported"
        }
        "direct" => "direct messages are not supported",
        "share" | "s" => {
            "share links are not supported; open the link in a browser and copy the address it redirects to"
        }
        "about" | "developer" | "legal" | "web" | "api" | "graphql" | "privacy" | "terms"
        | "lite" | "nametag" | "invites" | "static" | "press" | "create" | "settings" => {
            return Some(format!(
                "instagram.com/{segment}/ is not a post, profile, story or highlight link"
            ));
        }
        _ => return None,
    };
    Some(reason.to_string())
}

fn is_numeric_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Numeric highlight id, accepting a `highlight:` (or URL-encoded `highlight%3A`) prefix.
fn highlight_id(raw: &str) -> Result<String> {
    let id = strip_prefix_ignore_case(raw, "highlight:")
        .or_else(|| strip_prefix_ignore_case(raw, "highlight%3A"))
        .unwrap_or(raw);
    if is_numeric_id(id) {
        Ok(id.to_string())
    } else {
        Err(unsupported(format!(
            "invalid highlight id '{}'",
            shorten(raw)
        )))
    }
}

/// Decodes a shortcode to its numeric media id. Private-post shortcodes longer
/// than 28 characters have their 28-character suffix removed first.
pub fn shortcode_to_media_id(shortcode: &str) -> Result<u64> {
    let code = if shortcode.len() > PRIVATE_SHORTCODE_SUFFIX {
        // `get` instead of indexing: non-ASCII input may not split on a char boundary.
        shortcode
            .get(..shortcode.len() - PRIVATE_SHORTCODE_SUFFIX)
            .ok_or_else(|| Error::Unsupported(format!("invalid shortcode '{shortcode}'")))?
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
        // Non-ASCII input whose suffix cut falls inside a character must not panic.
        assert!(shortcode_to_media_id(&format!("{}a", "é".repeat(15))).is_err());
        assert!(shortcode_to_media_id(&"é".repeat(15)).is_err());
    }

    #[test]
    fn sections_parse_case_insensitively() {
        assert_eq!(
            "Stories".parse::<ProfileSection>().unwrap(),
            ProfileSection::Stories
        );
        assert!("igtv".parse::<ProfileSection>().is_err());
    }

    const CODE: &str = "C0ffeeXYZ12";

    fn post(index: Option<u32>) -> Target {
        Target::Post {
            shortcode: CODE.to_string(),
            index,
        }
    }

    fn user(name: &str, section: Option<ProfileSection>) -> Target {
        Target::Profile {
            username: name.to_string(),
            section,
        }
    }

    fn stories(name: &str, story_id: Option<&str>) -> Target {
        Target::Stories {
            username: name.to_string(),
            story_id: story_id.map(str::to_string),
        }
    }

    fn highlight(id: &str) -> Target {
        Target::Highlight { id: id.to_string() }
    }

    #[track_caller]
    fn parses(input: &str, expected: Target) {
        match parse_link(input) {
            Ok(target) => assert_eq!(target, expected, "input: {input:?}"),
            Err(err) => panic!("{input:?} failed: {err}"),
        }
    }

    /// Asserts `input` is rejected as unsupported with a message containing `needle`.
    #[track_caller]
    fn rejects(input: &str, needle: &str) {
        match parse_link(input) {
            Err(Error::Unsupported(message)) => assert!(
                message.contains(needle),
                "{input:?}: message {message:?} lacks {needle:?}"
            ),
            other => panic!("{input:?} should be unsupported, got {other:?}"),
        }
    }

    #[test]
    fn post_links_in_every_form() {
        for input in [
            "https://www.instagram.com/p/C0ffeeXYZ12/",
            "https://www.instagram.com/p/C0ffeeXYZ12",
            "https://www.instagram.com/reel/C0ffeeXYZ12/",
            "https://www.instagram.com/reels/C0ffeeXYZ12/",
            "https://www.instagram.com/tv/C0ffeeXYZ12/",
            "https://www.instagram.com/some.user/p/C0ffeeXYZ12/",
            "https://www.instagram.com/some.user/reel/C0ffeeXYZ12/",
            "https://www.instagram.com/some.user/tv/C0ffeeXYZ12/",
            "https://instagram.com/p/C0ffeeXYZ12/",
            "http://www.instagram.com/p/C0ffeeXYZ12/",
            "https://m.instagram.com/p/C0ffeeXYZ12/",
            "https://instagr.am/p/C0ffeeXYZ12/",
            "https://www.instagr.am/p/C0ffeeXYZ12/",
            "instagram.com/p/C0ffeeXYZ12",
            "www.instagram.com/reel/C0ffeeXYZ12/",
            "m.instagram.com/p/C0ffeeXYZ12/",
            "instagr.am/p/C0ffeeXYZ12",
            "//www.instagram.com/p/C0ffeeXYZ12/",
            "HTTPS://WWW.INSTAGRAM.COM/P/C0ffeeXYZ12/",
            "https://www.instagram.com/p/C0ffeeXYZ12/embed/captioned/",
            "https://www.instagram.com/p/C0ffeeXYZ12/#comments",
            "https://www.instagram.com:443/p/C0ffeeXYZ12/",
        ] {
            parses(input, post(None));
        }
    }

    #[test]
    fn tracking_parameters_are_ignored() {
        for input in [
            "https://www.instagram.com/p/C0ffeeXYZ12/?igsh=MWQ1ZGUxMzBkMA==",
            "https://www.instagram.com/reel/C0ffeeXYZ12/?utm_source=ig_web_copy_link&utm_medium=copy",
            "https://www.instagram.com/p/C0ffeeXYZ12/?hl=en&igshid=abc",
        ] {
            parses(input, post(None));
        }
        parses(
            "https://www.instagram.com/some.user/?igsh=abc&utm_source=qr",
            user("some.user", None),
        );
    }

    #[test]
    fn img_index_selects_one_carousel_item() {
        parses(
            "https://www.instagram.com/p/C0ffeeXYZ12/?img_index=3",
            post(Some(3)),
        );
        parses(
            "instagram.com/p/C0ffeeXYZ12/?utm_source=ig_web&img_index=02&igsh=x",
            post(Some(2)),
        );
        // Invalid or zero indexes are ignored rather than rejected.
        parses("instagram.com/p/C0ffeeXYZ12/?img_index=0", post(None));
        parses("instagram.com/p/C0ffeeXYZ12/?img_index=two", post(None));
        parses("instagram.com/p/C0ffeeXYZ12/?img_index=-1", post(None));
        // Only posts carry an index.
        parses(
            "instagram.com/some.user/?img_index=2",
            user("some.user", None),
        );
    }

    #[test]
    fn shortcodes_are_validated() {
        let private = format!("{CODE}{}", "x".repeat(28));
        parses(
            &format!("https://www.instagram.com/p/{private}/"),
            Target::Post {
                shortcode: private.clone(),
                index: None,
            },
        );
        parses(
            "instagram.com/p/A-b_C/",
            Target::Post {
                shortcode: "A-b_C".into(),
                index: None,
            },
        );
        rejects("https://www.instagram.com/p/", "no post shortcode");
        rejects("https://www.instagram.com/tv/", "no post shortcode");
        rejects(
            "https://www.instagram.com/some.user/p/",
            "no post shortcode",
        );
        rejects(
            "https://www.instagram.com/p/abc$def/",
            "invalid post shortcode",
        );
        rejects(
            "https://www.instagram.com/p/C0ff%C3%A9e/",
            "invalid post shortcode",
        );
        rejects(
            &format!("https://www.instagram.com/p/{}/", "z".repeat(20)),
            "invalid post shortcode",
        );
    }

    #[test]
    fn reels_feed_and_audio_pages_are_unsupported() {
        rejects("https://www.instagram.com/reels/", "reels feed");
        rejects("instagram.com/reels", "reels feed");
        rejects(
            "https://www.instagram.com/reels/audio/123456789012345/",
            "audio",
        );
        rejects("https://www.instagram.com/reel/audio/1234/", "audio");
    }

    #[test]
    fn profile_links_in_every_form() {
        for input in [
            "https://www.instagram.com/some.user/",
            "https://www.instagram.com/some.user",
            "https://www.instagram.com/Some.User/?hl=de",
            "instagram.com/some.user",
            "m.instagram.com/some.user/",
            "instagr.am/some.user",
            "@some.user",
            "@Some.User",
            "some.user",
            "SOME.USER",
            "  some.user  ",
        ] {
            parses(input, user("some.user", None));
        }
        parses("user_123", user("user_123", None));
        parses("1234567", user("1234567", None));
        parses(&"a".repeat(30), user(&"a".repeat(30), None));
    }

    #[test]
    fn profile_sections() {
        parses(
            "https://www.instagram.com/some.user/reels/",
            user("some.user", Some(ProfileSection::Reels)),
        );
        parses(
            "instagram.com/Some.User/reels",
            user("some.user", Some(ProfileSection::Reels)),
        );
        parses(
            "https://www.instagram.com/some.user/tagged/",
            user("some.user", Some(ProfileSection::Tagged)),
        );
        parses(
            "https://www.instagram.com/some.user/TAGGED/?igsh=x",
            user("some.user", Some(ProfileSection::Tagged)),
        );
    }

    #[test]
    fn unsupported_profile_pages() {
        rejects(
            "https://www.instagram.com/some.user/saved/",
            "saved collections are not supported",
        );
        rejects(
            "https://www.instagram.com/some.user/saved/all-posts/17890000000000000/",
            "saved collections are not supported",
        );
        rejects(
            "https://www.instagram.com/some.user/following/",
            "not supported",
        );
        rejects(
            "https://www.instagram.com/some.user/reels/extra/",
            "not supported",
        );
        rejects(
            "https://www.instagram.com/some-user/",
            "not a valid Instagram username",
        );
        rejects(
            &format!("https://www.instagram.com/{}/", "a".repeat(31)),
            "not a valid Instagram username",
        );
    }

    #[test]
    fn invalid_usernames() {
        rejects("@", "invalid username");
        rejects("@some-user", "invalid username");
        rejects("@some.user/", "invalid username");
        rejects(&format!("@{}", "a".repeat(31)), "invalid username");
        rejects(&"a".repeat(31), "at most 30 characters");
        rejects("some user", "neither an Instagram link nor a username");
    }

    #[test]
    fn story_links() {
        parses(
            "https://www.instagram.com/stories/some.user/",
            stories("some.user", None),
        );
        parses(
            "instagram.com/stories/Some.User",
            stories("some.user", None),
        );
        parses(
            "https://www.instagram.com/stories/some.user/3359123456789012345/?utm_source=ig_story_item_share",
            stories("some.user", Some("3359123456789012345")),
        );
        rejects("https://www.instagram.com/stories/", "no username");
        rejects(
            "https://www.instagram.com/stories/some.user/abc/",
            "invalid story id",
        );
        rejects(
            "https://www.instagram.com/stories/some-user/",
            "invalid username",
        );
    }

    #[test]
    fn highlight_links() {
        for input in [
            "https://www.instagram.com/stories/highlights/17912345678901234/",
            "instagram.com/stories/highlights/17912345678901234",
            "https://www.instagram.com/stories/highlights/highlight:17912345678901234/",
            "https://www.instagram.com/stories/highlights/highlight%3A17912345678901234/",
            "highlight:17912345678901234",
            "HIGHLIGHT:17912345678901234",
        ] {
            parses(input, highlight("17912345678901234"));
        }
        rejects("https://www.instagram.com/stories/highlights/", "no id");
        rejects(
            "https://www.instagram.com/stories/highlights/abc/",
            "invalid highlight id",
        );
        rejects("highlight:", "invalid highlight id");
        rejects("highlight:12a", "invalid highlight id");
    }

    #[test]
    fn markdown_and_quotes_are_unwrapped() {
        for input in [
            "[my post](https://www.instagram.com/p/C0ffeeXYZ12/)",
            "[see [this]](https://www.instagram.com/p/C0ffeeXYZ12/)",
            "[t](https://www.instagram.com/p/C0ffeeXYZ12/ \"a title\")",
            "[t](<https://www.instagram.com/p/C0ffeeXYZ12/>)",
            "<https://www.instagram.com/p/C0ffeeXYZ12/>",
            "\"https://www.instagram.com/p/C0ffeeXYZ12/\"",
            "'instagram.com/p/C0ffeeXYZ12'",
            "`https://www.instagram.com/p/C0ffeeXYZ12/`",
            "“https://www.instagram.com/p/C0ffeeXYZ12/”",
            " \t\"<https://www.instagram.com/p/C0ffeeXYZ12/>\" \n",
            "\u{feff}https://www.instagram.com/p/C0ffeeXYZ12/",
        ] {
            parses(input, post(None));
        }
        parses("'@Some.User'", user("some.user", None));
        parses("<some.user>", user("some.user", None));
    }

    #[test]
    fn foreign_and_lookalike_hosts_are_rejected() {
        rejects(
            "https://example.com/p/C0ffeeXYZ12/",
            "not an Instagram link: example.com",
        );
        rejects(
            "https://notinstagram.com/p/C0ffeeXYZ12/",
            "not an Instagram link: notinstagram.com",
        );
        rejects(
            "notinstagram.com",
            "not an Instagram link: notinstagram.com",
        );
        rejects(
            "https://instagram.com.evil.example/p/C0ffeeXYZ12/",
            "not an Instagram link: instagram.com.evil.example",
        );
        rejects(
            "instagram.company.com/p/C0ffeeXYZ12/",
            "not an Instagram link: instagram.company.com",
        );
        rejects(
            "https://instagram.com@evil.example/p/C0ffeeXYZ12/",
            "not an Instagram link: evil.example",
        );
        rejects(
            "https://help.instagram.com/123456",
            "not an Instagram link: help.instagram.com",
        );
        rejects(
            "https://scontent.cdninstagram.com/v/t51/1_n.jpg",
            "not an Instagram link: scontent.cdninstagram.com",
        );
        rejects(
            "https://instagram.co/p/C0ffeeXYZ12/",
            "not an Instagram link",
        );
        rejects(
            "ftp://instagram.com/p/C0ffeeXYZ12/",
            "unsupported URL scheme",
        );
        rejects(
            "javascript:alert(1)",
            "neither an Instagram link nor a username",
        );
    }

    #[test]
    fn empty_input_and_home_page_are_rejected() {
        rejects("", "empty link");
        rejects("   ", "empty link");
        rejects("\"\"", "empty link");
        rejects("[]()", "empty link");
        rejects("https://www.instagram.com/", "home page");
        rejects("instagram.com", "home page");
        rejects("https://www.instagram.com/?hl=en", "home page");
    }

    #[test]
    fn reserved_paths_are_not_usernames() {
        for (path, needle) in [
            ("explore/", "explore"),
            ("explore/tags/cats/", "explore"),
            ("explore/locations/123/somewhere/", "explore"),
            ("locations/123/", "location"),
            ("topics/foo/", "not supported"),
            ("directory/profiles/", "not supported"),
            ("accounts/login/?next=/p/C0ffeeXYZ12/", "login"),
            ("accounts/edit/", "account"),
            ("challenge/", "login"),
            ("session/login_activity/", "login"),
            ("oauth/authorize/", "login"),
            ("emails/settings/", "login"),
            ("direct/inbox/", "direct messages"),
            ("direct/t/1234/", "direct messages"),
            ("share/reel/BAabcdef/", "share links"),
            ("s/aGlnaGxpZ2h0OjE3OTEyMzQ1Njc4OTAxMjM0", "share links"),
            ("about/us/", "instagram.com/about/"),
            ("developer/", "not a post, profile"),
            ("legal/terms/", "not a post, profile"),
            ("web/", "not a post, profile"),
            ("api/v1/users/web_profile_info/", "instagram.com/api/"),
            ("graphql/query/", "not a post, profile"),
            ("privacy/", "not a post, profile"),
            ("terms/accept/", "not a post, profile"),
            ("lite/", "not a post, profile"),
            ("nametag/", "not a post, profile"),
            ("invites/contact/", "not a post, profile"),
            ("Explore/", "explore"),
        ] {
            rejects(&format!("https://www.instagram.com/{path}"), needle);
        }
    }

    #[test]
    fn errors_quote_long_input_briefly() {
        let long = format!("@{}", "x".repeat(500));
        match parse_link(&long) {
            Err(Error::Unsupported(message)) => assert!(message.len() < 120, "{message}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn canonical_url_for_every_variant() {
        let cases = [
            (post(None), "https://www.instagram.com/p/C0ffeeXYZ12/"),
            (
                post(Some(3)),
                "https://www.instagram.com/p/C0ffeeXYZ12/?img_index=3",
            ),
            (
                user("some.user", None),
                "https://www.instagram.com/some.user/",
            ),
            (
                user("some.user", Some(ProfileSection::Posts)),
                "https://www.instagram.com/some.user/",
            ),
            (
                user("some.user", Some(ProfileSection::Reels)),
                "https://www.instagram.com/some.user/reels/",
            ),
            (
                user("some.user", Some(ProfileSection::Tagged)),
                "https://www.instagram.com/some.user/tagged/",
            ),
            (
                user("some.user", Some(ProfileSection::Stories)),
                "https://www.instagram.com/stories/some.user/",
            ),
            (
                user("some.user", Some(ProfileSection::Highlights)),
                "https://www.instagram.com/some.user/",
            ),
            (
                stories("some.user", None),
                "https://www.instagram.com/stories/some.user/",
            ),
            (
                stories("some.user", Some("3359123456789012345")),
                "https://www.instagram.com/stories/some.user/3359123456789012345/",
            ),
            (
                highlight("17912345678901234"),
                "https://www.instagram.com/stories/highlights/17912345678901234/",
            ),
        ];
        for (target, url) in cases {
            assert_eq!(target.canonical_url(), url, "{target:?}");
        }
    }

    #[test]
    fn canonical_urls_parse_back() {
        for target in [
            post(None),
            post(Some(4)),
            user("some.user", None),
            user("some.user", Some(ProfileSection::Reels)),
            user("some.user", Some(ProfileSection::Tagged)),
            stories("some.user", None),
            stories("some.user", Some("3359123456789012345")),
            highlight("17912345678901234"),
        ] {
            parses(&target.canonical_url(), target);
        }
    }

    #[test]
    fn describe_every_variant() {
        let cases = [
            (post(None), "post C0ffeeXYZ12"),
            (post(Some(2)), "post C0ffeeXYZ12 #2"),
            (user("some.user", None), "some.user"),
            (
                user("some.user", Some(ProfileSection::Posts)),
                "some.user · posts",
            ),
            (
                user("some.user", Some(ProfileSection::Reels)),
                "some.user · reels",
            ),
            (
                user("some.user", Some(ProfileSection::Tagged)),
                "some.user · tagged",
            ),
            (
                user("some.user", Some(ProfileSection::Stories)),
                "some.user · stories",
            ),
            (
                user("some.user", Some(ProfileSection::Highlights)),
                "some.user · highlights",
            ),
            (stories("some.user", None), "some.user · stories"),
            (
                stories("some.user", Some("3359123456789012345")),
                "some.user · story 3359123456789012345",
            ),
            (
                highlight("17912345678901234"),
                "highlight 17912345678901234",
            ),
        ];
        for (target, name) in cases {
            assert_eq!(target.describe(), name, "{target:?}");
        }
    }

    #[test]
    fn read_links_handles_comments_markers_crlf_and_bom() {
        let text = "\u{feff}# my links\r\n\
            \r\n\
            https://www.instagram.com/p/C0ffeeXYZ12/\r\n\
            - [a reel](https://instagram.com/reel/C0ffeeXYZ13/?igsh=abc)\r\n\
            * @some.user\r\n\
            + other_user\r\n\
            1. \"third_user\"\r\n\
            2) <fourth_user>\r\n\
            \t  # indented comment\r\n\
            fifth_user sixth_user   # trailing comment\r\n\
            > - [ ] seventh_user\r\n\
            eighth_user, ninth_user\r\n\
            see instagram.com/stories/some.user/ and https://instagr.am/p/C0ffeeXYZ14/.\r\n\
            [profile](tenth_user)\r\n\
            10. 'eleventh_user'\n";
        assert_eq!(
            read_links(text),
            [
                "https://www.instagram.com/p/C0ffeeXYZ12/",
                "https://instagram.com/reel/C0ffeeXYZ13/?igsh=abc",
                "@some.user",
                "other_user",
                "third_user",
                "fourth_user",
                "fifth_user",
                "sixth_user",
                "seventh_user",
                "eighth_user",
                "ninth_user",
                "https://instagram.com/stories/some.user/",
                "https://instagr.am/p/C0ffeeXYZ14/",
                "tenth_user",
                "eleventh_user",
            ]
        );
    }

    #[test]
    fn read_links_bom_on_entry_line_and_empty_input() {
        assert_eq!(read_links("\u{feff}some_user\n"), ["some_user"]);
        assert_eq!(read_links("\u{feff}@some_user"), ["@some_user"]);
        assert!(read_links("").is_empty());
        assert!(read_links("\r\n  \n# only comments\n#another\n").is_empty());
        // `-` with no space is not a list marker.
        assert_eq!(read_links("-x"), ["-x"]);
    }

    #[test]
    fn read_links_entries_all_parse() {
        let text = "- https://www.instagram.com/p/C0ffeeXYZ12/?img_index=2\n\
            * @Some.User\n\
            1. <https://www.instagram.com/stories/highlights/17912345678901234/>\n\
            \"instagram.com/some.user/tagged\"\n";
        let targets: Vec<Target> = read_links(text)
            .iter()
            .map(|entry| parse_link(entry).unwrap())
            .collect();
        assert_eq!(
            targets,
            [
                post(Some(2)),
                user("some.user", None),
                highlight("17912345678901234"),
                user("some.user", Some(ProfileSection::Tagged)),
            ]
        );
    }

    #[test]
    fn extract_links_from_markdown() {
        let text = "Check [this post](https://www.instagram.com/p/C0ffeeXYZ12/) and \
            <https://instagram.com/some.user/>.\n\
            - [reel](https://www.instagram.com/reel/C0ffeeXYZ13/?igsh=abc)\n\
            **https://www.instagram.com/some.user/tagged/**\n\
            | https://www.instagram.com/stories/some.user/ | `instagram.com/p/C0ffeeXYZ14` |";
        assert_eq!(
            extract_links(text),
            [
                "https://www.instagram.com/p/C0ffeeXYZ12/",
                "https://instagram.com/some.user/",
                "https://www.instagram.com/reel/C0ffeeXYZ13/?igsh=abc",
                "https://www.instagram.com/some.user/tagged/",
                "https://www.instagram.com/stories/some.user/",
                "https://instagram.com/p/C0ffeeXYZ14",
            ]
        );
    }

    #[test]
    fn extract_links_from_html() {
        let html = r#"<p><a href="https://www.instagram.com/p/C0ffeeXYZ12/?utm_source=ig_web&amp;img_index=2">post</a>
            <a href='http://m.instagram.com/reel/C0ffeeXYZ13/'>reel</a>
            <img src="https://scontent.cdninstagram.com/v/x.jpg">
            <a href=https://instagr.am/p/C0ffeeXYZ14/>bare attr</a></p>"#;
        let links = extract_links(html);
        assert_eq!(
            links,
            [
                "https://www.instagram.com/p/C0ffeeXYZ12/?utm_source=ig_web&img_index=2",
                "https://m.instagram.com/reel/C0ffeeXYZ13/",
                "https://instagr.am/p/C0ffeeXYZ14/",
            ]
        );
        assert_eq!(parse_link(&links[0]).unwrap(), post(Some(2)));
    }

    #[test]
    fn extract_links_from_prose_with_punctuation() {
        let text = "Wow, look: instagram.com/p/C0ffeeXYZ12! Also www.instagram.com/some.user/reels/, \
            (instagr.am/p/C0ffeeXYZ14). Same again: HTTPS://instagram.com/p/C0ffeeXYZ12? \
            Really... https://www.instagram.com/stories/some.user/3359123456789012345/;\n\
            [12:01] <someone> https://www.instagram.com/p/C0ffeeXYZ13/?igsh=MTc4 :) \
            Schau mal → instagram.com/stories/highlights/17912345678901234/ 😀";
        assert_eq!(
            extract_links(text),
            [
                "https://instagram.com/p/C0ffeeXYZ12",
                "https://www.instagram.com/some.user/reels/",
                "https://instagr.am/p/C0ffeeXYZ14",
                "https://www.instagram.com/stories/some.user/3359123456789012345/",
                "https://www.instagram.com/p/C0ffeeXYZ13/?igsh=MTc4",
                "https://instagram.com/stories/highlights/17912345678901234/",
            ]
        );
    }

    #[test]
    fn extract_links_normalises_scheme_and_host_case() {
        assert_eq!(
            extract_links("HTTP://WWW.Instagram.COM/p/C0ffeeXYZ12/"),
            ["https://www.instagram.com/p/C0ffeeXYZ12/"]
        );
        assert_eq!(
            extract_links("instagram.com/Some.User/ and INSTAGRAM.COM/Some.User/"),
            ["https://instagram.com/Some.User/"]
        );
    }

    #[test]
    fn extract_links_ignores_lookalikes() {
        let text = "notinstagram.com/p/C0ffeeXYZ12 instagram.company.com/p/C0ffeeXYZ12 \
            https://instagram.com.evil.example/p/C0ffeeXYZ12 foo.instagram.com/p/C0ffeeXYZ12 \
            https://www.notinstagram.com/p/C0ffeeXYZ12 someone@instagram.com/x \
            example.com/instagram.com/p/C0ffeeXYZ12 éinstagram.com/p/C0ffeeXYZ12 \
            instagr.amazing.example/p/C0ffeeXYZ12 instagram.com instagram.com: \
            https://www.instagram.com/ www.instagram.com/?hl=en wwww.instagram.com/p/C0ffeeXYZ12 \
            natgeo @natgeo";
        assert_eq!(extract_links(text), Vec::<String>::new());
        assert!(extract_links("").is_empty());
    }

    #[test]
    fn extract_links_does_not_split_nested_urls() {
        assert_eq!(
            extract_links(
                "https://www.instagram.com/accounts/login/?next=instagram.com/p/C0ffeeXYZ12/"
            ),
            ["https://www.instagram.com/accounts/login/?next=instagram.com/p/C0ffeeXYZ12/"]
        );
    }
}
