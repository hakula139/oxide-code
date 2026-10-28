//! Preparation and publication of workspace file changes.

#[cfg(target_os = "linux")]
mod metadata;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::Path;

use tempfile::NamedTempFile;

use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::util::fs::{open_no_follow, validate_regular_file};
use crate::util::path::requires_directory;

pub(super) fn write(path: &Path, bytes: &[u8], expected: Option<&[u8]>) -> io::Result<()> {
    write_with(path, bytes, expected, |_, _| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Written,
    Synced,
    Validated,
}

fn write_with(
    path: &Path,
    bytes: &[u8],
    expected: Option<&[u8]>,
    mut checkpoint: impl FnMut(Phase, &File) -> io::Result<()>,
) -> io::Result<()> {
    if expected.is_none() && requires_directory(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a new file path has a directory suffix",
        ));
    }
    let Some(expected) = expected else {
        return create(path, bytes, &mut checkpoint);
    };
    let mut original = Original::open(path, expected)?;
    #[cfg(target_os = "linux")]
    if replace_atomically(path, bytes, expected, &original, &mut checkpoint)? {
        return Ok(());
    }
    write_in_place(path, bytes, expected, &mut original, &mut checkpoint)
}

/// Creates `path` from a staged copy, so an existing entry is never replaced and a partial write
/// never reaches the destination name.
fn create(
    path: &Path,
    bytes: &[u8],
    checkpoint: &mut impl FnMut(Phase, &File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .expect("prepare resolves an absolute file path");
    let mut staged = staged_file(parent)?;
    checkpoint(Phase::Prepared, staged.as_file())?;
    staged.as_file_mut().write_all(bytes)?;
    checkpoint(Phase::Written, staged.as_file())?;
    #[cfg(unix)]
    staged.as_file().set_permissions(creation_mode(parent)?)?;
    staged.as_file().sync_all()?;
    checkpoint(Phase::Synced, staged.as_file())?;
    checkpoint(Phase::Validated, staged.as_file())?;
    fs::hard_link(staged.path(), path)
}

/// Replaces `path` with a staged copy when a rename preserves every piece of metadata, which is
/// when the staged file with the original's mode alone captures the same snapshot.
#[cfg(target_os = "linux")]
fn replace_atomically(
    path: &Path,
    bytes: &[u8],
    expected: &[u8],
    original: &Original,
    checkpoint: &mut impl FnMut(Phase, &File) -> io::Result<()>,
) -> io::Result<bool> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let Some(snapshot) = &original.snapshot else {
        return Ok(false);
    };
    if original.file.metadata()?.nlink() != 1 {
        return Ok(false);
    }
    let parent = path
        .parent()
        .expect("prepare resolves an absolute file path");
    let mut staged = match staged_file(parent) {
        Ok(staged) => staged,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => return Err(error),
    };
    checkpoint(Phase::Prepared, staged.as_file())?;
    staged.as_file_mut().write_all(bytes)?;
    checkpoint(Phase::Written, staged.as_file())?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(snapshot.mode))?;
    if metadata::Snapshot::capture(staged.as_file())? != *snapshot {
        return Ok(false);
    }
    staged.as_file().sync_all()?;
    checkpoint(Phase::Synced, staged.as_file())?;
    if !bytes_match(staged.as_file_mut(), bytes)? {
        return Err(io::Error::other(
            "the prepared file changed before publication",
        ));
    }
    checkpoint(Phase::Validated, staged.as_file())?;
    verify_name(staged.as_file(), staged.path())?;
    original.verify(path, expected)?;
    fs::rename(staged.path(), path).map(|()| true)
}

/// Writes through the original's held handle, which preserves every piece of metadata and every
/// hard link without recreating any of it.
fn write_in_place(
    path: &Path,
    bytes: &[u8],
    expected: &[u8],
    original: &mut Original,
    checkpoint: &mut impl FnMut(Phase, &File) -> io::Result<()>,
) -> io::Result<()> {
    original.verify(path, expected)?;
    checkpoint(Phase::Prepared, &original.file)?;
    original.file.set_len(0)?;
    original.file.rewind()?;
    original.file.write_all(bytes)?;
    checkpoint(Phase::Written, &original.file)?;
    original.file.sync_all()?;
    checkpoint(Phase::Synced, &original.file)?;
    original.verify(path, bytes)?;
    checkpoint(Phase::Validated, &original.file)
}

fn staged_file(parent: &Path) -> io::Result<NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".ox-write.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        builder.permissions(fs::Permissions::from_mode(0o600));
    }
    builder.tempfile_in(parent)
}

/// The mode a new file in `parent` receives from the umask, read from a temporary probe.
#[cfg(unix)]
fn creation_mode(parent: &Path) -> io::Result<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt as _;

    let probe = tempfile::Builder::new()
        .prefix(".ox-mode.")
        .permissions(fs::Permissions::from_mode(0o666))
        .tempfile_in(parent)?;
    Ok(probe.as_file().metadata()?.permissions())
}

struct Original {
    file: File,
    /// `None` when a staged copy cannot reproduce the file's access metadata.
    #[cfg(target_os = "linux")]
    snapshot: Option<metadata::Snapshot>,
}

impl Original {
    fn open(path: &Path, expected: &[u8]) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let mut file = open_no_follow(path, options)?;
        let metadata = file.metadata()?;
        validate_regular_file(&metadata, MAX_TRACKED_FILE_SIZE).map_err(io::Error::other)?;
        #[cfg(unix)]
        validate_mode(std::os::unix::fs::MetadataExt::mode(&metadata) & 0o7777)?;
        if !bytes_match(&mut file, expected)? {
            return Err(stale_content());
        }
        #[cfg(target_os = "linux")]
        let snapshot = metadata::reproducible(&file)?
            .then(|| metadata::Snapshot::capture(&file))
            .transpose()?;
        Ok(Self {
            file,
            #[cfg(target_os = "linux")]
            snapshot,
        })
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

#[cfg(target_os = "linux")]
fn verify_name(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let named = fs::symlink_metadata(path)?;
    let held = file.metadata()?;
    if !named.is_file() || (named.dev(), named.ino()) != (held.dev(), held.ino()) {
        return Err(io::Error::other(
            "the prepared file's pathname changed before publication",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tool::local_testing::WorkspaceWrite;

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

    mod write_with {
        use super::*;

        #[test]
        fn preparation_failures_leave_the_original_unchanged() {
            // Only an atomic publication can stop after writing without the original changing.
            let phases: &[Phase] = if cfg!(target_os = "linux") {
                &[
                    Phase::Prepared,
                    Phase::Written,
                    Phase::Synced,
                    Phase::Validated,
                ]
            } else {
                &[Phase::Prepared]
            };
            for &phase in phases {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                fs::write(&path, b"original").unwrap();
                let before = entries(dir.path());
                let mut visited = Vec::new();

                let error = write_with(&path, b"replacement", Some(b"original"), |at, _| {
                    visited.push(at);
                    if at == phase {
                        return Err(io::Error::other("injected preparation failure"));
                    }
                    Ok(())
                })
                .unwrap_err();

                assert!(error.to_string().contains("injected"), "{phase:?}");
                assert_eq!(visited.last(), Some(&phase));
                assert_eq!(entries(dir.path()), before, "{phase:?}");
                assert_eq!(fs::read(&path).unwrap(), b"original", "{phase:?}");
            }
        }

        #[test]
        fn refuses_directory_suffixed_new_paths_before_preparation() {
            for name in ["missing/", "missing/."] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join(name);
                let mut visited = Vec::new();

                let error = write_with(&path, b"new", None, |phase, _| {
                    visited.push(phase);
                    Ok(())
                })
                .unwrap_err();

                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{name}");
                assert_eq!(error.to_string(), "a new file path has a directory suffix");
                assert_eq!(visited, [], "{name}");
                assert_eq!(entries(dir.path()), [] as [std::ffi::OsString; 0], "{name}");
            }
        }

        #[test]
        fn refuses_stale_expected_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let attempt = WorkspaceWrite::new(&path);

            let error = write(&path, b"new", Some(b"stale")).unwrap_err();

            assert_eq!(
                error.to_string(),
                "the file changed while preparing the write. Read it again."
            );
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
            attempt.assert_unchanged();
        }

        #[test]
        fn refuses_concurrent_content_and_identity_changes() {
            for replace in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                fs::write(&path, b"original").unwrap();
                let mut visited = Vec::new();

                let result = write_with(&path, b"ours", Some(b"original"), |phase, _| {
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

        #[test]
        fn concurrent_create_is_never_replaced() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            let mut visited = Vec::new();

            let result = write_with(&path, b"ours", None, |phase, _| {
                visited.push(phase);
                if phase == Phase::Validated {
                    fs::write(&path, b"theirs")?;
                }
                Ok(())
            });

            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(visited.last(), Some(&Phase::Validated));
            assert_eq!(fs::read(&path).unwrap(), b"theirs");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn refuses_substituted_stage_names() {
            use crate::util::fs::testing::symlink_file;

            for replacement in ["content", "symlink"] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                let victim = dir.path().join("victim");
                fs::write(&path, b"original").unwrap();
                fs::write(&victim, b"victim").unwrap();
                let destination = path.clone();
                let target = victim.clone();

                let error = write_with(&destination, b"ours", Some(b"original"), |phase, _| {
                    if phase == Phase::Validated {
                        let staged = fs::read_dir(destination.parent().unwrap())?
                            .map(Result::unwrap)
                            .map(|entry| entry.path())
                            .find(|candidate| {
                                candidate
                                    .file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .starts_with(".ox-write.")
                            })
                            .expect("the call staged a file");
                        fs::remove_file(&staged)?;
                        if replacement == "content" {
                            fs::write(&staged, b"substituted")?;
                        } else {
                            symlink_file(&target, &staged);
                        }
                    }
                    Ok(())
                })
                .unwrap_err();

                assert!(
                    error.to_string().contains("pathname changed"),
                    "{replacement}: {error}"
                );
                assert_eq!(fs::read(&path).unwrap(), b"original");
                assert_eq!(fs::read(&victim).unwrap(), b"victim");
                assert_eq!(
                    entries(dir.path()),
                    [
                        std::ffi::OsString::from("target"),
                        std::ffi::OsString::from("victim"),
                    ],
                    "{replacement}"
                );
            }
        }

        #[cfg(unix)]
        #[test]
        fn refuses_set_id_files() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).unwrap();
            let attempt = WorkspaceWrite::new(&path);

            let error = write(&path, b"new", Some(b"original")).unwrap_err();

            assert!(error.to_string().contains("set-ID"), "{error}");
            assert_eq!(fs::read(&path).unwrap(), b"original");
            attempt.assert_unchanged();
        }
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
        fn ordinary_replacements_keep_or_replace_the_inode_by_platform() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let before = identity_of(&path);

            write(&path, b"new", Some(b"original")).unwrap();

            assert_eq!(fs::read(&path).unwrap(), b"new");
            #[cfg(target_os = "linux")]
            assert_ne!(identity_of(&path), before);
            #[cfg(not(target_os = "linux"))]
            assert_eq!(identity_of(&path), before);
        }

        #[cfg(unix)]
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

        #[cfg(target_os = "linux")]
        #[test]
        fn writes_in_place_when_attributes_cannot_be_reproduced() {
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
            assert_eq!(
                current.metadata().unwrap().permissions().mode() & 0o7777,
                0o751
            );
            if attributes_supported {
                assert_eq!(identity_of(&path), before);
                assert_eq!(
                    current.get_xattr("user.ox.value").unwrap(),
                    Some(b"\x00\xff".to_vec())
                );
            }
        }

        #[cfg(unix)]
        #[test]
        fn falls_back_to_in_place_without_parent_write_access() {
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
            let staged = dir.path().join("staged");
            fs::write(&ordinary, b"").unwrap();

            write(&staged, b"content", None).unwrap();

            assert_eq!(
                fs::metadata(&staged).unwrap().permissions().mode(),
                fs::metadata(&ordinary).unwrap().permissions().mode()
            );
        }
    }

    #[cfg(target_os = "linux")]
    mod replace_atomically {
        use super::*;

        #[test]
        fn metadata_matching_allows_an_atomic_rename() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let before = identity_of(&path);
            let mut visited = Vec::new();

            write_with(&path, b"new", Some(b"original"), |phase, _| {
                visited.push(phase);
                Ok(())
            })
            .unwrap();

            assert_eq!(
                visited,
                [
                    Phase::Prepared,
                    Phase::Written,
                    Phase::Synced,
                    Phase::Validated,
                ]
            );
            assert_ne!(identity_of(&path), before);
        }
    }
}
