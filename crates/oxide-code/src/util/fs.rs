//! Filesystem helpers for persisting private state (0o700 dirs, 0o600 files).

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

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
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
            tracing::debug!("failed to tighten {} to 0o700: {e}", path.display());
        }
    }
    Ok(())
}

/// Atomically writes `bytes` to `path` with `0o600` perms on Unix.
///
/// The bytes are synced to a temp file in `path`'s directory, then renamed over `path`. Any
/// failure removes the temp file.
pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;
    let mut file = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temp file in {}", parent.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.as_file().sync_all())
        .with_context(|| format!("failed to write temp file {}", file.path().display()))?;
    file.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("failed to install file at {}", path.display()))?;
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn create_private_dir_all_sets_mode_0o700_on_new_dirs() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("private");
        create_private_dir_all(&target).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "expected 0o700, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_all_tightens_lax_existing_directory() {
        use std::os::unix::fs::DirBuilderExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("lax");
        fs::DirBuilder::new().mode(0o755).create(&target).unwrap();
        create_private_dir_all(&target).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "existing dir tightened: {mode:o}");
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

    #[cfg(unix)]
    #[test]
    fn atomic_write_private_sets_mode_0o600_on_new_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file");
        atomic_write_private(&path, b"x").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
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
}
