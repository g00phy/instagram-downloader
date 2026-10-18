//! Path helpers for partial and final files.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Highest `N` tried by [`free_path`] before giving up.
const MAX_SUFFIX: u32 = 9_999;

/// The partial file for `dest`: `.part` appended to the full file name
/// (`photo.jpg` → `photo.jpg.part`), in the same directory.
///
/// A path without a file name (`/`, `..`) gets `.part` as its own name inside it.
pub fn part_path(dest: &Path) -> PathBuf {
    match dest.file_name() {
        Some(name) => {
            let mut part = name.to_os_string();
            part.push(".part");
            dest.with_file_name(part)
        }
        None => dest.join(".part"),
    }
}

/// The first `stem (N).ext` next to `dest` that does not exist yet, so a file that
/// appeared at `dest` during the transfer is never overwritten.
pub(super) async fn free_path(dest: &Path) -> std::io::Result<Option<PathBuf>> {
    let stem = dest.file_stem().map(OsString::from).unwrap_or_default();
    let ext = dest.extension();
    for n in 1..=MAX_SUFFIX {
        let mut name = stem.clone();
        name.push(format!(" ({n})"));
        if let Some(ext) = ext {
            name.push(".");
            name.push(ext);
        }
        let candidate = dest.with_file_name(name);
        if !tokio::fs::try_exists(&candidate).await? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Size of the partial file, 0 if it does not exist.
pub(super) async fn existing_len(path: &Path) -> std::io::Result<u64> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() => Ok(meta.len()),
        Ok(_) => Ok(0),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err),
    }
}

/// Deletes `path`, treating "already gone" as success.
pub(super) async fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_appends_to_full_name() {
        assert_eq!(
            part_path(Path::new("dir/a b.tar.gz")),
            PathBuf::from("dir/a b.tar.gz.part")
        );
        assert_eq!(
            part_path(Path::new("日本/写真.jpg")),
            PathBuf::from("日本/写真.jpg.part")
        );
    }

    #[tokio::test]
    async fn free_path_skips_taken_names() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("photo.jpg");
        std::fs::write(&dest, b"x").unwrap();
        std::fs::write(dir.path().join("photo (1).jpg"), b"x").unwrap();
        let free = free_path(&dest).await.unwrap().unwrap();
        assert_eq!(free, dir.path().join("photo (2).jpg"));

        let bare = dir.path().join("README");
        assert_eq!(
            free_path(&bare).await.unwrap().unwrap(),
            dir.path().join("README (1)")
        );
    }
}
