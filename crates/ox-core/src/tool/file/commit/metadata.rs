//! The metadata a Linux rename must reproduce to preserve the original file.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;

use xattr::FileExt as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Snapshot {
    owner: (u32, u32),
    pub(super) mode: u32,
    attributes: BTreeMap<OsString, Vec<u8>>,
}

impl Snapshot {
    pub(super) fn capture(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            owner: (metadata.uid(), metadata.gid()),
            mode: metadata.mode() & 0o7777,
            attributes: attributes(file)?,
        })
    }
}

fn attributes(file: &File) -> io::Result<BTreeMap<OsString, Vec<u8>>> {
    let names = match file.list_xattr() {
        Ok(names) => Some(names),
        Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => None,
        Err(error) => return Err(error),
    };
    let mut attributes = BTreeMap::new();
    for name in names.into_iter().flatten() {
        if let Some(value) = file.get_xattr(&name)? {
            attributes.insert(name, value);
        } else {
            return Err(io::Error::other(
                "the file's metadata changed during inspection",
            ));
        }
    }
    for name in ["system.posix_acl_access", "security.selinux"] {
        if let Some(value) = supported_attribute(file, name)? {
            attributes.insert(name.into(), value);
        }
    }
    Ok(attributes)
}

/// Returns whether the file carries `security.capability`, which an in-place write clears.
pub(super) fn has_capability(file: &File) -> io::Result<bool> {
    Ok(supported_attribute(file, "security.capability")?.is_some())
}

fn supported_attribute(file: &File, name: &str) -> io::Result<Option<Vec<u8>>> {
    match file.get_xattr(name) {
        Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(None),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_reads_the_owner_mode_and_available_attributes() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        std::fs::write(&path, b"original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let file = File::open(&path).unwrap();
        let supported = file.set_xattr("user.ox.value", b"\x00\xff").is_ok();
        if !supported {
            assert_eq!(
                file.list_xattr().unwrap_err().raw_os_error(),
                Some(nix::libc::EOPNOTSUPP),
            );
        }

        let snapshot = Snapshot::capture(&file).unwrap();

        let metadata = file.metadata().unwrap();
        assert_eq!(snapshot.owner, (metadata.uid(), metadata.gid()));
        assert_eq!(snapshot.mode, 0o751);
        if supported {
            assert_eq!(
                snapshot.attributes.get(&OsString::from("user.ox.value")),
                Some(&b"\x00\xff".to_vec()),
            );
        } else {
            assert!(snapshot.attributes.is_empty());
        }
    }

    #[test]
    fn a_captured_snapshot_matches_only_identical_metadata() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        for path in [&first, &second] {
            std::fs::write(path, b"original").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let mut changed = Snapshot::capture(&File::open(&first).unwrap()).unwrap();

        assert_eq!(
            changed,
            Snapshot::capture(&File::open(&second).unwrap()).unwrap()
        );
        changed.mode = 0o600;
        assert_ne!(
            changed,
            Snapshot::capture(&File::open(&second).unwrap()).unwrap()
        );
    }
}
