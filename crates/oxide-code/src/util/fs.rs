//! Filesystem helpers for persisting private state (0o700 dirs, 0o600 files).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::Builder;

/// Creates `path` and its missing parents with `0o700` perms on Unix, also tightening an existing
/// `path` to `0o700` when permitted.
pub(crate) fn create_private_dir_all(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        use tracing::debug;

        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
            debug!("failed to tighten {} to 0o700: {e}", path.display());
        }
    }
    Ok(())
}

/// Atomically writes `bytes` to `path` with `0o600` perms on Unix.
///
/// The bytes are synced to a `.<name>.tmp.*` file beside `path`, then renamed over `path`. Any
/// failure removes the temp file.
pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;
    let name = path.file_name().unwrap_or_default().display();
    let mut file = Builder::new()
        .prefix(&format!(".{name}.tmp."))
        .tempfile_in(parent)
        .with_context(|| format!("failed to create a temp file in {}", parent.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.as_file().sync_all())
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("failed to install file at {}", path.display()))?;
    Ok(())
}

/// Opens `path` for appending with `0o600` perms on Unix, also tightening an existing file's mode.
pub(crate) fn open_private_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    fn entry_names(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<OsString> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // ── create_private_dir_all ──

    #[test]
    fn create_private_dir_all_creates_nested_dirs() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("a/b/c");

        create_private_dir_all(&target).unwrap();

        assert!(target.is_dir());
    }

    #[test]
    fn create_private_dir_all_is_idempotent() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested/leaf");
        create_private_dir_all(&target).unwrap();

        create_private_dir_all(&target).unwrap();

        assert!(target.is_dir());
    }

    #[test]
    fn create_private_dir_all_errors_with_actionable_path_when_parent_is_a_file() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"").unwrap();
        let target = blocker.join("nested");

        let err = create_private_dir_all(&target).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("failed to create {}", target.display()),
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_all_sets_mode_0o700_on_new_dirs() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("private");

        create_private_dir_all(&target).unwrap();

        assert_eq!(mode(&target), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_all_tightens_lax_existing_directory() {
        use std::os::unix::fs::DirBuilderExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("lax");
        fs::DirBuilder::new().mode(0o755).create(&target).unwrap();

        create_private_dir_all(&target).unwrap();

        assert_eq!(mode(&target), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_all_tolerates_existing_directory_it_cannot_chmod() {
        use std::os::unix::fs::MetadataExt;

        let root = Path::new("/");
        let own_uid = fs::metadata(tempdir().unwrap().path()).unwrap().uid();
        let before = fs::metadata(root).unwrap();
        if before.uid() == own_uid {
            return;
        }

        create_private_dir_all(root).unwrap();

        assert_eq!(fs::metadata(root).unwrap().mode(), before.mode());
    }

    // ── atomic_write_private ──

    #[test]
    fn atomic_write_private_writes_bytes_and_replaces_existing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");

        atomic_write_private(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        atomic_write_private(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn atomic_write_private_does_not_leave_temp_file_on_success() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");

        atomic_write_private(&path, b"x").unwrap();

        assert_eq!(entry_names(dir.path()), ["file"]);
    }

    #[test]
    fn atomic_write_private_errors_when_parent_is_missing() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("does/not/exist");

        let err = atomic_write_private(&parent.join("file"), b"x").unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("failed to create a temp file in {}", parent.display()),
        );
    }

    #[test]
    fn atomic_write_private_errors_when_path_has_no_parent() {
        let err = atomic_write_private(Path::new("/"), b"x").unwrap_err();

        assert_eq!(err.to_string(), "path / has no parent directory");
    }

    #[test]
    fn atomic_write_private_cleans_up_temp_when_rename_fails() {
        // Renaming a file over an existing directory fails on every platform.
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();

        let err = atomic_write_private(&target, b"x").unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("failed to install file at {}", target.display()),
        );
        assert_eq!(entry_names(dir.path()), ["target"]);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_private_sets_mode_0o600_on_new_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");

        atomic_write_private(&path, b"x").unwrap();

        assert_eq!(mode(&path), 0o600);
    }

    // ── open_private_append ──

    #[test]
    fn open_private_append_writes_after_existing_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log");
        fs::write(&path, b"first\n").unwrap();

        open_private_append(&path)
            .unwrap()
            .write_all(b"second\n")
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"first\nsecond\n");
    }

    #[cfg(unix)]
    #[test]
    fn open_private_append_tightens_lax_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("log");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        open_private_append(&path).unwrap();

        assert_eq!(mode(&path), 0o600);
    }
}
