//! Same-filesystem preparation and publication of workspace file changes.

mod metadata;
mod native;

use std::fs::{self, File, OpenOptions};
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
        .then(|| create_probe(parent))
        .transpose()?;
    let snapshot = match &original {
        Some(original) => original.snapshot.clone(),
        None => Snapshot::capture(probe.as_ref().expect("new files have a probe").as_file())?,
    };
    let mut staged = Staged::new(parent)?;
    #[cfg(any(target_os = "macos", windows))]
    if let Some(original) = &original {
        native::copy(&original.file, path, &staged.file, &staged.path)?;
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

struct Staged {
    file: File,
    path: TempPath,
    _directory: Option<TempDir>,
    #[cfg(windows)]
    published: bool,
}

impl Staged {
    fn new(parent: &Path) -> io::Result<Self> {
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
            let (file, path) = file.into_parts();
            Ok(Self {
                file,
                path,
                _directory: Some(directory),
            })
        }
        #[cfg(windows)]
        {
            let file = tempfile::Builder::new()
                .prefix(".ox-write.")
                .make_in(parent, native::create_private)?;
            let (file, path) = file.into_parts();
            Ok(Self {
                file,
                path,
                _directory: None,
                published: false,
            })
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
impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            _ = native::discard(&self.file);
        }
        self.path.disable_cleanup(true);
    }
}

fn create_probe(parent: &Path) -> io::Result<NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(".ox-mode.");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    builder.tempfile_in(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

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

                    let error = write_with(
                        &path,
                        b"replacement",
                        existing.then_some(b"original".as_slice()),
                        |at, file| {
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

                    assert!(error.to_string().contains("injected"), "{phase:?}");
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
            let snapshot = Snapshot::capture(&original).unwrap();

            write(&path, b"new", Some(b"original")).unwrap();

            let mut bytes = Vec::new();
            original.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"original");
            let replacement = File::open(&path).unwrap();
            assert_ne!(
                native::identity(&replacement).unwrap(),
                native::identity(&original).unwrap()
            );
            snapshot.verify(&replacement).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
        }

        #[test]
        fn new_file_and_empty_replacement_publish_complete_bytes() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");

            write(&path, b"new", None).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"new");
            write(&path, b"", Some(b"new")).unwrap();

            assert_eq!(fs::read(&path).unwrap(), b"");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
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

                write(&path, &bytes, existing.then_some(b"original".as_slice())).unwrap();

                assert_eq!(fs::read(&path).unwrap(), bytes, "existing={existing}");
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

                    let error = write_with(
                        &path,
                        b"ours",
                        existing.then_some(b"original".as_slice()),
                        |phase, mut staged| {
                            if phase == Phase::Validated {
                                staged.set_len(0)?;
                                staged.rewind()?;
                                staged.write_all(changed)?;
                            }
                            Ok(())
                        },
                    )
                    .unwrap_err();

                    assert!(
                        error.to_string().contains("prepared file changed"),
                        "{error}"
                    );
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
                    let destination = path.clone();
                    let target = victim.clone();

                    let error = tracker
                        .write_and_record("toolu_replace", async move {
                            write_with(
                                &destination,
                                b"ours",
                                existing.then_some(b"original".as_slice()),
                                |phase, _| {
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

                    assert!(error.to_string().contains("pathname changed"), "{error}");
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

                let result = write_with(&path, b"ours", Some(b"original"), |phase, _| {
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

                assert!(result.is_err(), "replace={replace}");
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

            let result = write_with(&path, b"ours", None, |phase, _| {
                if phase == Phase::Validated {
                    fs::write(&path, b"theirs")?;
                }
                Ok(())
            });

            assert!(result.is_err());
            assert_eq!(fs::read(&path).unwrap(), b"theirs");
            assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
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
            file.set_xattr("user.ox.binary", b"\x00\xff").unwrap();
            file.set_xattr("user.ox.empty", b"").unwrap();
            let snapshot = Snapshot::capture(&file).unwrap();

            write(&path, b"new", Some(b"original")).unwrap();

            snapshot.verify(&File::open(&path).unwrap()).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"new");
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

                let result = write_with(&path, b"ours", Some(b"original"), |phase, staged| {
                    if source && phase == Phase::Synced {
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
                    } else if !source && phase == Phase::Validated {
                        staged.set_permissions(fs::Permissions::from_mode(0o640))?;
                    }
                    Ok(())
                });

                assert!(result.is_err(), "source={source}");
                assert_eq!(fs::read(&path).unwrap(), b"original");
                assert_eq!(entries(dir.path()), [std::ffi::OsString::from("target")]);
            }
        }

        #[cfg(unix)]
        #[test]
        fn requires_parent_write_access_and_refuses_set_id_files() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            fs::write(&path, b"original").unwrap();
            let denied = crate::util::fs::testing::deny_subdir_creation(dir.path());

            assert!(write(&path, b"new", Some(b"original")).is_err());
            drop(denied);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).unwrap();
            let error = write(&path, b"new", Some(b"original")).unwrap_err();

            assert!(error.to_string().contains("set-ID"));
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

            write(&atomic, b"content", None).unwrap();

            assert_eq!(
                fs::metadata(&atomic).unwrap().permissions().mode(),
                fs::metadata(&ordinary).unwrap().permissions().mode()
            );
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn preserves_explicit_and_inherited_acls_and_parent_inheritance() {
            fn chmod(path: &Path, arguments: &[&str]) {
                let output = std::process::Command::new("/bin/chmod")
                    .args(arguments)
                    .arg(path)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let dir = tempfile::tempdir().unwrap();
            chmod(
                dir.path(),
                &["+a", "everyone allow read,file_inherit,directory_inherit"],
            );
            let inherited = dir.path().join("inherited");
            let explicit = dir.path().join("explicit");
            for path in [&inherited, &explicit] {
                fs::write(path, b"original").unwrap();
            }
            chmod(&explicit, &["-N"]);
            chmod(&explicit, &["+a", "everyone deny execute"]);
            let source = Snapshot::capture(&File::open(&explicit).unwrap()).unwrap();
            let inherited_source = Snapshot::capture(&File::open(&inherited).unwrap()).unwrap();
            assert_ne!(source, inherited_source);

            write(&explicit, b"changed", Some(b"original")).unwrap();
            write(&inherited, b"changed", Some(b"original")).unwrap();
            let created = dir.path().join("created");
            write(&created, b"new", None).unwrap();

            source.verify(&File::open(&explicit).unwrap()).unwrap();
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
            file.set_xattr("system.posix_acl_access", &acl(6)).unwrap();
            let snapshot = Snapshot::capture(&file).unwrap();
            File::open(dir.path())
                .unwrap()
                .set_xattr("system.posix_acl_default", &acl(7))
                .unwrap();

            write(&path, b"new", Some(b"original")).unwrap();

            snapshot.verify(&File::open(&path).unwrap()).unwrap();
            File::open(&path)
                .unwrap()
                .remove_xattr("system.posix_acl_access")
                .unwrap();
            let without_acl = Snapshot::capture(&File::open(&path).unwrap()).unwrap();
            write(&path, b"again", Some(b"new")).unwrap();
            without_acl.verify(&File::open(&path).unwrap()).unwrap();
        }
    }
}
