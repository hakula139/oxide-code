//! Access policy and visible metadata retained by workspace replacement.

use std::fs::File;
use std::io;

#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

#[cfg(unix)]
use xattr::FileExt as _;

#[cfg(any(target_os = "macos", windows))]
use super::native;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Snapshot {
    #[cfg(unix)]
    owner: (u32, u32),
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    attributes: BTreeMap<OsString, Vec<u8>>,
    #[cfg(target_os = "macos")]
    acl: Vec<u8>,
    #[cfg(target_os = "macos")]
    flags: u32,
    #[cfg(windows)]
    security: native::Security,
}

impl Snapshot {
    pub(super) fn capture(file: &File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let metadata = file.metadata()?;
            let mode = metadata.mode() & 0o7777;
            if mode & 0o6000 != 0 {
                return Err(io::Error::other(
                    "refusing to replace a file with set-ID permissions",
                ));
            }
            Ok(Self {
                owner: (metadata.uid(), metadata.gid()),
                mode,
                attributes: attributes(file)?,
                #[cfg(target_os = "macos")]
                acl: native::acl(file)?,
                #[cfg(target_os = "macos")]
                flags: std::os::macos::fs::MetadataExt::st_flags(&metadata) & !0x20,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                security: native::Security::capture(file)?,
            })
        }
    }

    pub(super) fn apply(&self, file: &File) -> io::Result<()> {
        #[cfg(unix)]
        {
            let metadata = file.metadata()?;
            if (metadata.uid(), metadata.gid()) != self.owner {
                nix::unistd::fchown(
                    file,
                    Some(nix::unistd::Uid::from_raw(self.owner.0)),
                    Some(nix::unistd::Gid::from_raw(self.owner.1)),
                )?;
            }
            file.set_permissions(std::fs::Permissions::from_mode(self.mode))?;
            #[cfg(target_os = "macos")]
            native::set_acl(file, &self.acl)?;
            for name in attributes(file)?.keys() {
                if !self.attributes.contains_key(name) {
                    file.remove_xattr(name)?;
                }
            }
            for (name, value) in &self.attributes {
                if file.get_xattr(name)?.as_ref() != Some(value) {
                    file.set_xattr(name, value)?;
                }
            }
            #[cfg(target_os = "macos")]
            native::set_flags(file, self.flags)?;
            Ok(())
        }
        #[cfg(windows)]
        {
            self.security.apply(file)
        }
    }

    pub(super) fn verify(&self, file: &File) -> io::Result<()> {
        if &Self::capture(file)? == self {
            Ok(())
        } else {
            Err(io::Error::other(
                "the file's access policy or metadata could not be preserved",
            ))
        }
    }
}

#[cfg(unix)]
fn attributes(file: &File) -> io::Result<BTreeMap<OsString, Vec<u8>>> {
    let names = match file.list_xattr() {
        Ok(names) => names,
        Err(error) if error.raw_os_error() == Some(nix::errno::Errno::EOPNOTSUPP as i32) => {
            return Ok(BTreeMap::new());
        }
        Err(error) => return Err(error),
    };
    let mut attributes = BTreeMap::new();
    for name in names {
        if let Some(value) = file.get_xattr(&name)? {
            attributes.insert(name, value);
        } else {
            return Err(io::Error::other(
                "the file's metadata changed during inspection",
            ));
        }
    }
    #[cfg(target_os = "linux")]
    {
        for name in ["system.posix_acl_access", "security.selinux"] {
            if let Some(value) = supported_attribute(file, name)? {
                attributes.insert(name.into(), value);
            }
        }
        for name in [
            "security.capability",
            "security.ima",
            "security.evm",
            "system.nfs4_acl",
        ] {
            if supported_attribute(file, name)?.is_some() {
                return Err(io::Error::other(format!(
                    "refusing to replace a file with unsupported access metadata {name:?}"
                )));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;

        if file.metadata()?.st_flags() & 0x20 != 0 {
            // Transparent compression owns these attributes and removes them on a content write.
            attributes.remove(std::ffi::OsStr::new("com.apple.decmpfs"));
            attributes.remove(std::ffi::OsStr::new("com.apple.ResourceFork"));
        }
    }
    Ok(attributes)
}

#[cfg(target_os = "linux")]
fn supported_attribute(file: &File, name: &str) -> io::Result<Option<Vec<u8>>> {
    match file.get_xattr(name) {
        Err(error) if error.raw_os_error() == Some(nix::libc::EOPNOTSUPP) => Ok(None),
        result => result,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    mod apply {
        use super::*;

        #[test]
        fn restores_changed_values_and_removes_extra_attributes() {
            let dir = tempfile::tempdir().unwrap();
            let source = File::create(dir.path().join("source")).unwrap();
            source.set_xattr("user.ox.value", b"\x00\xff").unwrap();
            source.set_xattr("user.ox.empty", b"").unwrap();
            let snapshot = Snapshot::capture(&source).unwrap();
            let staged = File::create(dir.path().join("staged")).unwrap();
            staged.set_xattr("user.ox.value", b"incorrect").unwrap();
            staged.set_xattr("user.ox.extra", b"extra").unwrap();

            snapshot.apply(&staged).unwrap();

            snapshot.verify(&staged).unwrap();
        }

        #[test]
        fn refuses_an_owner_assignment_the_process_cannot_make() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source");
            std::fs::write(&path, b"original").unwrap();
            let file = File::open(&path).unwrap();
            let original = Snapshot::capture(&file).unwrap();
            let mut desired = original.clone();
            desired.owner.0 += 1;

            let error = desired.apply(&file).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            original.verify(&file).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
        }
    }
}
