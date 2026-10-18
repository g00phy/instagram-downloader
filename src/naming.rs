//! Output filename templates (`{owner}/{category}/{date}_{id}_{index}.{ext}`).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::error::{Error, Result};
use crate::model::MediaFile;

/// Template used when none is configured.
pub const DEFAULT_TEMPLATE: &str = "{owner}/{category}/{date}_{id}_{index}.{ext}";

/// Placeholders a template may use (see README "Filename templates").
pub const PLACEHOLDERS: &[&str] = &[
    "owner",
    "owner_id",
    "category",
    "id",
    "shortcode",
    "post_id",
    "media_id",
    "index",
    "count",
    "date",
    "time",
    "datetime",
    "timestamp",
    "kind",
    "ext",
    "width",
    "height",
    "highlight",
    "caption",
];

/// Checks a template: balanced braces, known placeholders only, non-empty, not
/// absolute, no `..` components. Errors are [`crate::error::Error::Config`].
pub fn validate_template(template: &str) -> Result<()> {
    if template.trim().is_empty() {
        return Err(Error::Config("template is empty".into()));
    }
    if template.starts_with(SEPARATORS) {
        return Err(Error::Config(format!(
            "template '{template}' must be a relative path (it starts with a path separator)"
        )));
    }
    let mut chars = template.chars();
    if let (Some(drive), Some(':')) = (chars.next(), chars.next())
        && drive.is_ascii_alphabetic()
    {
        return Err(Error::Config(format!(
            "template '{template}' must be a relative path (it starts with a drive letter)"
        )));
    }
    parse_pieces(template)?;
    if template
        .split(SEPARATORS)
        .any(|component| matches!(component.trim(), "." | ".."))
    {
        return Err(Error::Config(format!(
            "template '{template}' must not contain '.' or '..' path components"
        )));
    }
    if template.ends_with(SEPARATORS) {
        return Err(Error::Config(format!(
            "template '{template}' must end with a file name, not a path separator"
        )));
    }
    Ok(())
}

/// Renders `template` for `media` into a relative path. `/` (and `\`) in the
/// template separate directories; each rendered component is sanitised with
/// [`sanitize_component`], so values can never add directories or escape the
/// output root. Dates are UTC.
///
/// Empty components in the template (`a//b`) are skipped; a component that
/// renders to nothing (e.g. `{highlight}` outside highlights) becomes `_`.
pub fn render(template: &str, media: &MediaFile) -> Result<PathBuf> {
    validate_template(template)?;
    let values = Values::new(media);
    let mut path = PathBuf::new();
    for component in template.split(SEPARATORS).filter(|c| !c.is_empty()) {
        let mut rendered = String::new();
        for piece in parse_pieces(component)? {
            match piece {
                Piece::Literal(text) => rendered.push_str(text),
                Piece::Placeholder(name) => rendered.push_str(&values.get(name)),
            }
        }
        path.push(sanitize_component(&rendered));
    }
    Ok(path)
}

/// Makes one path component safe on Windows, macOS and Linux: replaces
/// `<>:"/\|?*` and control characters with `_`, trims trailing dots/spaces,
/// prefixes reserved device names (`CON`, `NUL`, `COM1`, …) with `_`, limits the
/// length to 150 bytes on a char boundary (keeping the extension), and maps an
/// empty result to `_`.
///
/// Leading whitespace is trimmed too, and Unicode bidirectional controls
/// (which can disguise an extension) are replaced like control characters.
pub fn sanitize_component(component: &str) -> String {
    let replaced: String = component
        .chars()
        .map(|c| if is_forbidden_char(c) { '_' } else { c })
        .collect();
    let trimmed = trim_name(&replaced);
    let mut name = if is_reserved_device_name(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    if name.len() > MAX_COMPONENT_BYTES {
        name = truncate_keeping_extension(&name, MAX_COMPONENT_BYTES);
    }
    if name.is_empty() {
        "_".to_string()
    } else {
        name
    }
}

/// `path` if nothing exists there, else the first free `name (1).ext`, `name (2).ext`, ….
///
/// "Exists" includes broken symlinks; paths whose metadata cannot be read are
/// treated as free, so the search always terminates.
pub fn unique_path(path: &Path) -> PathBuf {
    if !path_taken(path) {
        return path.to_path_buf();
    }
    let stem = path.file_stem().unwrap_or_default();
    let ext = path.extension();
    let mut n: u64 = 1;
    loop {
        let mut name = OsString::from(stem);
        name.push(format!(" ({n})"));
        if let Some(ext) = ext {
            name.push(".");
            name.push(ext);
        }
        let candidate = path.with_file_name(name);
        if !path_taken(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// Directory separators accepted in templates.
const SEPARATORS: [char; 2] = ['/', '\\'];

/// Longest path component [`sanitize_component`] produces, in bytes.
const MAX_COMPONENT_BYTES: usize = 150;

/// Longest extension (characters after the last dot) kept when truncating.
const MAX_KEPT_EXTENSION_CHARS: usize = 10;

/// Characters of the caption `{caption}` keeps.
const CAPTION_CHARS: usize = 50;

/// A parsed fragment of a template.
enum Piece<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
}

/// Splits `template` into literals and placeholders, checking that braces are
/// balanced and every placeholder is in [`PLACEHOLDERS`].
fn parse_pieces(template: &str) -> Result<Vec<Piece<'_>>> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if open > 0 {
            pieces.push(Piece::Literal(&rest[..open]));
        }
        if rest[open..].starts_with('}') {
            return Err(Error::Config(format!(
                "unmatched '}}' in template '{template}'"
            )));
        }
        let after = &rest[open + 1..];
        let close = after
            .find(['{', '}'])
            .filter(|&i| after[i..].starts_with('}'))
            .ok_or_else(|| Error::Config(format!("unclosed '{{' in template '{template}'")))?;
        let name = &after[..close];
        if !PLACEHOLDERS.contains(&name) {
            return Err(Error::Config(format!(
                "unknown placeholder {{{name}}} in template; available: {}",
                PLACEHOLDERS.join(", ")
            )));
        }
        pieces.push(Piece::Placeholder(name));
        rest = &after[close + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Literal(rest));
    }
    Ok(pieces)
}

/// Placeholder values for one file.
struct Values<'a> {
    media: &'a MediaFile,
    taken_at: DateTime<Utc>,
}

impl<'a> Values<'a> {
    fn new(media: &'a MediaFile) -> Self {
        // Out-of-range timestamps (corrupt data) fall back to the Unix epoch.
        let taken_at = DateTime::from_timestamp(media.taken_at, 0).unwrap_or_default();
        Values { media, taken_at }
    }

    fn get(&self, name: &str) -> String {
        let media = self.media;
        let optional = |value: Option<&String>| value.cloned().unwrap_or_default();
        let number = |value: Option<u32>| value.map(|n| n.to_string()).unwrap_or_default();
        match name {
            "owner" => media.owner.clone(),
            "owner_id" => optional(media.owner_id.as_ref()),
            "category" => media.category.dir_name().to_string(),
            "id" => match &media.shortcode {
                Some(code) if !code.is_empty() => code.clone(),
                _ => media.post_id.clone(),
            },
            "shortcode" => optional(media.shortcode.as_ref()),
            "post_id" => media.post_id.clone(),
            "media_id" => media.key.clone(),
            "index" => format!("{:02}", media.index),
            "count" => media.count.to_string(),
            "date" => self.taken_at.format("%Y-%m-%d").to_string(),
            "time" => self.taken_at.format("%H-%M-%S").to_string(),
            "datetime" => self.taken_at.format("%Y%m%d_%H%M%S").to_string(),
            "timestamp" => media.taken_at.to_string(),
            "kind" => media.kind.as_str().to_string(),
            "ext" => media.ext.clone(),
            "width" => number(media.width),
            "height" => number(media.height),
            "highlight" => optional(media.highlight.as_ref()),
            "caption" => media
                .caption
                .as_deref()
                .map(caption_excerpt)
                .unwrap_or_default(),
            // `validate_template` rejects every other name before rendering.
            _ => String::new(),
        }
    }
}

/// First [`CAPTION_CHARS`] characters of `caption`, whitespace runs collapsed.
fn caption_excerpt(caption: &str) -> String {
    let collapsed = caption.split_whitespace().collect::<Vec<_>>().join(" ");
    let excerpt: String = collapsed.chars().take(CAPTION_CHARS).collect();
    excerpt.trim().to_string()
}

fn is_forbidden_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        || c.is_control()
        || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Trims leading whitespace and trailing whitespace and dots.
fn trim_name(name: &str) -> &str {
    name.trim_start()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` (any case, with or
/// without an extension), which Windows maps to devices.
fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            upper.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && matches!(upper.as_bytes().get(3), Some(b'1'..=b'9'))
        }
    }
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn floor_char_boundary(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Cuts `name` to at most `max` bytes, keeping a short extension intact.
fn truncate_keeping_extension(name: &str, max: usize) -> String {
    let extension = name
        .rfind('.')
        .filter(|&dot| dot > 0)
        .map(|dot| &name[dot..])
        .filter(|ext| {
            let chars = ext.chars().count() - 1;
            (1..=MAX_KEPT_EXTENSION_CHARS).contains(&chars) && ext.len() < max
        });
    match extension {
        Some(ext) => {
            let stem = &name[..name.len() - ext.len()];
            let stem = trim_name(floor_char_boundary(stem, max - ext.len()));
            let stem = if stem.is_empty() { "_" } else { stem };
            format!("{stem}{ext}")
        }
        None => trim_name(floor_char_boundary(name, max)).to_string(),
    }
}

/// Whether anything (file, directory, even a broken symlink) is at `path`.
fn path_taken(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use std::path::Component;

    use super::*;
    use crate::error::Error;
    use crate::model::{Category, MediaKind};

    /// 2024-05-01 13:37:00 UTC.
    const TAKEN_AT: i64 = 1_714_570_620;

    fn media() -> MediaFile {
        MediaFile {
            key: "3301234567890123456".into(),
            post_id: "3301234567890000000".into(),
            shortcode: Some("C0ffeeXYZ12".into()),
            index: 2,
            count: 5,
            kind: MediaKind::Image,
            category: Category::Post,
            url: "https://cdn.example.invalid/v/fake.jpg".into(),
            width: Some(1080),
            height: Some(1350),
            owner: "example_user".into(),
            owner_id: Some("1234567".into()),
            taken_at: TAKEN_AT,
            caption: Some("Hello\nworld\t\tagain  ".into()),
            highlight: None,
            ext: "jpg".into(),
        }
    }

    fn path(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    /// Renders a single-component template to a string.
    #[track_caller]
    fn one(template: &str, media: &MediaFile) -> String {
        let rendered = render(template, media).unwrap();
        assert_eq!(rendered.components().count(), 1, "{rendered:?}");
        rendered.to_string_lossy().into_owned()
    }

    #[track_caller]
    fn config_error(template: &str, needle: &str) {
        match validate_template(template) {
            Err(Error::Config(message)) => assert!(
                message.contains(needle),
                "{template:?}: {message:?} lacks {needle:?}"
            ),
            other => panic!("{template:?} should be invalid, got {other:?}"),
        }
        assert!(render(template, &media()).is_err());
    }

    /// Every component is a plain name: no root, prefix, `.` or `..`.
    #[track_caller]
    fn assert_contained(rendered: &Path) {
        assert!(rendered.is_relative(), "{rendered:?}");
        for component in rendered.components() {
            match component {
                Component::Normal(name) => {
                    let name = name.to_string_lossy();
                    assert!(!name.contains(['/', '\\']), "{rendered:?}");
                    assert!(name != "." && name != "..", "{rendered:?}");
                    assert!(name.len() <= MAX_COMPONENT_BYTES, "{rendered:?}");
                }
                other => panic!("{rendered:?} has component {other:?}"),
            }
        }
    }

    #[test]
    fn valid_templates() {
        validate_template(DEFAULT_TEMPLATE).unwrap();
        let all = PLACEHOLDERS
            .iter()
            .map(|name| format!("{{{name}}}"))
            .collect::<Vec<_>>()
            .join("_");
        validate_template(&all).unwrap();
        validate_template("{owner}\\{category}\\{id}.{ext}").unwrap();
        validate_template("fixed name.jpg").unwrap();
        validate_template("{owner}/..{id}.{ext}").unwrap();
        validate_template("a//{id}.{ext}").unwrap();
    }

    #[test]
    fn template_errors_are_precise() {
        config_error("", "template is empty");
        config_error("   ", "template is empty");
        config_error("{owner", "unclosed '{'");
        config_error("{owner}/{id.{ext}", "unclosed '{'");
        config_error("{{owner}}", "unclosed '{'");
        config_error("owner}", "unmatched '}'");
        config_error("{id}}.{ext}", "unmatched '}'");
        config_error("{}", "unknown placeholder {} in template");
        config_error("{Owner}", "unknown placeholder {Owner}");
        config_error("{own/er}.{ext}", "unknown placeholder {own/er}");
        config_error("/{owner}/{id}.{ext}", "relative path");
        config_error("\\{owner}\\{id}.{ext}", "relative path");
        config_error("\\\\server\\share\\{id}", "relative path");
        config_error("C:\\insta\\{id}.{ext}", "drive letter");
        config_error("c:{id}.{ext}", "drive letter");
        config_error("../{id}.{ext}", "'..'");
        config_error("{owner}/../{id}.{ext}", "'..'");
        config_error("{owner}\\..\\{id}.{ext}", "'..'");
        config_error("./{id}.{ext}", "'.'");
        config_error("{owner}/ .. /{id}", "'..'");
        config_error("{owner}/", "must end with a file name");
        config_error("{owner}\\", "must end with a file name");
    }

    #[test]
    fn unknown_placeholder_lists_available_names() {
        let Err(Error::Config(message)) = validate_template("{owner}/{foo}.{ext}") else {
            panic!("expected a config error");
        };
        assert_eq!(
            message,
            format!(
                "unknown placeholder {{foo}} in template; available: {}",
                PLACEHOLDERS.join(", ")
            )
        );
        assert!(message.contains("owner, owner_id, category"));
    }

    #[test]
    fn renders_default_template() {
        assert_eq!(
            render(DEFAULT_TEMPLATE, &media()).unwrap(),
            path(&["example_user", "posts", "2024-05-01_C0ffeeXYZ12_02.jpg"])
        );
        let story = MediaFile {
            shortcode: None,
            category: Category::Story,
            kind: MediaKind::Video,
            index: 1,
            ext: "mp4".into(),
            ..media()
        };
        assert_eq!(
            render(DEFAULT_TEMPLATE, &story).unwrap(),
            path(&[
                "example_user",
                "stories",
                "2024-05-01_3301234567890000000_01.mp4"
            ])
        );
    }

    #[test]
    fn renders_every_placeholder() {
        let m = media();
        let cases = [
            ("{owner}", "example_user"),
            ("{owner_id}", "1234567"),
            ("{category}", "posts"),
            ("{id}", "C0ffeeXYZ12"),
            ("{shortcode}", "C0ffeeXYZ12"),
            ("{post_id}", "3301234567890000000"),
            ("{media_id}", "3301234567890123456"),
            ("{index}", "02"),
            ("{count}", "5"),
            ("{date}", "2024-05-01"),
            ("{time}", "13-37-00"),
            ("{datetime}", "20240501_133700"),
            ("{timestamp}", "1714570620"),
            ("{kind}", "image"),
            ("{ext}", "jpg"),
            ("{width}", "1080"),
            ("{height}", "1350"),
            ("x{highlight}x", "xx"),
            ("{caption}", "Hello world again"),
        ];
        for (template, expected) in cases {
            assert_eq!(one(template, &m), expected, "{template}");
        }
        // Every placeholder is covered above.
        for name in PLACEHOLDERS {
            assert!(
                cases
                    .iter()
                    .any(|(t, _)| t.contains(&format!("{{{name}}}"))),
                "{name} untested"
            );
        }
    }

    #[test]
    fn optional_values_render_empty() {
        let m = MediaFile {
            owner_id: None,
            shortcode: None,
            width: None,
            height: None,
            caption: None,
            highlight: None,
            ..media()
        };
        assert_eq!(
            one(
                "a{owner_id}b{shortcode}c{width}d{height}e{caption}f{highlight}g",
                &m
            ),
            "abcdefg"
        );
        assert_eq!(one("{id}", &m), "3301234567890000000");
        let empty_code = MediaFile {
            shortcode: Some(String::new()),
            ..media()
        };
        assert_eq!(one("{id}", &empty_code), "3301234567890000000");
        // A component that renders to nothing still yields a valid name.
        assert_eq!(
            render("{owner}/{highlight}/{id}.{ext}", &m).unwrap(),
            path(&["example_user", "_", "3301234567890000000.jpg"])
        );
    }

    #[test]
    fn other_categories_kinds_and_highlights() {
        let m = MediaFile {
            category: Category::Highlight,
            kind: MediaKind::Video,
            highlight: Some("Summer trip".into()),
            index: 123,
            ..media()
        };
        assert_eq!(
            one("{category} {kind} {highlight} {index}", &m),
            "highlights video Summer trip 123"
        );
        for (category, dir) in [
            (Category::Post, "posts"),
            (Category::Reel, "reels"),
            (Category::Tagged, "tagged"),
            (Category::Story, "stories"),
            (Category::Highlight, "highlights"),
        ] {
            let m = MediaFile {
                category,
                ..media()
            };
            assert_eq!(one("{category}", &m), dir);
        }
    }

    #[test]
    fn dates_are_utc_and_survive_bad_timestamps() {
        let m = MediaFile {
            taken_at: 0,
            ..media()
        };
        assert_eq!(one("{datetime}", &m), "19700101_000000");
        let m = MediaFile {
            taken_at: i64::MAX,
            ..media()
        };
        assert_eq!(one("{date}", &m), "1970-01-01");
        assert_eq!(one("{timestamp}", &m), i64::MAX.to_string());
    }

    #[test]
    fn caption_is_collapsed_and_cut_to_fifty_chars() {
        let caption = |text: &str| {
            let m = MediaFile {
                caption: Some(text.into()),
                ..media()
            };
            one("{caption}", &m)
        };
        assert_eq!(
            caption("  multi\r\n\nline\t\tcaption  "),
            "multi line caption"
        );
        assert_eq!(caption(&"x".repeat(80)), "x".repeat(50));
        // Cut on characters, not bytes, then trimmed.
        assert_eq!(caption(&"日本".repeat(40)), "日本".repeat(25));
        // 50 emoji are 200 bytes, so the 150-byte component limit applies too.
        assert_eq!(caption(&"😀".repeat(60)), "😀".repeat(37));
        let m = MediaFile {
            caption: Some("😀".repeat(60)),
            ..media()
        };
        assert_eq!(Values::new(&m).get("caption"), "😀".repeat(50));
        assert_eq!(caption(&format!("{} yz", "x".repeat(49))), "x".repeat(49));
        assert_eq!(caption("   "), "_");
    }

    #[test]
    fn backslashes_and_repeated_separators_in_templates() {
        assert_eq!(
            render("{owner}\\{category}\\{id}.{ext}", &media()).unwrap(),
            path(&["example_user", "posts", "C0ffeeXYZ12.jpg"])
        );
        assert_eq!(
            render("{owner}//{id}.{ext}", &media()).unwrap(),
            path(&["example_user", "C0ffeeXYZ12.jpg"])
        );
    }

    #[test]
    fn values_cannot_escape_or_add_directories() {
        let evil = MediaFile {
            owner: "../../evil".into(),
            caption: Some("a/b\\c/../../etc/passwd".into()),
            ..media()
        };
        let rendered = render("{owner}/{caption}.{ext}", &evil).unwrap();
        assert_eq!(
            rendered,
            path(&[".._.._evil", "a_b_c_.._.._etc_passwd.jpg"])
        );
        assert_contained(&rendered);

        for owner in [
            "..",
            ".",
            "",
            "/",
            "\\",
            "C:",
            "/etc/passwd",
            "..\\..\\x",
            " .. ",
        ] {
            let m = MediaFile {
                owner: owner.into(),
                ..media()
            };
            let rendered = render(DEFAULT_TEMPLATE, &m).unwrap();
            assert_eq!(
                rendered.components().count(),
                3,
                "{owner:?} -> {rendered:?}"
            );
            assert_contained(&rendered);
        }
        assert_eq!(
            render(
                DEFAULT_TEMPLATE,
                &MediaFile {
                    owner: "..".into(),
                    ..media()
                }
            )
            .unwrap(),
            path(&["_", "posts", "2024-05-01_C0ffeeXYZ12_02.jpg"])
        );

        let ext = MediaFile {
            ext: "jpg/../../x".into(),
            ..media()
        };
        let rendered = render("{id}.{ext}", &ext).unwrap();
        assert_eq!(rendered, path(&["C0ffeeXYZ12.jpg_.._.._x"]));
    }

    #[test]
    fn windows_reserved_names_in_values() {
        for (owner, expected) in [
            ("con", "_con"),
            ("NUL", "_NUL"),
            ("Com1", "_Com1"),
            ("lpt9", "_lpt9"),
            ("aux.", "_aux"),
        ] {
            let m = MediaFile {
                owner: owner.into(),
                ..media()
            };
            assert_eq!(one("{owner}", &m), expected);
        }
        let m = MediaFile {
            highlight: Some("prn".into()),
            ..media()
        };
        assert_eq!(one("{highlight}.{ext}", &m), "_prn.jpg");
    }

    #[test]
    fn long_and_unicode_values_are_truncated_on_char_boundaries() {
        let m = MediaFile {
            caption: Some("😀".repeat(60)),
            ..media()
        };
        let name = one("{caption}.{ext}", &m);
        assert!(name.len() <= 150, "{}", name.len());
        assert_eq!(name, format!("{}.jpg", "😀".repeat(36)));

        let m = MediaFile {
            caption: Some("word ".repeat(40)),
            ..media()
        };
        let name = one("{caption}{caption}{caption}{caption}_{index}.{ext}", &m);
        assert!(name.len() <= 150);
        assert!(name.ends_with(".jpg"), "{name}");

        let m = MediaFile {
            owner: "Café ☕ 日本語 ñandú".into(),
            ..media()
        };
        assert_eq!(one("{owner}", &m), "Café ☕ 日本語 ñandú");
    }

    #[test]
    fn sanitize_replaces_forbidden_characters() {
        assert_eq!(
            sanitize_component("a<b>c:d\"e/f\\g|h?i*j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(
            sanitize_component("line\nbreak\ttab\u{0}nul\u{7f}"),
            "line_break_tab_nul_"
        );
        assert_eq!(sanitize_component("esc\u{1b}[31m"), "esc_[31m");
        // Right-to-left override could disguise `exe` as `gpj`.
        assert_eq!(sanitize_component("evil\u{202e}gpj.exe"), "evil_gpj.exe");
        assert_eq!(sanitize_component("Café ☕ 日本語"), "Café ☕ 日本語");
    }

    #[test]
    fn sanitize_trims_and_never_returns_empty() {
        assert_eq!(sanitize_component("  name. . "), "name");
        assert_eq!(sanitize_component("name..."), "name");
        assert_eq!(sanitize_component("\tname"), "_name");
        assert_eq!(sanitize_component(" name"), "name");
        assert_eq!(sanitize_component(".hidden"), ".hidden");
        assert_eq!(sanitize_component("a.b"), "a.b");
        for empty in ["", " ", "   ", ".", "..", "...", " . . "] {
            assert_eq!(sanitize_component(empty), "_", "{empty:?}");
        }
    }

    #[test]
    fn sanitize_prefixes_reserved_device_names() {
        for (name, expected) in [
            ("CON", "_CON"),
            ("con", "_con"),
            ("Prn", "_Prn"),
            ("aux", "_aux"),
            ("nul.txt", "_nul.txt"),
            ("NUL.tar.gz", "_NUL.tar.gz"),
            ("COM1", "_COM1"),
            ("com9.jpg", "_com9.jpg"),
            ("LPT1", "_LPT1"),
            ("lpt5.mp4", "_lpt5.mp4"),
            ("CON ", "_CON"),
            ("con .txt", "_con .txt"),
            ("conin$", "_conin$"),
            ("COM0", "COM0"),
            ("COM10", "COM10"),
            ("LPT", "LPT"),
            ("console", "console"),
            ("icon", "icon"),
            ("nul_", "nul_"),
            ("connect.jpg", "connect.jpg"),
        ] {
            assert_eq!(sanitize_component(name), expected, "{name:?}");
        }
    }

    #[test]
    fn sanitize_limits_length_keeping_extension() {
        assert_eq!(sanitize_component(&"a".repeat(300)), "a".repeat(150));
        assert_eq!(
            sanitize_component(&format!("{}.mp4", "b".repeat(300))),
            format!("{}.mp4", "b".repeat(146))
        );
        // A "long extension" is just part of the name.
        let long_ext = format!("{}.{}", "c".repeat(100), "d".repeat(100));
        assert_eq!(sanitize_component(&long_ext), long_ext[..150]);
        // Multi-byte characters are never split.
        assert_eq!(sanitize_component(&"é".repeat(200)), "é".repeat(75));
        assert_eq!(
            sanitize_component(&format!("{}.jpg", "é".repeat(200))),
            format!("{}.jpg", "é".repeat(73))
        );
        assert_eq!(
            sanitize_component(&format!("{}.jpg", "😀".repeat(100))),
            format!("{}.jpg", "😀".repeat(36))
        );
        // Truncation must not leave a trailing space or dot behind.
        assert_eq!(
            sanitize_component(&format!("{} {}", "a".repeat(149), "b".repeat(10))),
            "a".repeat(149)
        );
        assert_eq!(
            sanitize_component(&format!("{}. {}.jpg", "a".repeat(144), "b".repeat(10))),
            format!("{}.jpg", "a".repeat(144))
        );
        // The device-name prefix still fits.
        let reserved = format!("nul.{}", "x".repeat(200));
        let sanitized = sanitize_component(&reserved);
        assert!(sanitized.starts_with("_nul."));
        assert_eq!(sanitized.len(), 150);
        for len in [149, 150, 151, 152, 1000] {
            for ch in ["a", "é", "日", "😀"] {
                let name = format!("{}.webp", ch.repeat(len));
                let sanitized = sanitize_component(&name);
                assert!(sanitized.len() <= 150, "{len} {ch}");
                assert!(sanitized.ends_with(".webp"), "{len} {ch}");
            }
        }
    }

    #[test]
    fn unique_path_appends_counters() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("photo.jpg");
        assert_eq!(unique_path(&photo), photo);

        fs::write(&photo, b"x").unwrap();
        let first = unique_path(&photo);
        assert_eq!(first, dir.path().join("photo (1).jpg"));
        fs::write(&first, b"x").unwrap();
        assert_eq!(unique_path(&photo), dir.path().join("photo (2).jpg"));

        let readme = dir.path().join("README");
        fs::write(&readme, b"x").unwrap();
        assert_eq!(unique_path(&readme), dir.path().join("README (1)"));

        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        assert_eq!(unique_path(&folder), dir.path().join("folder (1)"));

        let archive = dir.path().join("archive.tar.gz");
        fs::write(&archive, b"x").unwrap();
        assert_eq!(unique_path(&archive), dir.path().join("archive.tar (1).gz"));

        let unicode = dir.path().join("Café photo 😀.jpg");
        fs::write(&unicode, b"x").unwrap();
        assert_eq!(
            unique_path(&unicode),
            dir.path().join("Café photo 😀 (1).jpg")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unique_path_treats_broken_symlinks_as_taken() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.jpg");
        std::os::unix::fs::symlink(dir.path().join("missing"), &link).unwrap();
        assert_eq!(unique_path(&link), dir.path().join("link (1).jpg"));
    }
}
