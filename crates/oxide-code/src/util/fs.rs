//! Filesystem helpers for persisting private state (0o700 dirs, 0o600 files).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::debug;
use uuid::Uuid;

/// Creates `path` (and parents) with `0o700` perms on Unix.
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
            debug!("failed to tighten {} to 0o700: {e}", path.display());
        }
    }
    Ok(())
}

/// Atomically writes `bytes` to `path` with `0o600` perms on Unix.
///
/// Writes to a sibling temp file (same parent so `rename` stays a directory-internal atomic op),
/// flushes contents to disk, then renames over the destination. On any failure the temp file is
/// removed instead of being leaked.
pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
        Uuid::new_v4().simple(),
    ));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("failed to create temp file {}", tmp.display()))?;

    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(anyhow::Error::from);
    if let Err(e) = write_result {
        _ = fs::remove_file(&tmp);
        return Err(e.context(format!("failed to write temp file {}", tmp.display())));
    }
    drop(file);

    if let Err(e) = fs::rename(&tmp, path) {
        _ = fs::remove_file(&tmp);
        return Err(
            anyhow::Error::from(e).context(format!("failed to install file at {}", path.display()))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

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

    #[test]
    fn create_private_dir_all_errors_with_actionable_path_when_parent_is_a_file() {
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"").unwrap();
        let target = blocker.join("nested");
        let err = create_private_dir_all(&target).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("failed to create") && chain.contains("nested"),
            "actionable error: {chain}"
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
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp leftovers: {leftovers:?}");
    }

    #[test]
    fn atomic_write_private_errors_when_parent_is_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does/not/exist/file");
        let err = atomic_write_private(&path, b"x").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("failed to create temp file"),
            "actionable error: {chain}"
        );
    }

    #[test]
    fn atomic_write_private_errors_when_path_has_no_parent() {
        let err = atomic_write_private(Path::new("/"), b"x").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("no parent directory"),
            "actionable error: {chain}"
        );
    }

    #[test]
    fn atomic_write_private_cleans_up_temp_when_rename_fails() {
        // POSIX `rename(file, existing_dir)` errors with EISDIR; the
        // function must remove the temp file rather than leak it.
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();

        let err = atomic_write_private(&target, b"x").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("failed to install file"),
            "actionable error: {chain}"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp cleaned up: {leftovers:?}");
    }
}
