//! Output filename templates (`{owner}/{category}/{date}_{id}_{index}.{ext}`).

use std::path::{Path, PathBuf};

use crate::error::Result;
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
    let _ = template;
    todo!("links agent")
}

/// Renders `template` for `media` into a relative path. `/` (and `\`) in the
/// template separate directories; each rendered component is sanitised with
/// [`sanitize_component`], so values can never add directories or escape the
/// output root. Dates are UTC.
pub fn render(template: &str, media: &MediaFile) -> Result<PathBuf> {
    let _ = (template, media);
    todo!("links agent")
}

/// Makes one path component safe on Windows, macOS and Linux: replaces
/// `<>:"/\|?*` and control characters with `_`, trims trailing dots/spaces,
/// prefixes reserved device names (`CON`, `NUL`, `COM1`, …) with `_`, limits the
/// length to 150 bytes on a char boundary (keeping the extension), and maps an
/// empty result to `_`.
pub fn sanitize_component(component: &str) -> String {
    let _ = component;
    todo!("links agent")
}

/// `path` if nothing exists there, else the first free `name (1).ext`, `name (2).ext`, ….
pub fn unique_path(path: &Path) -> PathBuf {
    let _ = path;
    todo!("links agent")
}
