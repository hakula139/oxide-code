//! Filesystem helpers for persisting private state (0700 dirs, 0600 files).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result};
use tempfile::Builder;

/// Creates `path` and its missing parents with `0700` perms on Unix, also tightening an existing
/// `path` to `0700` when permitted.
pub(crate) fn create_private_dir_all(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        use tracing::debug;

        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
            debug!("failed to tighten {} to 0o700: {e}", path.display());
        }
    }
    Ok(())
}

/// Atomically writes `bytes` to `path` with `0600` perms on Unix by renaming a synced
/// `.<name>.tmp.*` file beside `path` over it. A failure removes the temp file.
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

/// Opens `path` for appending with `0600` perms on Unix, also tightening an existing file's mode.
pub(crate) fn open_private_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};

    /// A symlink to a file. Windows needs Developer Mode or admin rights for it, which CI has.
    pub(crate) fn symlink_file(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }

    /// A symlink to a directory, needing the same Windows rights as [`symlink_file`].
    pub(crate) fn symlink_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    }

    /// Denies reading the file or directory at `path` until the returned guard drops, which
    /// restores access so the temp dir can be removed.
    pub(crate) fn deny_read_access(path: &Path) -> AccessDenied {
        #[cfg(unix)]
        let denied = AccessDenied::with_mode(path, 0o000);
        // `(RD)` is the right to read a file's data or list a directory.
        #[cfg(windows)]
        let denied = AccessDenied::with_right(path, "(RD)");

        let readable = if path.is_dir() {
            fs::read_dir(path).is_ok()
        } else {
            File::open(path).is_ok()
        };
        assert!(!readable, "{} stayed readable", path.display());
        denied
    }

    /// Denies creating subdirectories in `dir` until the returned guard drops, which restores
    /// access.
    pub(crate) fn deny_subdir_creation(dir: &Path) -> AccessDenied {
        #[cfg(unix)]
        let denied = AccessDenied::with_mode(dir, 0o555);
        // `(AD)` is the right to create a subdirectory.
        #[cfg(windows)]
        let denied = AccessDenied::with_right(dir, "(AD)");

        assert!(
            fs::create_dir(dir.join("probe")).is_err(),
            "{} accepted a subdirectory",
            dir.display()
        );
        denied
    }

    /// Access denied by [`deny_read_access`] or [`deny_subdir_creation`], restored on drop.
    pub(crate) struct AccessDenied {
        path: PathBuf,
        #[cfg(unix)]
        mode: u32,
    }

    impl AccessDenied {
        /// Sets the permission bits of `path` to `mode`, keeping the old bits to restore.
        #[cfg(unix)]
        fn with_mode(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let old = fs::metadata(path).unwrap().permissions().mode();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            Self {
                path: path.to_owned(),
                mode: old,
            }
        }

        /// Denies Everyone the `icacls` `right` on `path`.
        #[cfg(windows)]
        fn with_right(path: &Path, right: &str) -> Self {
            icacls(path, &["/deny", format!("{EVERYONE}:{right}").as_str()]);
            Self {
                path: path.to_owned(),
            }
        }
    }

    impl Drop for AccessDenied {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;

                fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode)).unwrap();
            }
            #[cfg(windows)]
            icacls(&self.path, &["/remove:d", EVERYONE]);
        }
    }

    /// The well-known SID of Everyone. A deny entry for it binds the file's owner too.
    #[cfg(windows)]
    const EVERYONE: &str = "*S-1-1-0";

    #[cfg(windows)]
    fn icacls(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("icacls")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    mod create_private_dir_all {
        use super::*;

        #[test]
        fn creates_nested_dirs() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("a/b/c");

            create_private_dir_all(&target).unwrap();

            assert!(target.is_dir());
        }

        #[test]
        fn is_idempotent() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("nested/leaf");
            create_private_dir_all(&target).unwrap();

            create_private_dir_all(&target).unwrap();

            assert!(target.is_dir());
        }

        #[test]
        fn errors_with_actionable_path_when_parent_is_a_file() {
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
        fn sets_mode_0o700_on_new_dirs() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("private");

            create_private_dir_all(&target).unwrap();

            assert_eq!(mode(&target), 0o700);
        }

        #[cfg(unix)]
        #[test]
        fn tightens_lax_existing_directory() {
            use std::os::unix::fs::DirBuilderExt as _;

            let dir = tempdir().unwrap();
            let target = dir.path().join("lax");
            fs::DirBuilder::new().mode(0o755).create(&target).unwrap();

            create_private_dir_all(&target).unwrap();

            assert_eq!(mode(&target), 0o700);
        }

        #[cfg(unix)]
        #[test]
        fn tolerates_existing_directory_it_cannot_chmod() {
            use std::os::unix::fs::MetadataExt as _;

            let root = Path::new("/");
            let own_uid = fs::metadata(tempdir().unwrap().path()).unwrap().uid();
            let before = fs::metadata(root).unwrap();
            if before.uid() == own_uid {
                return;
            }

            create_private_dir_all(root).unwrap();

            assert_eq!(fs::metadata(root).unwrap().mode(), before.mode());
        }
    }

    mod atomic_write_private {
        use super::*;

        fn entry_names(dir: &Path) -> Vec<OsString> {
            let mut names: Vec<OsString> = fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        }

        #[test]
        fn writes_bytes_and_replaces_existing() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("file");

            atomic_write_private(&path, b"first").unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"first");

            atomic_write_private(&path, b"second").unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"second");
        }

        #[test]
        fn does_not_leave_temp_file_on_success() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("file");

            atomic_write_private(&path, b"x").unwrap();

            assert_eq!(entry_names(dir.path()), ["file"]);
        }

        #[test]
        fn errors_when_parent_is_missing() {
            let dir = tempdir().unwrap();
            let parent = dir.path().join("does/not/exist");

            let err = atomic_write_private(&parent.join("file"), b"x").unwrap_err();

            assert_eq!(
                err.to_string(),
                format!("failed to create a temp file in {}", parent.display()),
            );
        }

        #[test]
        fn errors_when_path_has_no_parent() {
            let err = atomic_write_private(Path::new("/"), b"x").unwrap_err();

            assert_eq!(err.to_string(), "path / has no parent directory");
        }

        #[test]
        fn cleans_up_temp_when_rename_fails() {
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
        fn sets_mode_0o600_on_new_files() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("file");

            atomic_write_private(&path, b"x").unwrap();

            assert_eq!(mode(&path), 0o600);
        }
    }

    mod open_private_append {
        use super::*;

        #[test]
        fn writes_after_existing_content() {
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
        fn tightens_lax_existing_file() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("log");
            fs::write(&path, b"").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

            open_private_append(&path).unwrap();

            assert_eq!(mode(&path), 0o600);
        }
    }
}
