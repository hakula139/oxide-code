//! Same-filesystem preparation and publication of workspace file changes.

mod metadata;
mod native;

#[cfg(any(unix, test))]
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::Path;

use tempfile::{NamedTempFile, TempDir, TempPath};

use self::metadata::Snapshot;
use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::util::fs::{open_no_follow, validate_regular_file};

pub(super) fn write(path: &Path, bytes: &[u8], expected: Option<&[u8]>) -> io::Result<()> {
    write_with(path, bytes, expected, |_, _| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    Written,
    Metadata,
    Synced,
    Validated,
}

fn write_with(
    path: &Path,
    bytes: &[u8],
    expected: Option<&[u8]>,
    mut checkpoint: impl FnMut(Phase, &File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the file has no parent"))?;
    let original = expected
        .map(|bytes| Original::open(path, bytes))
        .transpose()?;
    let probe = original
        .is_none()
        .then(|| TemporaryFile::probe(parent))
        .transpose()?;
    let snapshot = match &original {
        Some(original) => original.snapshot.clone(),
        None => Snapshot::capture(&probe.as_ref().expect("new files have a probe").file)?,
    };
    let mut staged = TemporaryFile::staged(parent)?;
    #[cfg(any(target_os = "macos", windows))]
    if let Some(original) = &original {
        native::copy(&original.file, &staged.file)?;
    }
    let file = &mut staged.file;
    checkpoint(Phase::Prepared, file)?;
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(bytes)?;
    checkpoint(Phase::Written, file)?;

    snapshot.apply(file)?;
    checkpoint(Phase::Metadata, file)?;
    file.sync_all()?;
    checkpoint(Phase::Synced, file)?;

    if let Some(original) = &original {
        original.verify(path, expected.expect("overwrites have expected bytes"))?;
    }
    checkpoint(Phase::Validated, file)?;
    if !bytes_match(file, bytes)? {
        return Err(io::Error::other(
            "the prepared file changed before publication",
        ));
    }
    snapshot.verify(file)?;

    #[cfg(unix)]
    {
        staged.verify_name()?;
        if original.is_some() {
            fs::rename(&staged.path, path)
        } else {
            native::publish_new(&staged.path, path)
        }
    }
    #[cfg(windows)]
    {
        native::publish(&staged.file, path, original.is_some())?;
        staged.published = true;
        Ok(())
    }
}

struct Original {
    file: File,
    snapshot: Snapshot,
}

impl Original {
    fn open(path: &Path, expected: &[u8]) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let mut file = open_no_follow(path, options)?;
        check_file(&mut file, expected)?;
        let snapshot = Snapshot::capture(&file)?;
        Ok(Self { file, snapshot })
    }

    fn verify(&self, path: &Path, expected: &[u8]) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let mut current = open_no_follow(path, options)?;
        if native::identity(&current)? != native::identity(&self.file)? {
            return Err(io::Error::other(
                "the file was replaced while preparing the write",
            ));
        }
        check_file(&mut current, expected)?;
        self.snapshot.verify(&current)
    }
}

fn check_file(file: &mut File, expected: &[u8]) -> io::Result<()> {
    let metadata = file.metadata()?;
    validate_regular_file(&metadata, MAX_TRACKED_FILE_SIZE).map_err(io::Error::other)?;
    if native::link_count(file)? != 1 {
        return Err(io::Error::other(
            "refusing to replace a file with multiple hard links",
        ));
    }
    if !bytes_match(file, expected)? {
        return Err(io::Error::other(
            "the file changed while preparing the write. Read it again.",
        ));
    }
    Ok(())
}

fn bytes_match(file: &mut File, expected: &[u8]) -> io::Result<bool> {
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes == expected)
}

struct TemporaryFile {
    file: File,
    path: TempPath,
    _directory: Option<TempDir>,
    #[cfg(windows)]
    published: bool,
}

impl TemporaryFile {
    fn staged(parent: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let directory = tempfile::Builder::new()
                .prefix(".ox-write.")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in(parent)?;
            #[cfg(target_os = "macos")]
            native::make_private(&File::open(directory.path())?)?;
            let file = NamedTempFile::new_in(directory.path())?;
            Ok(Self::from_named(file, Some(directory)))
        }
        #[cfg(windows)]
        {
            let file = tempfile::Builder::new()
                .prefix(".ox-write.")
                .make_in(parent, native::create_private)?;
            Ok(Self::from_named(file, None))
        }
    }

    fn probe(parent: &Path) -> io::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(".ox-mode.");
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::PermissionsExt as _;

            builder
                .permissions(fs::Permissions::from_mode(0o666))
                .tempfile_in(parent)?
        };
        #[cfg(windows)]
        let file = builder.make_in(parent, |path| {
            use std::os::windows::fs::OpenOptionsExt as _;

            use windows_sys::Win32::Storage::FileSystem;

            // Creation grants the requested DELETE access even when the inherited DACL denies it.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .access_mode(
                    FileSystem::FILE_GENERIC_READ
                        | FileSystem::FILE_GENERIC_WRITE
                        | FileSystem::DELETE,
                )
                .share_mode(FileSystem::FILE_SHARE_READ | FileSystem::FILE_SHARE_WRITE)
                .open(path)
        })?;
        Ok(Self::from_named(file, None))
    }

    fn from_named(file: NamedTempFile, directory: Option<TempDir>) -> Self {
        let (file, path) = file.into_parts();
        Self {
            file,
            path,
            _directory: directory,
            #[cfg(windows)]
            published: false,
        }
    }

    #[cfg(unix)]
    fn verify_name(&self) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt as _;

        let named = fs::symlink_metadata(&self.path)?;
        let held = self.file.metadata()?;
        if !named.is_file() || (named.dev(), named.ino()) != (held.dev(), held.ino()) {
            return Err(io::Error::other(
                "the prepared file's pathname changed before publication",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.published {
            _ = native::discard(&self.file);
        }
        self.path.disable_cleanup(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tool::local_testing::WorkspaceWrite;

    mod write {
        use super::*;

        fn entries(path: &Path) -> Vec<std::ffi::OsString> {
            let mut names: Vec<_> = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        }

        #[test]
        fn preparation_failures_leave_original_and_new_paths_unchanged() {
            for phase in [
                Phase::Prepared,
                Phase::Written,
                Phase::Metadata,
                Phase::Synced,
                Phase::Validated,
            ] {
                for existing in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("target");
                    let original = if existing {
                        fs::write(&path, b"original").unwrap();
                        Some(File::open(&path).unwrap())
                    } else {
                        None
                    };
                    let before = entries(dir.path());
                    let attempt = WorkspaceWrite::new(&path);
                    let mut visited = Vec::new();

                    let error = write_with(
                        &path,
                        b"replacement",
                        existing.then_some(b"original".as_slice()),
                        |at, file| {
                            visited.push(at);
                            if at == phase {
                                let mut file = file;
                                file.set_len(0)?;
                                file.write_all(b"partial")?;
                                return Err(io::Error::other("injected preparation failure"));
                            }
                            Ok(())
                        },
                    )
                    .unwrap_err();

                    if attempt.acl_available {
                        assert!(error.to_string().contains("injected"), "{phase:?}");
                        assert_eq!(visited.last(), Some(&phase));
                    } else {
                        attempt.assert_native_refusal(&error);
                        assert_eq!(visited, []);
                    }
                    assert_eq!(entries(dir.path()), before, "{phase:?}");
                    if let Some(original) = original {
                        assert_eq!(fs::read(&path).unwrap(), b"original", "{phase:?}");
                        assert_eq!(
                            native::identity(&File::open(&path).unwrap()).unwrap(),
                            native::identity(&original).unwrap(),
                            "{phase:?}"
                        );
                    } else {
                        assert!(!path.exists(), "{phase:?}");
                    }
                }
            }
        }

        #[test]
        fn replacement_changes_identity_and_keeps_open_original_content() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let mut original = File::open(&path).unwrap();
            let snapshot = Snapshot::capture(&original);
            let attempt = WorkspaceWrite::new(&path);

            let result = write(&path, b"new", Some(b"original"));

            let mut bytes = Vec::new();
            original.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"original");
            if attempt.acl_available {
                result.unwrap();
                let replacement = File::open(&path).unwrap();
                assert_ne!(
                    native::identity(&replacement).unwrap(),
                    native::identity(&original).unwrap()
                );
                snapshot.unwrap().verify(&replacement).unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"new");
            } else {
                attempt.assert_native_refusal(&snapshot.unwrap_err());
                attempt.assert_native_refusal(&result.unwrap_err());
            }
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[test]
        fn new_file_and_empty_replacement_publish_complete_bytes() {
            for (existing, bytes) in [(false, b"new".as_slice()), (true, b"")] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                if existing {
                    fs::write(&path, b"original").unwrap();
                }
                let attempt = WorkspaceWrite::new(&path);

                let result = write(&path, bytes, existing.then_some(b"original".as_slice()));

                if attempt.acl_available {
                    result.unwrap();
                    assert_eq!(fs::read(&path).unwrap(), bytes);
                    assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
                } else {
                    attempt.assert_native_refusal(&result.unwrap_err());
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
                let attempt = WorkspaceWrite::new(&path);

                let result = write(&path, &bytes, existing.then_some(b"original".as_slice()));

                if attempt.acl_available {
                    result.unwrap();
                    assert_eq!(fs::read(&path).unwrap(), bytes, "existing={existing}");
                } else {
                    attempt.assert_native_refusal(&result.unwrap_err());
                }
            }
        }

        #[test]
        fn refuses_stage_content_changes_after_original_validation() {
            for existing in [false, true] {
                for changed in [b"".as_slice(), b"OURS", b"ours-extra"] {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("target");
                    if existing {
                        fs::write(&path, b"original").unwrap();
                    }
                    let before = entries(dir.path());
                    let attempt = WorkspaceWrite::new(&path);
                    let mut visited = Vec::new();

                    let error = write_with(
                        &path,
                        b"ours",
                        existing.then_some(b"original".as_slice()),
                        |phase, mut staged| {
                            visited.push(phase);
                            if phase == Phase::Validated {
                                staged.set_len(0)?;
                                staged.rewind()?;
                                staged.write_all(changed)?;
                            }
                            Ok(())
                        },
                    )
                    .unwrap_err();

                    if attempt.acl_available {
                        assert!(
                            error.to_string().contains("prepared file changed"),
                            "{error}"
                        );
                        assert_eq!(visited.last(), Some(&Phase::Validated));
                    } else {
                        attempt.assert_native_refusal(&error);
                        assert_eq!(visited, []);
                    }
                    assert_eq!(entries(dir.path()), before);
                    if existing {
                        assert_eq!(fs::read(&path).unwrap(), b"original");
                    } else {
                        assert!(!path.exists());
                    }
                }
            }
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn refuses_substituted_stage_names_without_recording_a_write() {
            use std::sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            };

            use crate::file_tracker::testing::{seed_full_read, tracker};
            use crate::util::fs::testing::symlink_file;

            for existing in [false, true] {
                for symlink in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("target");
                    let victim = dir.path().join("victim");
                    fs::write(&victim, b"victim").unwrap();
                    let tracker = tracker();
                    let original = if existing {
                        fs::write(&path, b"original").unwrap();
                        seed_full_read(&tracker, &path);
                        Some(File::open(&path).unwrap())
                    } else {
                        None
                    };
                    let before = tracker.snapshot_all();
                    let names = entries(dir.path());
                    let attempt = WorkspaceWrite::new(&path);
                    let checkpoints = Arc::new(AtomicUsize::new(0));
                    let observed = Arc::clone(&checkpoints);
                    let destination = path.clone();
                    let target = victim.clone();

                    let error = tracker
                        .write_and_record("toolu_replace", async move {
                            write_with(
                                &destination,
                                b"ours",
                                existing.then_some(b"original".as_slice()),
                                |phase, _| {
                                    observed.fetch_add(1, Ordering::Relaxed);
                                    if phase == Phase::Validated {
                                        let directory =
                                            fs::read_dir(destination.parent().unwrap())?
                                                .map(Result::unwrap)
                                                .find(|entry| entry.file_type().unwrap().is_dir())
                                                .unwrap();
                                        let staged =
                                            fs::read_dir(directory.path())?.next().unwrap()?.path();
                                        fs::remove_file(&staged)?;
                                        if symlink {
                                            symlink_file(&target, &staged);
                                        } else {
                                            fs::write(&staged, b"substituted")?;
                                        }
                                    }
                                    Ok(())
                                },
                            )?;
                            Ok((destination, b"ours".to_vec()))
                        })
                        .await
                        .unwrap_err();

                    if attempt.acl_available {
                        assert!(error.to_string().contains("pathname changed"), "{error}");
                        assert_eq!(checkpoints.load(Ordering::Relaxed), 5);
                    } else {
                        attempt.assert_native_refusal(error.downcast_ref::<io::Error>().unwrap());
                        assert_eq!(checkpoints.load(Ordering::Relaxed), 0);
                    }
                    assert_eq!(tracker.snapshot_all(), before);
                    assert_eq!(entries(dir.path()), names);
                    assert_eq!(fs::read(&victim).unwrap(), b"victim");
                    if let Some(original) = original {
                        assert_eq!(fs::read(&path).unwrap(), b"original");
                        assert_eq!(
                            native::identity(&File::open(&path).unwrap()).unwrap(),
                            native::identity(&original).unwrap()
                        );
                    } else {
                        assert!(!path.exists());
                    }
                }
            }
        }

        #[test]
        fn refuses_stale_bytes_and_multiple_links() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();

            assert!(
                write(&path, b"new", Some(b"stale"))
                    .unwrap_err()
                    .to_string()
                    .contains("changed")
            );
            fs::hard_link(&path, dir.path().join("alias")).unwrap();
            assert!(
                write(&path, b"new", Some(b"original"))
                    .unwrap_err()
                    .to_string()
                    .contains("hard links")
            );

            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(fs::read(dir.path().join("alias")).unwrap(), b"original");
        }

        #[test]
        fn refuses_concurrent_content_and_identity_changes() {
            for replace in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                fs::write(&path, b"original").unwrap();
                let attempt = WorkspaceWrite::new(&path);
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

                if attempt.acl_available {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        if replace {
                            "the file was replaced while preparing the write"
                        } else {
                            "the file changed while preparing the write. Read it again."
                        }
                    );
                    assert_eq!(visited.last(), Some(&Phase::Synced));
                } else {
                    attempt.assert_native_refusal(&result.unwrap_err());
                    assert_eq!(visited, []);
                }
                assert_eq!(
                    fs::read(&path).unwrap(),
                    if replace || !attempt.acl_available {
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
            let attempt = WorkspaceWrite::new(&path);
            let mut visited = Vec::new();

            let result = write_with(&path, b"ours", None, |phase, _| {
                visited.push(phase);
                if phase == Phase::Validated {
                    fs::write(&path, b"theirs")?;
                }
                Ok(())
            });

            if attempt.acl_available {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
                assert_eq!(visited.last(), Some(&Phase::Validated));
                assert_eq!(fs::read(&path).unwrap(), b"theirs");
                assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
            } else {
                attempt.assert_native_refusal(&result.unwrap_err());
                assert_eq!(visited, []);
            }
        }

        #[cfg(unix)]
        #[test]
        fn preserves_modes_xattrs_and_native_names() {
            use std::os::unix::fs::PermissionsExt as _;
            use xattr::FileExt as _;

            let dir = tempfile::tempdir().unwrap();
            #[cfg(target_os = "linux")]
            let name = {
                use std::os::unix::ffi::OsStringExt as _;
                std::ffi::OsString::from_vec(b"file-\xff".to_vec())
            };
            #[cfg(not(target_os = "linux"))]
            let name = std::ffi::OsString::from("file-\u{1f4c4}");
            let path = dir.path().join(name);
            fs::write(&path, b"original").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
            let file = File::open(&path).unwrap();
            let attributes_supported = match file.set_xattr("user.ox.binary", b"\x00\xff") {
                Ok(()) => {
                    file.set_xattr("user.ox.empty", b"").unwrap();
                    true
                }
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EOPNOTSUPP));
                    false
                }
            };
            let snapshot = Snapshot::capture(&file);
            let attempt = WorkspaceWrite::new(&path);

            let result = write(&path, b"new", Some(b"original"));

            if attempt.acl_available {
                result.unwrap();
                let current = File::open(&path).unwrap();
                snapshot.unwrap().verify(&current).unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"new");
                if attributes_supported {
                    assert_eq!(
                        current.get_xattr("user.ox.binary").unwrap(),
                        Some(b"\x00\xff".to_vec())
                    );
                    assert_eq!(
                        current.get_xattr("user.ox.empty").unwrap(),
                        Some(Vec::new())
                    );
                } else {
                    assert_eq!(
                        current.list_xattr().unwrap_err().raw_os_error(),
                        Some(nix::libc::EOPNOTSUPP)
                    );
                }
            } else {
                attempt.assert_native_refusal(&snapshot.unwrap_err());
                attempt.assert_native_refusal(&result.unwrap_err());
            }
            assert_eq!(entries(dir.path()), [path.file_name().unwrap()]);
        }

        #[cfg(unix)]
        #[test]
        fn refuses_metadata_changes_and_readback_mismatches() {
            use std::os::unix::fs::PermissionsExt as _;

            for source in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                fs::write(&path, b"original").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                let attempt = WorkspaceWrite::new(&path);
                let mut visited = Vec::new();

                let result = write_with(&path, b"ours", Some(b"original"), |phase, staged| {
                    visited.push(phase);
                    if source && phase == Phase::Synced {
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
                    } else if !source && phase == Phase::Validated {
                        staged.set_permissions(fs::Permissions::from_mode(0o640))?;
                    }
                    Ok(())
                });

                if attempt.acl_available {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        "the file's access policy or metadata could not be preserved",
                        "source={source}"
                    );
                    assert_eq!(
                        visited.last(),
                        Some(&if source {
                            Phase::Synced
                        } else {
                            Phase::Validated
                        })
                    );
                } else {
                    attempt.assert_native_refusal(&result.unwrap_err());
                    assert_eq!(visited, []);
                }
                assert_eq!(fs::read(&path).unwrap(), b"original");
                assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
            }
        }

        #[cfg(unix)]
        #[test]
        fn requires_parent_write_access() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let identity = native::identity(&File::open(&path).unwrap()).unwrap();
            let attempt = WorkspaceWrite::new(&path);
            let denied = crate::util::fs::testing::deny_subdir_creation(dir.path());

            let error = write(&path, b"new", Some(b"original")).unwrap_err();
            drop(denied);

            if attempt.acl_available {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            } else {
                attempt.assert_native_refusal(&error);
            }
            assert_eq!(
                native::identity(&File::open(&path).unwrap()).unwrap(),
                identity
            );
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[cfg(unix)]
        #[test]
        fn new_file_uses_the_parents_normal_creation_mode() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let ordinary = dir.path().join("ordinary");
            let atomic = dir.path().join("atomic");
            fs::write(&ordinary, b"").unwrap();
            let attempt = WorkspaceWrite::new(&atomic);

            let result = write(&atomic, b"content", None);

            if attempt.acl_available {
                result.unwrap();
                assert_eq!(
                    fs::metadata(&atomic).unwrap().permissions().mode(),
                    fs::metadata(&ordinary).unwrap().permissions().mode()
                );
            } else {
                attempt.assert_native_refusal(&result.unwrap_err());
            }
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn preserves_explicit_and_inherited_acls_and_parent_inheritance() {
            use std::os::unix::fs::PermissionsExt as _;

            use nix::unistd::{AccessFlags, access};

            fn set_acl(path: &Path, text: &str) {
                let text = std::ffi::CString::new(text).unwrap();
                native::set_test_acl(&File::open(path).unwrap(), &text).unwrap();
            }

            let dir = tempfile::tempdir().unwrap();
            set_acl(
                dir.path(),
                indoc::indoc! {"
                    !#acl 1
                    group:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:::allow,file_inherit,directory_inherit:read
                "},
            );
            let inherited = dir.path().join("inherited");
            let explicit = dir.path().join("explicit");
            for path in [&inherited, &explicit] {
                fs::write(path, b"original").unwrap();
            }
            fs::set_permissions(&explicit, fs::Permissions::from_mode(0o755)).unwrap();
            access(&explicit, AccessFlags::X_OK).unwrap();
            set_acl(
                &explicit,
                indoc::indoc! {"
                    !#acl 1
                    group:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:::deny:execute
                "},
            );
            let source = Snapshot::capture(&File::open(&explicit).unwrap()).unwrap();
            let inherited_source = Snapshot::capture(&File::open(&inherited).unwrap()).unwrap();
            assert_ne!(source, inherited_source);

            write(&explicit, b"changed", Some(b"original")).unwrap();
            write(&inherited, b"changed", Some(b"original")).unwrap();
            let created = dir.path().join("created");
            write(&created, b"new", None).unwrap();

            source.verify(&File::open(&explicit).unwrap()).unwrap();
            assert_eq!(
                access(&explicit, AccessFlags::X_OK),
                Err(nix::errno::Errno::EACCES)
            );
            inherited_source
                .verify(&File::open(&inherited).unwrap())
                .unwrap();
            inherited_source
                .verify(&File::open(&created).unwrap())
                .unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn preserves_named_user_acl_and_removes_inherited_acl_when_absent() {
            use xattr::FileExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let acl = |owner| {
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
            };
            let file = File::open(&path).unwrap();
            let attributes_supported = match file.set_xattr("system.posix_acl_access", &acl(6)) {
                Ok(()) => true,
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EOPNOTSUPP));
                    false
                }
            };
            let snapshot = Snapshot::capture(&file);
            let attempt = WorkspaceWrite::new(&path);
            if attributes_supported {
                File::open(dir.path())
                    .unwrap()
                    .set_xattr("system.posix_acl_default", &acl(7))
                    .unwrap();
            }

            let result = write(&path, b"new", Some(b"original"));

            if attempt.acl_available {
                result.unwrap();
                snapshot
                    .unwrap()
                    .verify(&File::open(&path).unwrap())
                    .unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"new");
                let current = File::open(&path).unwrap();
                if attributes_supported {
                    assert_eq!(
                        current.get_xattr("system.posix_acl_access").unwrap(),
                        Some(acl(6))
                    );
                    current.remove_xattr("system.posix_acl_access").unwrap();
                    let without_acl = Snapshot::capture(&current).unwrap();

                    write(&path, b"again", Some(b"new")).unwrap();

                    let current = File::open(&path).unwrap();
                    without_acl.verify(&current).unwrap();
                    assert_eq!(current.get_xattr("system.posix_acl_access").unwrap(), None);
                    assert_eq!(fs::read(&path).unwrap(), b"again");
                } else {
                    assert_eq!(current.get_xattr("system.posix_acl_access").unwrap(), None);
                }
            } else {
                attempt.assert_native_refusal(&snapshot.unwrap_err());
                attempt.assert_native_refusal(&result.unwrap_err());
            }
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }
    }
}
