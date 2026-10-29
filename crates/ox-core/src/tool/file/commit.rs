//! Publication of workspace file changes.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::Path;

use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::util::fs::{FileReadError, open_no_follow, read_regular_contents};
use crate::util::path::requires_directory;

/// Creates `path` with an exclusive open, which never replaces an existing entry, including a
/// dangling symlink, and leaves the new file's mode to the umask and any default ACL. A failed
/// write leaves the partial file at `path`.
pub(super) fn create(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if requires_directory(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a new file path has a directory suffix",
        ));
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| left_partial_file(&error))
}

fn left_partial_file(error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{error}. A partial file remains. Read it before writing again."),
    )
}

/// An existing file opened once to rewrite it, holding the content read through that handle.
pub(crate) struct Checked {
    file: File,
    content: Vec<u8>,
}

/// The points of a rewrite where a test can act on the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The new bytes are written over the truncated original, before the sync.
    Written,
    /// The new bytes are synced, before the final check.
    Synced,
}

impl Checked {
    /// Opens `path` for reading and writing without following a symlink at the final component,
    /// then reads it, refusing anything but a regular file within [`MAX_TRACKED_FILE_SIZE`].
    pub(super) fn open(path: &Path) -> Result<Self, FileReadError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let mut file = open_no_follow(path, options)?;
        let metadata = file.metadata()?;
        let content = read_regular_contents(&mut file, &metadata, MAX_TRACKED_FILE_SIZE)?;
        Ok(Self { file, content })
    }

    /// The file's content as the open read it.
    pub(crate) fn content(&self) -> &[u8] {
        &self.content
    }

    /// Rewrites the file at `path` through the held handle, which keeps the inode with its
    /// metadata and every hard link. Refuses a file whose content changed since the open, and
    /// checks afterward that `path` still names the file and holds `bytes`. A crash mid-write can
    /// leave partial content, and a failed write after the truncation says so.
    pub(super) fn rewrite(self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.rewrite_with(path, bytes, |_| Ok(()))
    }

    fn rewrite_with(
        mut self,
        path: &Path,
        bytes: &[u8],
        mut checkpoint: impl FnMut(Phase) -> io::Result<()>,
    ) -> io::Result<()> {
        #[cfg(unix)]
        self.refuse_clearable_attributes()?;
        if !bytes_match(&mut self.file, &self.content)? {
            return Err(stale_content());
        }
        self.file.set_len(0)?;
        self.file
            .rewind()
            .and_then(|()| self.file.write_all(bytes))
            .and_then(|()| checkpoint(Phase::Written))
            .and_then(|()| self.file.sync_all())
            .map_err(|error| left_partial_file(&error))?;
        checkpoint(Phase::Synced)?;
        self.verify(path, bytes)
    }

    /// Refuses set-ID permissions, and a capability on Linux, which a write can clear.
    #[cfg(unix)]
    fn refuse_clearable_attributes(&self) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt as _;

        validate_mode(self.file.metadata()?.mode() & 0o7777)?;
        #[cfg(target_os = "linux")]
        if has_capability(&self.file)? {
            return Err(io::Error::other(
                "refusing to write a file with a capability set",
            ));
        }
        Ok(())
    }

    /// Checks that `path` still names the held file and holds `expected`.
    fn verify(&self, path: &Path, expected: &[u8]) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.read(true);
        let mut current = open_no_follow(path, options)?;
        if identity(&current)? != identity(&self.file)? {
            return Err(io::Error::other(
                "the file was replaced while preparing the write",
            ));
        }
        if !bytes_match(&mut current, expected)? {
            return Err(stale_content());
        }
        Ok(())
    }
}

fn stale_content() -> io::Error {
    io::Error::other("the file changed while preparing the write. Read it again.")
}

#[cfg(unix)]
fn validate_mode(mode: u32) -> io::Result<()> {
    if mode & 0o6000 != 0 {
        Err(io::Error::other(
            "refusing to write a file with set-ID permissions",
        ))
    } else {
        Ok(())
    }
}

fn bytes_match(file: &mut File, expected: &[u8]) -> io::Result<bool> {
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes == expected)
}

#[cfg(unix)]
fn identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    crate::util::fs::file_id(file)
}

/// Whether `file` carries `security.capability`, which the kernel clears on a write.
#[cfg(target_os = "linux")]
fn has_capability(file: &File) -> io::Result<bool> {
    use xattr::FileExt as _;

    match file.get_xattr("security.capability") {
        Ok(value) => Ok(value.is_some()),
        Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::util::fs::testing::symlink_file;

    fn entries(path: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    fn identity_of(path: &Path) -> String {
        format!("{:?}", identity(&File::open(path).unwrap()).unwrap())
    }

    /// Rewrites the existing file at `path` with `bytes` through a fresh [`Checked`] open.
    fn rewrite(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let checked = Checked::open(path).map_err(|error| match error {
            FileReadError::Io(error) => error,
            error => io::Error::other(error),
        })?;
        checked.rewrite(path, bytes)
    }

    mod create_and_rewrite {
        use super::*;

        #[test]
        fn replacement_publishes_complete_bytes() {
            for (existing, bytes) in [
                (false, b"new".as_slice()),
                (true, b"".as_slice()),
                (true, b"replacement".as_slice()),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                let before = existing.then(|| {
                    fs::write(&path, b"original").unwrap();
                    identity_of(&path)
                });

                let result = if existing {
                    rewrite(&path, bytes)
                } else {
                    create(&path, bytes)
                };

                result.unwrap();
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
                if let Some(before) = before {
                    assert_eq!(identity_of(&path), before, "the write keeps the inode");
                }
            }
        }

        #[test]
        fn output_can_exceed_the_input_read_limit() {
            let bytes = vec![b'x'; usize::try_from(MAX_TRACKED_FILE_SIZE).unwrap() + 1];
            for existing in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                if existing {
                    fs::write(&path, b"original").unwrap();
                }

                let result = if existing {
                    rewrite(&path, &bytes)
                } else {
                    create(&path, &bytes)
                };

                result.unwrap();
                assert_eq!(fs::read(&path).unwrap(), bytes, "existing={existing}");
            }
        }

        #[test]
        fn writes_through_every_hard_link_in_place() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            let alias = dir.path().join("alias");
            fs::write(&path, b"original").unwrap();
            fs::hard_link(&path, &alias).unwrap();
            let before = identity_of(&path);

            rewrite(&path, b"new").unwrap();

            assert_eq!(identity_of(&path), before);
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(fs::read(&alias).unwrap(), b"new");
        }

        #[test]
        fn create_new_refuses_existing_entries_and_dangling_links() {
            let identity = |path: &Path| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;

                    let metadata = fs::symlink_metadata(path).unwrap();
                    (metadata.dev(), metadata.ino())
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt as _;

                    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

                    let file = fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                        .open(path)
                        .unwrap();
                    crate::util::fs::file_id(&file).unwrap()
                }
            };
            let dir = tempfile::tempdir().unwrap();
            let existing = dir.path().join("existing.txt");
            fs::write(&existing, "original").unwrap();
            let target = dir.path().join("target.txt");
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);
            let before_entries = entries(dir.path());

            for path in [&existing, &link] {
                let before = identity(path);

                let error = create(path, b"x").unwrap_err();

                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                assert_eq!(identity(path), before);
                assert_eq!(fs::read(&existing).unwrap(), b"original");
                assert_eq!(entries(dir.path()), before_entries);
                assert_eq!(fs::read_link(&link).unwrap(), target);
                assert!(!target.exists(), "the link target must not be created");
            }
        }

        #[test]
        fn overwrite_refuses_a_symlink() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.txt");
            fs::write(&target, "original").unwrap();
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);

            let err = rewrite(&link, b"x").unwrap_err();
            #[cfg(unix)]
            assert_eq!(err.raw_os_error(), Some(nix::errno::Errno::ELOOP as i32));
            #[cfg(windows)]
            assert_eq!(err.to_string(), "the final path component is a symlink");
            assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        }

        #[test]
        fn refuses_directory_suffixed_new_paths() {
            for name in ["missing/", "missing/."] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join(name);

                let error = create(&path, b"new").unwrap_err();

                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name}");
                assert_eq!(error.to_string(), "a new file path has a directory suffix");
                assert_eq!(entries(dir.path()), [] as [std::ffi::OsString; 0], "{name}");
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn keeps_the_mode_and_extended_attributes() {
            use std::os::unix::fs::PermissionsExt as _;
            use xattr::FileExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
            let file = File::open(&path).unwrap();
            let attributes_supported = file.set_xattr("user.ox.value", b"\x00\xff").is_ok();
            let before = identity_of(&path);

            rewrite(&path, b"new").unwrap();

            let current = File::open(&path).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(identity_of(&path), before);
            assert_eq!(
                current.metadata().unwrap().permissions().mode() & 0o7777,
                0o751
            );
            if attributes_supported {
                assert_eq!(
                    current.get_xattr("user.ox.value").unwrap(),
                    Some(b"\x00\xff".to_vec())
                );
            }
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn keeps_a_named_user_acl() {
            use xattr::FileExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            // The kernel's version 2 ACL encoding: owner rw, user 65534 r, group r, mask r, other
            // none, each as a little-endian (tag, permissions, id) entry.
            let mut acl = 2_u32.to_le_bytes().to_vec();
            for (tag, permissions, id) in [
                (1_u16, 6_u16, u32::MAX),
                (2, 4, 65534),
                (4, 4, u32::MAX),
                (16, 4, u32::MAX),
                (32, 0, u32::MAX),
            ] {
                acl.extend(tag.to_le_bytes());
                acl.extend(permissions.to_le_bytes());
                acl.extend(id.to_le_bytes());
            }
            let file = File::open(&path).unwrap();
            let acl_supported = match file.set_xattr("system.posix_acl_access", &acl) {
                Ok(()) => true,
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EOPNOTSUPP));
                    false
                }
            };
            let before = identity_of(&path);

            rewrite(&path, b"new").unwrap();

            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(identity_of(&path), before);
            if acl_supported {
                assert_eq!(
                    File::open(&path)
                        .unwrap()
                        .get_xattr("system.posix_acl_access")
                        .unwrap(),
                    Some(acl)
                );
            }
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[cfg(unix)]
        #[test]
        fn writes_without_parent_write_access() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            let before = identity_of(&path);
            let denied = crate::util::fs::testing::deny_subdir_creation(dir.path());

            rewrite(&path, b"new").unwrap();
            drop(denied);

            assert_eq!(identity_of(&path), before);
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                0o640
            );
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[cfg(unix)]
        #[test]
        fn new_file_uses_the_parents_normal_creation_mode() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let ordinary = dir.path().join("ordinary");
            let created = dir.path().join("created");
            fs::write(&ordinary, b"").unwrap();

            create(&created, b"content").unwrap();

            assert_eq!(
                fs::metadata(&created).unwrap().permissions().mode(),
                fs::metadata(&ordinary).unwrap().permissions().mode()
            );
        }
    }

    mod rewrite_with {
        use super::*;

        #[test]
        fn a_failed_write_says_a_partial_file_remains() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();

            let checked = Checked::open(&path).unwrap();

            let error = checked
                .rewrite_with(&path, b"new", |phase| {
                    assert_eq!(phase, Phase::Written);
                    Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"))
                })
                .unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::StorageFull);
            assert_eq!(
                error.to_string(),
                "disk full. A partial file remains. Read it before writing again."
            );
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[test]
        fn refuses_concurrent_content_and_identity_changes() {
            for replace in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                fs::write(&path, b"original").unwrap();
                let mut visited = Vec::new();

                let checked = Checked::open(&path).unwrap();

                let result = checked.rewrite_with(&path, b"ours", |phase| {
                    visited.push(phase);
                    if phase == Phase::Synced {
                        if replace {
                            let other = dir.path().join("other");
                            fs::write(&other, b"original")?;
                            fs::rename(other, &path)?;
                        } else {
                            fs::write(&path, b"theirs")?;
                        }
                    }
                    Ok(())
                });

                assert_eq!(
                    result.unwrap_err().to_string(),
                    if replace {
                        "the file was replaced while preparing the write"
                    } else {
                        "the file changed while preparing the write. Read it again."
                    }
                );
                assert!(visited.contains(&Phase::Synced), "{visited:?}");
                assert_eq!(
                    fs::read(&path).unwrap(),
                    if replace {
                        b"original".as_slice()
                    } else {
                        b"theirs"
                    }
                );
                assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
            }
        }
    }

    #[cfg(unix)]
    mod checked {
        use super::*;

        #[test]
        fn open_refuses_a_fifo_without_blocking() {
            let dir = tempfile::tempdir().unwrap();
            let fifo = dir.path().join("pipe");
            nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();

            std::thread::spawn(move || sender.send(Checked::open(&fifo).map(|_| ())));
            let result = receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the open must not block");

            assert!(
                matches!(result, Err(FileReadError::NonRegular)),
                "{result:?}"
            );
        }
    }

    mod left_partial_file {
        use super::*;

        #[test]
        fn tells_the_model_to_read_the_partial_file_and_keeps_the_kind() {
            let error = left_partial_file(&io::Error::new(io::ErrorKind::StorageFull, "disk full"));

            assert_eq!(error.kind(), io::ErrorKind::StorageFull);
            assert_eq!(
                error.to_string(),
                "disk full. A partial file remains. Read it before writing again."
            );
        }
    }

    #[cfg(unix)]
    mod validate_mode {
        use super::*;

        #[test]
        fn accepts_ordinary_permissions_and_refuses_either_set_id_bit() {
            for mode in [0o600, 0o751, 0o1777] {
                validate_mode(mode).unwrap();
            }
            for mode in [0o4755, 0o2755, 0o6755] {
                let error = validate_mode(mode).unwrap_err();

                assert!(error.to_string().contains("set-ID"), "mode={mode:o}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    mod has_capability {
        use super::*;

        #[test]
        fn reads_a_file_without_extended_attribute_support_as_having_none() {
            let file = File::open("/proc/self/status").unwrap();

            assert!(!has_capability(&file).unwrap());
        }
    }
}
