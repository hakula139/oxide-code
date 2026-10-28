//! Publication of workspace file changes.

use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::Path;

use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::util::fs::{open_no_follow, validate_regular_file};
use crate::util::path::requires_directory;

pub(super) fn write(path: &Path, bytes: &[u8], expected: Option<&[u8]>) -> io::Result<()> {
    write_with(path, bytes, expected, |_| Ok(()))
}

/// The points of an in-place write where a test can act on the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The original is checked and still unchanged.
    Prepared,
    /// The new bytes are written and synced, before the final check.
    Synced,
}

fn write_with(
    path: &Path,
    bytes: &[u8],
    expected: Option<&[u8]>,
    checkpoint: impl FnMut(Phase) -> io::Result<()>,
) -> io::Result<()> {
    match expected {
        Some(expected) => write_in_place(path, bytes, expected, checkpoint),
        None => create(path, bytes),
    }
}

/// Creates `path` with an exclusive open, which never replaces an existing entry, including a
/// dangling symlink, and leaves the new file's mode to the umask and any default ACL. A failed
/// write leaves the partial file at `path`.
fn create(path: &Path, bytes: &[u8]) -> io::Result<()> {
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

/// Rewrites `path` through the handle checked against `expected`, which keeps the inode with its
/// metadata and every hard link. A crash or a full disk mid-write can leave partial content.
fn write_in_place(
    path: &Path,
    bytes: &[u8],
    expected: &[u8],
    mut checkpoint: impl FnMut(Phase) -> io::Result<()>,
) -> io::Result<()> {
    let mut original = Original::open(path, expected)?;
    original.verify(path, expected)?;
    checkpoint(Phase::Prepared)?;
    original.file.set_len(0)?;
    original.file.rewind()?;
    original.file.write_all(bytes)?;
    original.file.sync_all()?;
    checkpoint(Phase::Synced)?;
    original.verify(path, bytes)
}

struct Original {
    file: File,
}

impl Original {
    fn open(path: &Path, expected: &[u8]) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let mut file = open_no_follow(path, options)?;
        let metadata = file.metadata()?;
        validate_regular_file(&metadata, MAX_TRACKED_FILE_SIZE).map_err(io::Error::other)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            validate_mode(metadata.mode() & 0o7777)?;
        }
        #[cfg(target_os = "linux")]
        if has_capability(&file)? {
            return Err(io::Error::other(
                "refusing to write a file with a capability set",
            ));
        }
        if !bytes_match(&mut file, expected)? {
            return Err(stale_content());
        }
        Ok(Self { file })
    }

    fn verify(&self, path: &Path, expected: &[u8]) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
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

    mod write {
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
                if existing {
                    fs::write(&path, b"original").unwrap();
                }

                let result = write(&path, bytes, existing.then_some(b"original".as_slice()));

                result.unwrap();
                assert_eq!(fs::read(&path).unwrap(), bytes);
                assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
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

                let result = write(&path, &bytes, existing.then_some(b"original".as_slice()));

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

            write(&path, b"new", Some(b"original")).unwrap();

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

                let error = write(path, b"x", None).unwrap_err();

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

            let err = write(&link, b"x", Some(b"original")).unwrap_err();
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

                let error = write(&path, b"new", None).unwrap_err();

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

            write(&path, b"new", Some(b"original")).unwrap();

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

        /// A POSIX ACL whose owner entry grants `owner` permissions.
        #[cfg(target_os = "linux")]
        fn acl(owner: u16) -> Vec<u8> {
            let mut bytes = 2_u32.to_le_bytes().to_vec();
            for (tag, permissions, id) in [
                (1_u16, owner, u32::MAX),
                (2, 4_u16, 65534),
                (4, 4, u32::MAX),
                (16, 4, u32::MAX),
                (32, 0, u32::MAX),
            ] {
                bytes.extend(tag.to_le_bytes());
                bytes.extend(permissions.to_le_bytes());
                bytes.extend(id.to_le_bytes());
            }
            bytes
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn keeps_a_named_user_acl() {
            use xattr::FileExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let acl = acl(6);
            let file = File::open(&path).unwrap();
            let acl_supported = match file.set_xattr("system.posix_acl_access", &acl) {
                Ok(()) => true,
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EOPNOTSUPP));
                    false
                }
            };
            let before = identity_of(&path);

            write(&path, b"new", Some(b"original")).unwrap();

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

            write(&path, b"new", Some(b"original")).unwrap();
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

            write(&created, b"content", None).unwrap();

            assert_eq!(
                fs::metadata(&created).unwrap().permissions().mode(),
                fs::metadata(&ordinary).unwrap().permissions().mode()
            );
        }
    }

    mod write_with {
        use super::*;

        #[test]
        fn a_failure_before_writing_leaves_the_original_unchanged() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let before = entries(dir.path());

            let error = write_with(&path, b"replacement", Some(b"original"), |phase| {
                if phase == Phase::Prepared {
                    return Err(io::Error::other("injected preparation failure"));
                }
                Ok(())
            })
            .unwrap_err();

            assert!(error.to_string().contains("injected"), "{error}");
            assert_eq!(entries(dir.path()), before);
            assert_eq!(fs::read(&path).unwrap(), b"original");
        }

        #[test]
        fn refuses_stale_expected_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();

            let error = write(&path, b"new", Some(b"stale")).unwrap_err();

            assert_eq!(
                error.to_string(),
                "the file changed while preparing the write. Read it again."
            );
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[test]
        fn refuses_concurrent_content_and_identity_changes() {
            for replace in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                fs::write(&path, b"original").unwrap();
                let mut visited = Vec::new();

                let result = write_with(&path, b"ours", Some(b"original"), |phase| {
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
}
