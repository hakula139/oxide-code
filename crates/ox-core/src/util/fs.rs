//! Bounded regular-file access and private-state persistence (0700 dirs, 0600 files).

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result};
use tempfile::Builder;

// ── Regular File Reading ──

#[derive(Debug, thiserror::Error)]
pub(crate) enum FileReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("not a regular file: directory")]
    Directory,
    #[error("not a regular file")]
    NonRegular,
    #[error("too large to read ({size} bytes, max {max} bytes)")]
    TooLarge { size: u64, max: u64 },
}

/// Reads a regular file through a no-follow handle, consuming at most `limit + 1` bytes and
/// rejecting overflow even if the file grew after its metadata was checked.
pub(crate) fn read_regular_file(
    path: &Path,
    limit: u64,
) -> Result<(Metadata, Vec<u8>), FileReadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    let file = open_no_follow(path, options)?;
    let metadata = file.metadata()?;
    let bytes = read_regular_contents(file, &metadata, limit)?;
    Ok((metadata, bytes))
}

fn read_regular_contents(
    reader: impl Read,
    metadata: &Metadata,
    limit: u64,
) -> Result<Vec<u8>, FileReadError> {
    validate_regular_file(metadata, limit)?;
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let size = bytes.len() as u64;
    if size > limit {
        return Err(FileReadError::TooLarge { size, max: limit });
    }
    Ok(bytes)
}

/// Checks that `metadata` names a regular file within `limit` bytes.
pub(crate) fn validate_regular_file(metadata: &Metadata, limit: u64) -> Result<(), FileReadError> {
    if metadata.is_dir() {
        return Err(FileReadError::Directory);
    }
    if !metadata.is_file() {
        return Err(FileReadError::NonRegular);
    }
    let size = metadata.len();
    if size > limit {
        return Err(FileReadError::TooLarge { size, max: limit });
    }
    Ok(())
}

// ── No-Follow Open ──

/// Opens `path` without following a final symlink or blocking on a FIFO.
#[cfg(unix)]
pub(crate) fn open_no_follow(path: &Path, mut options: OpenOptions) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    use nix::fcntl::OFlag;

    options
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
}

/// `FILE_FLAG_OPEN_REPARSE_POINT` opens a reparse point itself, so a symlink or junction there
/// fails the open as `O_NOFOLLOW` makes it fail on Unix. Other reparse points need their file
/// system filter, so a second open keeps its handle only when it reaches the same file.
#[cfg(windows)]
pub(crate) fn open_no_follow(path: &Path, mut options: OpenOptions) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let file = options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(file);
    }
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other("the final path component is a symlink"));
    }

    let reopened = options.custom_flags(0).open(path)?;
    if file_id(&reopened)? == file_id(&file)? {
        Ok(reopened)
    } else {
        Err(io::Error::other("the file changed while it was opened"))
    }
}

/// The volume and full file ID, including the upper 64 bits required by ReFS.
#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "the Windows file ID query has no safe standard library counterpart"
)]
pub(crate) fn file_id(file: impl std::os::windows::io::AsHandle) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut info = FILE_ID_INFO::default();
    // The handle stays borrowed and Windows receives an aligned output of exactly FILE_ID_INFO size.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_handle().as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            u32::try_from(std::mem::size_of_val(&info)).expect("FILE_ID_INFO fits u32"),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

// ── Private State ──

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

    /// Creates `link` as a symlink to the file `target`. Windows needs Developer Mode or admin
    /// rights for it, which CI has.
    pub(crate) fn symlink_file(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }

    /// Creates `link` as a symlink to the directory `target`, needing the same Windows rights as
    /// [`symlink_file`].
    pub(crate) fn symlink_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    }

    /// Denies reading the file or directory at `path` until the returned guard drops.
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

    /// Denies creating subdirectories in `dir` until the returned guard drops.
    pub(crate) fn deny_subdir_creation(dir: &Path) -> AccessDenied {
        #[cfg(unix)]
        let denied = AccessDenied::with_mode(dir, 0o555);
        // `(AD)` is the right to create a subdirectory.
        #[cfg(windows)]
        let denied = AccessDenied::with_right(dir, "(AD)");

        let created = fs::create_dir(dir.join("probe")).is_ok();
        assert!(!created, "{} accepted a subdirectory", dir.display());
        denied
    }

    /// Access denied by [`deny_read_access`] or [`deny_subdir_creation`], restored on drop so the
    /// temp dir can be removed.
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
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

    mod file_read_error {
        use super::*;

        #[test]
        fn describes_io_type_and_size_failures() {
            let cases = [
                (
                    "io",
                    FileReadError::Io(io::Error::other("fixture failure")),
                    "fixture failure",
                ),
                (
                    "directory",
                    FileReadError::Directory,
                    "not a regular file: directory",
                ),
                (
                    "non-regular",
                    FileReadError::NonRegular,
                    "not a regular file",
                ),
                (
                    "size limit",
                    FileReadError::TooLarge { size: 9, max: 8 },
                    "too large to read (9 bytes, max 8 bytes)",
                ),
            ];

            for (name, error, expected) in cases {
                assert_eq!(error.to_string(), expected, "{name}");
            }
        }
    }

    mod read_regular_file {
        use super::*;
        use crate::util::fs::testing::symlink_file;

        #[test]
        fn reads_empty_and_exact_limit_files() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            let cases = [
                ("empty", b"".as_slice(), 0),
                ("at limit", b"bounded".as_slice(), 7),
            ];

            for (name, content, limit) in cases {
                fs::write(&path, content).unwrap();

                let (metadata, bytes) = read_regular_file(&path, limit).unwrap();

                assert_eq!(metadata.len(), content.len() as u64, "{name}");
                assert_eq!(bytes, content, "{name}");
            }
        }

        #[test]
        fn rejects_a_file_above_the_requested_limit() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            fs::write(&path, b"too large").unwrap();

            let error = read_regular_file(&path, 8).unwrap_err();

            assert!(matches!(error, FileReadError::TooLarge { size: 9, max: 8 }));
        }

        #[test]
        fn preserves_a_missing_files_io_error() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("missing.txt");

            let error = read_regular_file(&path, 8).unwrap_err();

            assert!(
                matches!(error, FileReadError::Io(ref error) if error.kind() == io::ErrorKind::NotFound)
            );
        }

        #[test]
        fn refuses_a_final_symlink() {
            let dir = tempdir().unwrap();
            let target = dir.path().join("target.txt");
            let link = dir.path().join("link.txt");
            fs::write(&target, b"original").unwrap();
            symlink_file(&target, &link);

            let error = read_regular_file(&link, 8).unwrap_err();

            assert!(matches!(error, FileReadError::Io(_)));
            #[cfg(unix)]
            let expected =
                io::Error::from_raw_os_error(nix::errno::Errno::ELOOP as i32).to_string();
            #[cfg(windows)]
            let expected = "the final path component is a symlink";
            assert_eq!(error.to_string(), expected);
            assert_eq!(fs::read(&target).unwrap(), b"original");
        }

        #[cfg(unix)]
        #[test]
        fn refuses_a_fifo_without_waiting_for_a_writer() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("pipe");
            nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();

            std::thread::spawn(move || {
                done_tx.send(read_regular_file(&path, 8)).unwrap();
            });
            let result = done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();

            assert!(matches!(result, Err(FileReadError::NonRegular)));
        }
    }

    mod read_regular_contents {
        use super::*;

        struct Counted<R> {
            reader: R,
            consumed: usize,
        }

        impl<R: Read> Read for Counted<R> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.reader.read(buffer)?;
                self.consumed += count;
                Ok(count)
            }
        }

        #[test]
        fn stops_after_one_extra_byte_when_a_file_grows_after_metadata() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("growing.txt");
            fs::write(&path, b"old").unwrap();
            let file = File::open(&path).unwrap();
            let metadata = file.metadata().unwrap();
            let mut counted = Counted {
                reader: file,
                consumed: 0,
            };
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b" plus newly appended bytes")
                .unwrap();

            let error = read_regular_contents(&mut counted, &metadata, 8).unwrap_err();

            assert_eq!(metadata.len(), 3);
            assert_eq!(counted.consumed, 9);
            assert!(matches!(error, FileReadError::TooLarge { size: 9, max: 8 }));
            assert_eq!(fs::read(&path).unwrap(), b"old plus newly appended bytes");
        }

        #[test]
        fn rejects_oversized_metadata_before_consuming_bytes() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            fs::write(&path, b"too large").unwrap();
            let file = File::open(&path).unwrap();
            let metadata = file.metadata().unwrap();
            let mut counted = Counted {
                reader: file,
                consumed: 0,
            };

            let error = read_regular_contents(&mut counted, &metadata, 8).unwrap_err();

            assert_eq!(counted.consumed, 0);
            assert!(matches!(error, FileReadError::TooLarge { size: 9, max: 8 }));
        }

        #[test]
        fn preserves_an_error_during_consumption() {
            struct FailedReader;

            impl Read for FailedReader {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "fixture read failure",
                    ))
                }
            }

            let file = tempfile::tempfile().unwrap();
            let metadata = file.metadata().unwrap();

            let error = read_regular_contents(FailedReader, &metadata, 8).unwrap_err();

            assert!(
                matches!(error, FileReadError::Io(ref error) if error.kind() == io::ErrorKind::PermissionDenied)
            );
            assert_eq!(error.to_string(), "fixture read failure");
        }
    }

    mod validate_regular_file {
        use super::*;

        #[test]
        fn validates_regular_files_size_and_directories() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            fs::write(&path, b"bounded").unwrap();
            let cases = [
                ("at limit", path.as_path(), 7, Ok(())),
                (
                    "above limit",
                    path.as_path(),
                    6,
                    Err("too large to read (7 bytes, max 6 bytes)"),
                ),
                (
                    "directory",
                    dir.path(),
                    8,
                    Err("not a regular file: directory"),
                ),
            ];

            for (name, path, limit, expected) in cases {
                let metadata = fs::metadata(path).unwrap();

                let result = validate_regular_file(&metadata, limit);

                assert_eq!(
                    result.map_err(|error| error.to_string()),
                    expected.map_err(str::to_owned),
                    "{name}"
                );
            }
        }

        #[cfg(unix)]
        #[test]
        fn refuses_socket_metadata() {
            let dir = tempdir().unwrap();
            let path = dir.path().join("socket");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let metadata = fs::metadata(&path).unwrap();

            let result = validate_regular_file(&metadata, 8);

            assert!(matches!(result, Err(FileReadError::NonRegular)));
        }
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
