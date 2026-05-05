//! Per-machine `device_id` (64 lowercase hex chars) at `$XDG_DATA_HOME/ox/user-id`. Lazily minted;
//! filesystem failure falls back to an ephemeral id rather than blocking client construction.

use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;
use uuid::Uuid;

use crate::util::fs::{atomic_write_private, create_private_dir_all};
use crate::util::path::xdg_dir;

const DATA_DIR: &str = "ox";
const FILE_NAME: &str = "user-id";
const ID_LEN: usize = 64;

/// Loads the persisted id, minting one if absent; falls back to ephemeral on filesystem failure.
pub(super) fn load_or_create_device_id() -> String {
    fallback_to_ephemeral(try_load_or_create())
}

fn fallback_to_ephemeral(result: Result<String>) -> String {
    match result {
        Ok(id) => id,
        Err(e) => {
            warn!("device-id storage unavailable, using ephemeral id: {e:#}");
            generate()
        }
    }
}

fn try_load_or_create() -> Result<String> {
    let path = device_id_path().context("cannot determine device-id storage location")?;
    try_load_or_create_at(&path)
}

fn try_load_or_create_at(path: &Path) -> Result<String> {
    if let Some(existing) = read_existing(path)? {
        return Ok(existing);
    }
    let parent = path.parent().context("device-id path has no parent")?;
    create_private_dir_all(parent)?;
    let id = generate();
    atomic_write_private(path, id.as_bytes())?;
    Ok(id)
}

fn device_id_path() -> Option<PathBuf> {
    xdg_dir(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        dirs::home_dir(),
        Path::new(".local/share"),
        &Path::new(DATA_DIR).join(FILE_NAME),
    )
}

fn read_existing(path: &Path) -> Result<Option<String>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(format!("failed to read device id at {}", path.display())));
        }
    };
    let trimmed = std::str::from_utf8(&bytes).map(str::trim).ok();
    Ok(trimmed.filter(|s| is_valid_id(s)).map(str::to_owned))
}

fn is_valid_id(s: &str) -> bool {
    s.len() == ID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn generate() -> String {
    let mut buf = String::with_capacity(ID_LEN);
    let a = Uuid::new_v4().into_bytes();
    let b = Uuid::new_v4().into_bytes();
    for byte in a.iter().chain(b.iter()) {
        write!(&mut buf, "{byte:02x}").expect("writing to a String never fails");
    }
    buf
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    // ── load_or_create_device_id ──

    #[test]
    fn load_or_create_device_id_produces_valid_id_under_normal_env() {
        // Don't override XDG_DATA_HOME — parallel Client::new() calls would race the tempdir.
        let id = load_or_create_device_id();
        assert!(is_valid_id(&id), "{id}");
    }

    // ── fallback_to_ephemeral ──

    #[test]
    fn fallback_to_ephemeral_uses_persisted_id_on_ok() {
        let persisted = "a".repeat(64);
        assert_eq!(fallback_to_ephemeral(Ok(persisted.clone())), persisted);
    }

    #[test]
    fn fallback_to_ephemeral_mints_fresh_id_on_err() {
        let id = fallback_to_ephemeral(Err(anyhow::anyhow!("disk full")));
        assert!(is_valid_id(&id), "fallback mints a valid id: {id}");
    }

    // ── try_load_or_create_at ──

    #[test]
    fn try_load_or_create_at_persists_across_calls() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ox/user-id");
        let id1 = try_load_or_create_at(&path).unwrap();
        let id2 = try_load_or_create_at(&path).unwrap();
        assert!(is_valid_id(&id1));
        assert_eq!(id1, id2, "second call returns the persisted id");
    }

    #[test]
    fn try_load_or_create_at_writes_id_to_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ox/user-id");
        let id = try_load_or_create_at(&path).unwrap();
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.trim(), id);
    }

    #[test]
    fn try_load_or_create_at_replaces_invalid_persisted_value() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ox/user-id");
        create_private_dir_all(path.parent().unwrap()).unwrap();
        atomic_write_private(&path, b"not-a-valid-id").unwrap();
        let id = try_load_or_create_at(&path).unwrap();
        assert!(is_valid_id(&id));
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.trim(), id, "invalid value rewritten");
    }

    #[test]
    fn try_load_or_create_at_propagates_unwritable_parent_as_error() {
        // Parent is a regular file (`/dev/null`), so `mkdir` fails.
        let path = Path::new("/dev/null/ox/user-id");
        let err = try_load_or_create_at(path).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("/dev/null"),
            "actionable path in error: {chain}"
        );
    }

    #[test]
    fn try_load_or_create_at_errors_on_path_without_parent() {
        // Empty path: NotFound from read_existing, then `.parent()` is None.
        let err = try_load_or_create_at(Path::new("")).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no parent"),
            "actionable error for parentless path: {chain}"
        );
    }

    // ── read_existing ──

    #[test]
    fn read_existing_treats_non_utf8_content_as_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("user-id");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(read_existing(&path).unwrap().is_none());
    }

    #[test]
    fn read_existing_propagates_io_error_other_than_not_found() {
        // Reading a directory as a file errors with IsADirectory.
        let dir = tempdir().unwrap();
        let err = read_existing(dir.path()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("failed to read device id"),
            "wrap message: {chain}"
        );
    }

    // ── is_valid_id ──

    #[test]
    fn is_valid_id_accepts_only_64_lowercase_hex_chars() {
        assert!(is_valid_id(&"a".repeat(64)));
        assert!(!is_valid_id(&"A".repeat(64)), "rejects uppercase");
        assert!(!is_valid_id(&"g".repeat(64)), "rejects non-hex");
        assert!(!is_valid_id(&"a".repeat(63)), "rejects short");
        assert!(!is_valid_id(&"a".repeat(65)), "rejects long");
    }

    // ── generate ──

    #[test]
    fn generate_produces_unique_64_char_lowercase_hex() {
        let a = generate();
        let b = generate();
        assert!(is_valid_id(&a), "{a}");
        assert!(is_valid_id(&b), "{b}");
        assert_ne!(a, b, "two calls produce distinct ids");
    }
}
