//! Filesystem helpers that behave well on Windows: text files in any encoding
//! Windows tools write, and renames that survive brief locks held by antivirus
//! scanners, the search indexer or sync clients.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};

/// Waits between rename attempts (about 1.5 s in total).
const RENAME_BACKOFF: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// Decodes text file bytes: UTF-16 LE/BE when a UTF-16 BOM is present (Windows
/// PowerShell 5.1 `>` and `Out-File` write UTF-16LE), otherwise UTF-8 with an
/// optional BOM. `None` when the bytes are neither.
pub fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(rest.to_vec()).ok()
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units = bytes.chunks_exact(2).map(|pair| unit([pair[0], pair[1]]));
    char::decode_utf16(units)
        .collect::<std::result::Result<String, _>>()
        .ok()
}

/// Reads a text file in any encoding [`decode_text`] understands. Read failures
/// are [`Error::Io`]; undecodable contents are [`Error::Config`] naming the path.
pub fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|err| Error::io(format!("cannot read {}", path.display()), err))?;
    decode_text(&bytes).ok_or_else(|| {
        Error::Config(format!(
            "{} is not UTF-8 or UTF-16 text; save it as UTF-8",
            path.display()
        ))
    })
}

/// Whether a rename failure is the kind Windows reports while another process
/// briefly holds the file (sharing or lock violation, access denied).
fn is_transient(err: &io::Error) -> bool {
    if !cfg!(windows) {
        return false;
    }
    // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33).
    err.kind() == io::ErrorKind::PermissionDenied || matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

/// [`std::fs::rename`], retried for about 1.5 s while Windows reports the file
/// as in use. Other errors (and every error elsewhere) are returned at once.
pub fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut waits = RENAME_BACKOFF.iter();
    loop {
        match std::fs::rename(from, to) {
            Err(err) if is_transient(&err) => match waits.next() {
                Some(wait) => std::thread::sleep(*wait),
                None => return Err(err),
            },
            other => return other,
        }
    }
}

/// Async [`rename_with_retry`] for tokio tasks.
pub async fn rename_with_retry_async(from: &Path, to: &Path) -> io::Result<()> {
    let mut waits = RENAME_BACKOFF.iter();
    loop {
        match tokio::fs::rename(from, to).await {
            Err(err) if is_transient(&err) => match waits.next() {
                Some(wait) => tokio::time::sleep(*wait).await,
                None => return Err(err),
            },
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_bom_and_utf16() {
        assert_eq!(decode_text(b"a\r\nb").as_deref(), Some("a\r\nb"));
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi").as_deref(), Some("hi"));
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("hé\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_text(&le).as_deref(), Some("hé\r\n"));
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("x".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode_text(&be).as_deref(), Some("x"));
        assert_eq!(decode_text(&[0xFF, 0xFE, 0x41]), None, "odd UTF-16 length");
        assert_eq!(decode_text(&[0xC3, 0x28]), None, "invalid UTF-8");
    }

    #[test]
    fn read_text_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        assert!(matches!(read_text(&missing), Err(Error::Io { .. })));
        let binary = dir.path().join("binary.txt");
        std::fs::write(&binary, [0xC3, 0x28]).unwrap();
        let err = read_text(&binary).unwrap_err();
        assert!(err.to_string().contains("binary.txt"), "{err}");
    }

    #[test]
    fn rename_moves_and_reports_missing_sources() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.part");
        let to = dir.path().join("a.jpg");
        std::fs::write(&from, b"x").unwrap();
        rename_with_retry(&from, &to).unwrap();
        assert!(to.exists() && !from.exists());
        assert!(rename_with_retry(&from, &to).is_err());
    }
}
