//! Per-machine `device_id` (64 lowercase hex chars) stored at `$XDG_DATA_HOME/ox/user-id`.
//! A filesystem failure falls back to an ephemeral id, so storage problems do not block startup.

use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use crate::util::fs::{atomic_write_private, create_private_dir_all};
use crate::util::path::{XdgBase, app_dir};

const FILE_NAME: &str = "user-id";
const ID_LEN: usize = 64;

/// Loads the persisted id, mints one if absent, and uses an ephemeral id on filesystem failure.
pub fn load_or_create_device_id() -> String {
    fallback_to_ephemeral(try_load_or_create())
}

fn fallback_to_ephemeral(result: Result<String>) -> String {
    match result {
        Ok(id) => id,
        Err(e) => {
            warn!(
                error = %format_args!("{e:#}"),
                "device-id storage unavailable, using ephemeral id"
            );
            generate()
        }
    }
}

fn try_load_or_create() -> Result<String> {
    let path = device_id_path().context("failed to determine the device-id storage location")?;
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
    app_dir(XdgBase::Data).map(|dir| dir.join(FILE_NAME))
}

fn read_existing(path: &Path) -> Result<Option<String>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read device ID at {}", path.display()));
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
    let mut bytes = [0u8; ID_LEN / 2];
    getrandom::fill(&mut bytes).expect("the OS random source is readable");
    let mut id = String::with_capacity(ID_LEN);
    for byte in bytes {
        _ = write!(&mut id, "{byte:02x}");
    }
    id
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    mod load_or_create_device_id {
        use super::*;

        #[test]
        fn stores_the_id_under_the_data_dir() {
            let dir = tempdir().unwrap();

            let id =
                temp_env::with_var("XDG_DATA_HOME", Some(dir.path()), load_or_create_device_id);

            assert!(is_valid_id(&id), "{id}");
            let on_disk = fs::read_to_string(dir.path().join("ox/user-id")).unwrap();
            assert_eq!(on_disk, id);
        }
    }

    mod fallback_to_ephemeral {
        use super::*;

        #[test]
        fn keeps_the_persisted_id() {
            let persisted = "a".repeat(64);
            assert_eq!(fallback_to_ephemeral(Ok(persisted.clone())), persisted);
        }

        #[test]
        fn mints_an_id_when_storage_fails() {
            let id = fallback_to_ephemeral(Err(anyhow::anyhow!("disk full")));
            assert!(is_valid_id(&id), "fallback mints a valid id: {id}");
        }
    }

    mod try_load_or_create_at {
        use super::*;

        #[test]
        fn persists_across_calls() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("ox/user-id");

            let id1 = try_load_or_create_at(&path).unwrap();
            let id2 = try_load_or_create_at(&path).unwrap();

            assert!(is_valid_id(&id1));
            assert_eq!(id1, id2, "second call returns the persisted id");
            assert_eq!(fs::read_to_string(&path).unwrap(), id1);
        }

        #[test]
        fn replaces_invalid_persisted_value() {
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
        fn propagates_unwritable_parent_as_error() {
            // `blocker` is a regular file, so no path beneath it can be read or created.
            let dir = tempdir().unwrap();
            let blocker = dir.path().join("blocker");
            fs::write(&blocker, "").unwrap();

            let err = try_load_or_create_at(&blocker.join("ox/user-id")).unwrap_err();

            let chain = format!("{err:#}");
            assert!(
                chain.contains(&*blocker.to_string_lossy()),
                "actionable path in error: {chain}"
            );
        }

        #[test]
        fn errors_on_path_without_parent() {
            // Empty path: NotFound from read_existing, then `.parent()` is None.
            let err = try_load_or_create_at(Path::new("")).unwrap_err();
            assert_eq!(format!("{err:#}"), "device-id path has no parent");
        }
    }

    mod read_existing {
        use super::*;

        #[test]
        fn treats_non_utf8_content_as_absent() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("user-id");
            fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();

            assert!(read_existing(&path).unwrap().is_none());
        }

        #[test]
        fn propagates_io_error_other_than_not_found() {
            // Reading a directory as a file errors with IsADirectory.
            let dir = tempdir().unwrap();
            let err = read_existing(dir.path()).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("failed to read device ID at {}", dir.path().display()),
            );
        }
    }

    mod is_valid_id {
        use super::*;

        #[test]
        fn accepts_only_64_lowercase_hex_chars() {
            assert!(is_valid_id(&"a".repeat(64)));
            assert!(!is_valid_id(&"A".repeat(64)), "rejects uppercase");
            assert!(!is_valid_id(&"g".repeat(64)), "rejects non-hex");
            assert!(!is_valid_id(&"a".repeat(63)), "rejects short");
            assert!(!is_valid_id(&"a".repeat(65)), "rejects long");
        }
    }

    mod generate {
        use super::*;

        #[test]
        fn produces_unique_64_char_lowercase_hex() {
            let a = generate();
            let b = generate();

            assert!(is_valid_id(&a), "{a}");
            assert!(is_valid_id(&b), "{b}");
            assert_ne!(a, b, "two calls produce distinct ids");
        }
    }
}
