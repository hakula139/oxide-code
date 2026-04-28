//! Stable per-machine `device_id` sent in `metadata.user_id`.
//!
//! Mirrors claude-code's `getOrCreateUserID` (`utils/config.ts:1757`):
//! 64 lowercase hex chars (32 random bytes), generated lazily on first
//! use and persisted under `$XDG_DATA_HOME/ox/user-id`. 3P proxies
//! fingerprint absence and malformed shape, not the value itself, so
//! a fresh ox-private id satisfies the verifier without depending on
//! claude-code's `~/.claude.json#userID` (which is private to that
//! tool's keychain and not meant to be shared).
//!
//! Never panics on filesystem errors: a missing or unwritable XDG dir
//! falls back to an in-memory id so the request still ships with a
//! valid shape. The 3P verifier checks shape, not persistence — a
//! per-process id costs only a tiny amount of cache fragmentation
//! upstream and is strictly better than missing.

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

/// Loads the persisted device id, generating and writing one if it
/// does not yet exist. Filesystem failures degrade to a fresh ephemeral
/// id rather than failing client construction.
pub(super) fn load_or_create_device_id() -> String {
    match try_load_or_create() {
        Ok(id) => id,
        Err(e) => {
            warn!("device-id storage unavailable, using ephemeral id: {e:#}");
            generate()
        }
    }
}

fn try_load_or_create() -> Result<String> {
    let path = device_id_path().context("cannot determine device-id storage location")?;
    if let Some(existing) = read_existing(&path)? {
        return Ok(existing);
    }
    let parent = path.parent().context("device-id path has no parent")?;
    create_private_dir_all(parent)?;
    let id = generate();
    atomic_write_private(&path, id.as_bytes())?;
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

/// Returns the trimmed contents of `path` when it exists *and* parses
/// as a 64-char lowercase hex string. Anything else — missing file,
/// truncated write from a crash, accidental hand-edit — is treated as
/// "no id present" so the caller mints a fresh one.
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

/// 32 random bytes (two UUID v4s), hex-encoded → 64 lowercase hex chars.
/// `Uuid::new_v4` pulls from `getrandom`, which uses the OS CSPRNG.
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
    fn load_or_create_device_id_persists_across_calls() {
        let dir = tempdir().unwrap();
        let id1 = temp_env::with_var(
            "XDG_DATA_HOME",
            Some(dir.path().to_string_lossy().into_owned()),
            load_or_create_device_id,
        );
        let id2 = temp_env::with_var(
            "XDG_DATA_HOME",
            Some(dir.path().to_string_lossy().into_owned()),
            load_or_create_device_id,
        );
        assert!(is_valid_id(&id1), "first id valid: {id1}");
        assert_eq!(id1, id2, "second call returns the persisted id");
    }

    #[test]
    fn load_or_create_device_id_writes_to_xdg_data_home() {
        let dir = tempdir().unwrap();
        let id = temp_env::with_var(
            "XDG_DATA_HOME",
            Some(dir.path().to_string_lossy().into_owned()),
            load_or_create_device_id,
        );
        let on_disk = fs::read_to_string(dir.path().join("ox/user-id")).unwrap();
        assert_eq!(on_disk.trim(), id);
    }

    #[test]
    fn load_or_create_device_id_replaces_invalid_persisted_value() {
        // A truncated or hand-edited file must be treated as "no id"
        // so the verifier sees a well-formed value instead of the
        // garbage on disk.
        let dir = tempdir().unwrap();
        let target = dir.path().join("ox/user-id");
        create_private_dir_all(target.parent().unwrap()).unwrap();
        atomic_write_private(&target, b"not-a-valid-id").unwrap();

        let id = temp_env::with_var(
            "XDG_DATA_HOME",
            Some(dir.path().to_string_lossy().into_owned()),
            load_or_create_device_id,
        );
        assert!(is_valid_id(&id), "minted id valid: {id}");
        let on_disk = fs::read_to_string(&target).unwrap();
        assert_eq!(on_disk.trim(), id, "invalid value rewritten");
    }

    // ── is_valid_id ──

    #[test]
    fn is_valid_id_rejects_uppercase_and_non_hex() {
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
