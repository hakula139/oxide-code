//! Native operations whose handle and buffer contracts are contained here.

#[cfg(windows)]
mod extended;

use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt as _, io::AsRawHandle as _};
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation, Security as WinSecurity, Storage::FileSystem, System::SystemServices,
};

#[cfg(unix)]
pub(super) fn identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
pub(super) fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    crate::util::fs::file_id(file)
}

#[cfg(unix)]
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    Ok(winapi_util::file::information(file)?.number_of_links())
}

#[cfg(unix)]
pub(super) fn publish_new(source: &Path, destination: &Path) -> io::Result<()> {
    // The staging guard removes its link after publication, including if that cleanup fails once.
    std::fs::hard_link(source, destination)
}

#[cfg(target_os = "macos")]
pub(super) fn make_private(file: &File) -> io::Result<()> {
    set_acl(file, &[])
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "macOS ACL functions are absent from the safe filesystem APIs"
)]
unsafe extern "C" {
    fn acl_get_fd_np(fd: i32, kind: u32) -> *mut std::ffi::c_void;
    fn acl_init(count: i32) -> *mut std::ffi::c_void;
    fn acl_set_fd(fd: i32, acl: *mut std::ffi::c_void) -> i32;
    fn acl_size(acl: *mut std::ffi::c_void) -> isize;
    fn acl_copy_ext(
        buffer: *mut std::ffi::c_void,
        acl: *mut std::ffi::c_void,
        size: isize,
    ) -> isize;
    fn acl_copy_int(buffer: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn acl_free(acl: *mut std::ffi::c_void) -> i32;
}

#[cfg(target_os = "macos")]
struct Acl(std::ptr::NonNull<std::ffi::c_void>);

#[cfg(target_os = "macos")]
impl Acl {
    fn from_pointer(pointer: *mut std::ffi::c_void) -> io::Result<Self> {
        std::ptr::NonNull::new(pointer)
            .map(Self)
            .ok_or_else(io::Error::last_os_error)
    }
}

#[cfg(target_os = "macos")]
impl Drop for Acl {
    #[expect(
        unsafe_code,
        reason = "this guard exclusively owns the allocation returned by the ACL API"
    )]
    fn drop(&mut self) {
        // The pointer came from an allocating ACL function and is freed exactly once.
        _ = unsafe { acl_free(self.0.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "capture an owned macOS ACL and serialize it into a sized buffer"
)]
pub(super) fn acl(file: &File) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd as _;

    // ACL_TYPE_EXTENDED selects the file's access ACL. The descriptor remains borrowed.
    let pointer = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if pointer.is_null() && io::Error::last_os_error().raw_os_error() == Some(nix::libc::ENOENT) {
        return Ok(Vec::new());
    }
    let acl = Acl::from_pointer(pointer)?;
    // Both calls borrow a valid ACL owned by the guard. The output buffer has the requested size.
    let size = unsafe { acl_size(acl.0.as_ptr()) };
    let capacity = usize::try_from(size).map_err(|_| io::Error::last_os_error())?;
    let mut bytes = vec![0; capacity];
    let copied = unsafe { acl_copy_ext(bytes.as_mut_ptr().cast(), acl.0.as_ptr(), size) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "restore only an ACL serialized by this module or an empty ACL"
)]
pub(super) fn set_acl(file: &File, bytes: &[u8]) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // Nonempty input is the unmodified output of acl_copy_ext. No caller supplies external bytes.
    let pointer = unsafe {
        if bytes.is_empty() {
            acl_init(0)
        } else {
            acl_copy_int(bytes.as_ptr().cast())
        }
    };
    let acl = Acl::from_pointer(pointer)?;
    // The descriptor and owned ACL stay alive for the duration of this call.
    zero_result(unsafe { acl_set_fd(file.as_raw_fd(), acl.0.as_ptr()) })
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "fchflags applies the captured non-compression BSD flags"
)]
pub(super) fn set_flags(file: &File, flags: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // The borrowed descriptor remains valid and fchflags does not take ownership.
    zero_result(unsafe { nix::libc::fchflags(file.as_raw_fd(), flags) })
}

#[cfg(target_os = "macos")]
fn zero_result(result: i32) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::path::{Component, Prefix};

    let absolute = std::path::absolute(path)?;
    let mut wide = Vec::new();
    let skip = match absolute.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => {
                wide.extend("\\\\?\\".encode_utf16());
                0
            }
            Prefix::UNC(_, _) => {
                wide.extend("\\\\?\\UNC\\".encode_utf16());
                2
            }
            _ => 0,
        },
        _ => 0,
    };
    wide.extend(absolute.as_os_str().encode_wide().skip(skip));
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
fn windows_result(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
struct LocalMemory(std::ptr::NonNull<std::ffi::c_void>);

#[cfg(windows)]
impl LocalMemory {
    fn new(pointer: *mut std::ffi::c_void) -> io::Result<Self> {
        std::ptr::NonNull::new(pointer)
            .map(Self)
            .ok_or_else(io::Error::last_os_error)
    }
}

#[cfg(windows)]
impl Drop for LocalMemory {
    #[expect(
        unsafe_code,
        reason = "Windows returns these owned security allocations for LocalFree"
    )]
    fn drop(&mut self) {
        // Only successful LocalAlloc-backed Windows API results enter this guard.
        unsafe {
            Foundation::LocalFree(self.0.as_ptr());
        }
    }
}

#[cfg(windows)]
const ACCESS_INFORMATION: u32 = WinSecurity::OWNER_SECURITY_INFORMATION
    | WinSecurity::GROUP_SECURITY_INFORMATION
    | WinSecurity::DACL_SECURITY_INFORMATION
    | WinSecurity::LABEL_SECURITY_INFORMATION;

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Access {
    owner: Option<NativeBytes>,
    group: Option<NativeBytes>,
    dacl: Option<NativeBytes>,
    label: Option<NativeBytes>,
    control: u16,
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeBytes(Vec<u32>);

#[cfg(windows)]
impl NativeBytes {
    #[expect(
        unsafe_code,
        reason = "copy a SID or ACL borrowed from a valid native descriptor into aligned owned storage"
    )]
    unsafe fn copy(pointer: *const std::ffi::c_void, length: usize) -> Self {
        let mut words = vec![0_u32; length.div_ceil(4)];
        // The caller supplies a valid descriptor component and its API-reported byte length.
        unsafe {
            std::ptr::copy_nonoverlapping(pointer.cast::<u8>(), words.as_mut_ptr().cast(), length);
        }
        Self(words)
    }

    fn pointer(value: &Option<Self>) -> *mut std::ffi::c_void {
        value.as_ref().map_or(std::ptr::null_mut(), |bytes| {
            bytes.0.as_ptr().cast_mut().cast()
        })
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "copy the native descriptor components before freeing their owning allocation"
)]
fn access(file: &File) -> io::Result<Access> {
    use WinSecurity::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};

    let mut pointer = std::ptr::null_mut();
    let mut owner = std::ptr::null_mut();
    let mut group = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut label = std::ptr::null_mut();
    // READ_CONTROL suffices for owner, group, DACL and the mandatory integrity label.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            ACCESS_INFORMATION,
            &raw mut owner,
            &raw mut group,
            &raw mut dacl,
            &raw mut label,
            &raw mut pointer,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error.cast_signed()));
    }
    let descriptor = LocalMemory::new(pointer)?;
    let mut control = 0;
    let mut revision = 0;
    // All component pointers borrow this live Windows-owned descriptor.
    windows_result(unsafe {
        WinSecurity::GetSecurityDescriptorControl(
            descriptor.0.as_ptr(),
            &raw mut control,
            &raw mut revision,
        )
    })?;
    let sid = |pointer: WinSecurity::PSID| {
        // A non-null component is a valid SID returned by GetSecurityInfo.
        (!pointer.is_null()).then(|| unsafe {
            NativeBytes::copy(pointer, WinSecurity::GetLengthSid(pointer) as usize)
        })
    };
    let acl = |pointer: *mut WinSecurity::ACL| {
        // A non-null component includes an ACL header whose AclSize covers the whole allocation.
        (!pointer.is_null())
            .then(|| unsafe { NativeBytes::copy(pointer.cast(), usize::from((*pointer).AclSize)) })
    };
    Ok(Access {
        owner: sid(owner),
        group: sid(group),
        dacl: acl(dacl),
        label: acl(label),
        control: control
            & (WinSecurity::SE_DACL_PRESENT
                | WinSecurity::SE_DACL_PROTECTED
                | WinSecurity::SE_DACL_AUTO_INHERITED
                | WinSecurity::SE_DACL_AUTO_INHERIT_REQ),
    })
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "parse internally captured SDDL into a Windows-owned descriptor"
)]
fn descriptor(sddl: &[u16]) -> io::Result<LocalMemory> {
    let mut pointer = std::ptr::null_mut();
    if sddl.last() != Some(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unterminated security descriptor",
        ));
    }
    // The input is terminated and remains borrowed for the duration of parsing.
    windows_result(unsafe {
        WinSecurity::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut pointer,
            std::ptr::null_mut(),
        )
    })?;
    LocalMemory::new(pointer)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "restore the captured access descriptor on the already-open private file"
)]
fn set_access(file: &File, access: &Access) -> io::Result<()> {
    let current = self::access(file)?;
    let mut information = 0;
    if access.owner != current.owner {
        information |= WinSecurity::OWNER_SECURITY_INFORMATION;
    }
    if access.group != current.group {
        information |= WinSecurity::GROUP_SECURITY_INFORMATION;
    }
    if access.label != current.label {
        information |= WinSecurity::LABEL_SECURITY_INFORMATION;
    }
    let protection = if access.control & WinSecurity::SE_DACL_PROTECTED != 0 {
        WinSecurity::PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        WinSecurity::UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    if access.dacl != current.dacl || access.control != current.control {
        information |= WinSecurity::DACL_SECURITY_INFORMATION | protection;
    }
    if information == 0 {
        return Ok(());
    }
    // Each pointer borrows an aligned SID or ACL captured by GetSecurityInfo. Null ACLs stay null.
    let error = unsafe {
        WinSecurity::Authorization::SetSecurityInfo(
            file.as_raw_handle(),
            WinSecurity::Authorization::SE_FILE_OBJECT,
            information,
            NativeBytes::pointer(&access.owner),
            NativeBytes::pointer(&access.group),
            NativeBytes::pointer(&access.dacl).cast(),
            NativeBytes::pointer(&access.label).cast(),
        )
    };
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(error.cast_signed()))
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "CreateFileW must receive the restrictive DACL at creation"
)]
pub(super) fn create_private(path: &Path) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle as _;

    let name = wide(path)?;
    let sddl: Vec<_> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
    let descriptor = descriptor(&sddl)?;
    let attributes = WinSecurity::SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<WinSecurity::SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES fits u32"),
        lpSecurityDescriptor: descriptor.0.as_ptr(),
        bInheritHandle: 0,
    };
    // CREATE_NEW cannot open an attacker-created entry. The DACL grants only the file owner access.
    let handle = unsafe {
        FileSystem::CreateFileW(
            name.as_ptr(),
            Foundation::GENERIC_ALL,
            FileSystem::FILE_SHARE_READ | FileSystem::FILE_SHARE_WRITE,
            &raw const attributes,
            FileSystem::CREATE_NEW,
            FileSystem::FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // Successful CreateFileW transfers one owned handle to File.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "publish through the pinned staging handle with a sized native rename record"
)]
pub(super) fn publish(file: &File, destination: &Path, replace: bool) -> io::Result<()> {
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
    };

    let destination = wide(destination)?;
    let name_bytes = u32::try_from((destination.len() - 1) * 2).map_err(io::Error::other)?;
    let offset = std::mem::offset_of!(FileSystem::FILE_RENAME_INFO, FileName);
    let size = offset + destination.len() * 2;
    let size_u32 = u32::try_from(size).map_err(io::Error::other)?;
    let mut storage = vec![0_u64; size.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<FileSystem::FILE_RENAME_INFO>();
    // u64 storage supplies native alignment and covers the header plus the terminated UTF-16 name.
    unsafe {
        (&raw mut (*info).Anonymous).write(if replace {
            FileSystem::FILE_RENAME_INFO_0 {
                Flags: FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS,
            }
        } else {
            FileSystem::FILE_RENAME_INFO_0 {
                ReplaceIfExists: false,
            }
        });
        (&raw mut (*info).RootDirectory).write(std::ptr::null_mut());
        (&raw mut (*info).FileNameLength).write(name_bytes);
        destination
            .as_ptr()
            .copy_to_nonoverlapping((&raw mut (*info).FileName).cast(), destination.len());
        windows_result(FileSystem::SetFileInformationByHandle(
            file.as_raw_handle(),
            if replace {
                FileSystem::FileRenameInfoEx
            } else {
                FileSystem::FileRenameInfo
            },
            info.cast(),
            size_u32,
        ))
    }
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "dispose an unpublished temporary file using the DELETE authority retained by its handle"
)]
pub(super) fn discard(file: &File) -> io::Result<()> {
    let disposition = FileSystem::FILE_DISPOSITION_INFO { DeleteFile: true };
    // The original handle retains DELETE access even when the restored DACL denies new delete opens.
    windows_result(unsafe {
        FileSystem::SetFileInformationByHandle(
            file.as_raw_handle(),
            FileSystem::FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(std::mem::size_of_val(&disposition))
                .expect("FILE_DISPOSITION_INFO fits u32"),
        )
    })
}

#[cfg(windows)]
struct CopyIdentity {
    source: (u64, [u8; 16]),
    destination: (u64, [u8; 16]),
    checked: AtomicBool,
    failed: AtomicBool,
    #[cfg(test)]
    fail_after_chunk: bool,
    #[cfg(test)]
    copied_data: AtomicBool,
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "CopyFileExW lends its actual source and destination handles to this callback"
)]
unsafe extern "system" fn check_copy_identity(
    _: i64,
    transferred: i64,
    _: i64,
    _: i64,
    _: u32,
    reason: u32,
    source: Foundation::HANDLE,
    destination: Foundation::HANDLE,
    context: *const std::ffi::c_void,
) -> u32 {
    use std::os::windows::io::BorrowedHandle;

    // The synchronous caller owns this context until CopyFileExW returns.
    let context = unsafe { &*context.cast::<CopyIdentity>() };
    #[cfg(not(test))]
    _ = transferred;
    #[cfg(test)]
    if context.fail_after_chunk && reason == FileSystem::CALLBACK_CHUNK_FINISHED {
        context
            .copied_data
            .store(transferred > 0, Ordering::Relaxed);
        context.failed.store(true, Ordering::Relaxed);
        return FileSystem::PROGRESS_CANCEL;
    }
    if reason != FileSystem::CALLBACK_STREAM_SWITCH {
        return FileSystem::PROGRESS_CONTINUE;
    }
    let matches = |handle, expected| {
        // The copy API lends these live handles only for the callback. They are never closed here.
        let handle = unsafe { BorrowedHandle::borrow_raw(handle) };
        crate::util::fs::file_id(handle).is_ok_and(|id| id == expected)
    };
    context.checked.store(true, Ordering::Relaxed);
    if !matches(source, context.source) || !matches(destination, context.destination) {
        context.failed.store(true, Ordering::Relaxed);
    }
    if context.failed.load(Ordering::Relaxed) {
        FileSystem::PROGRESS_CANCEL
    } else {
        FileSystem::PROGRESS_CONTINUE
    }
}

#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "fcopyfile has no safe descriptor-bound standard library API"
)]
pub(super) fn copy(source: &File, _: &Path, destination: &File, _: &Path) -> io::Result<()> {
    use std::io::Seek as _;
    use std::os::fd::AsRawFd as _;

    let mut source = source;
    source.rewind()?;
    // A null state asks libc to own its state. DATA | XATTR leaves the staging access policy private.
    let result = unsafe {
        nix::libc::fcopyfile(
            source.as_raw_fd(),
            destination.as_raw_fd(),
            std::ptr::null_mut(),
            12,
        )
    };
    zero_result(result)
}

#[cfg(windows)]
pub(super) fn copy(
    source: &File,
    source_path: &Path,
    destination: &File,
    destination_path: &Path,
) -> io::Result<()> {
    let context = CopyIdentity {
        source: identity(source)?,
        destination: identity(destination)?,
        checked: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        #[cfg(test)]
        fail_after_chunk: false,
        #[cfg(test)]
        copied_data: AtomicBool::new(false),
    };
    copy_with(source_path, destination_path, &context)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "native copying preserves application streams and EAs, with opened-handle identity checks"
)]
fn copy_with(
    source_path: &Path,
    destination_path: &Path,
    context: &CopyIdentity,
) -> io::Result<()> {
    let source_path = wide(source_path)?;
    let destination_path = wide(destination_path)?;
    // The callback only borrows this live context. Both native paths remain terminated and alive.
    let result = unsafe {
        FileSystem::CopyFileExW(
            source_path.as_ptr(),
            destination_path.as_ptr(),
            Some(check_copy_identity),
            std::ptr::from_ref(context).cast(),
            std::ptr::null_mut(),
            0,
        )
    };
    if context.failed.load(Ordering::Relaxed) {
        return Err(io::Error::other(
            "the native copy could not validate its opened files",
        ));
    }
    windows_result(result)?;
    if !context.checked.load(Ordering::Relaxed) {
        return Err(io::Error::other(
            "the native copy did not expose its opened files",
        ));
    }
    Ok(())
}

#[cfg(windows)]
const PRESERVED_ATTRIBUTES: u32 = FileSystem::FILE_ATTRIBUTE_HIDDEN
    | FileSystem::FILE_ATTRIBUTE_READONLY
    | FileSystem::FILE_ATTRIBUTE_SYSTEM
    | FileSystem::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Security {
    access: Access,
    attributes: u32,
    compression: u16,
    extended: extended::Snapshot,
    streams: std::collections::BTreeMap<Vec<u16>, u64>,
}

#[cfg(windows)]
impl Security {
    pub(super) fn capture(file: &File) -> io::Result<Self> {
        use std::os::windows::fs::MetadataExt as _;

        let attributes = file.metadata()?.file_attributes();
        if attributes & FileSystem::FILE_ATTRIBUTE_ENCRYPTED != 0 {
            return Err(io::Error::other(
                "refusing to replace a file with EFS access metadata",
            ));
        }
        let external = externally_backed(file)?;
        if attributes & FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 && !external {
            return Err(io::Error::other(
                "refusing to replace an unsupported reparse point",
            ));
        }
        check_access_classes(|information| access_entries(file, information))?;
        Ok(Self {
            access: access(file)?,
            attributes: attributes & PRESERVED_ATTRIBUTES,
            compression: compression(file)?,
            extended: extended::capture(file)?,
            streams: streams(file, external)?,
        })
    }

    pub(super) fn apply(&self, file: &File) -> io::Result<()> {
        if compression(file)? != self.compression {
            set_compression(file, self.compression)?;
        }
        set_attributes(file, self.attributes)?;
        set_access(file, &self.access)
    }
}

#[cfg(windows)]
fn check_access_classes(mut entries: impl FnMut(u32) -> io::Result<u16>) -> io::Result<()> {
    for (information, name) in [
        (
            WinSecurity::ATTRIBUTE_SECURITY_INFORMATION,
            "resource attributes",
        ),
        (
            WinSecurity::SCOPE_SECURITY_INFORMATION,
            "central access policy",
        ),
        (
            SystemServices::PROCESS_TRUST_LABEL_SECURITY_INFORMATION.cast_unsigned(),
            "process trust label",
        ),
        (
            SystemServices::ACCESS_FILTER_SECURITY_INFORMATION.cast_unsigned(),
            "access filter",
        ),
    ] {
        let count = entries(information).map_err(|error| {
            io::Error::new(error.kind(), format!("failed to inspect {name}: {error}"))
        })?;
        if count != 0 {
            return Err(io::Error::other(format!(
                "refusing to replace a file with unsupported access metadata: {name}"
            )));
        }
    }
    Ok(())
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "query only a readable access-control ACL class and inspect its native header"
)]
fn access_entries(file: &File, information: u32) -> io::Result<u16> {
    let mut pointer = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    // The selected access classes require READ_CONTROL without requesting privileged audit data.
    let error = unsafe {
        WinSecurity::Authorization::GetSecurityInfo(
            file.as_raw_handle(),
            WinSecurity::Authorization::SE_FILE_OBJECT,
            information,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut acl,
            &raw mut pointer,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error.cast_signed()));
    }
    let _descriptor = LocalMemory::new(pointer)?;
    // The non-null ACL header borrows the live descriptor allocation owned by this guard.
    Ok(if acl.is_null() {
        0
    } else {
        unsafe { (*acl).AceCount }
    })
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "query native compression into a correctly sized stack value"
)]
fn compression(file: &File) -> io::Result<u16> {
    use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_GET_COMPRESSION};

    let mut format: u16 = 0;
    let mut returned = 0;
    // The synchronous call borrows the file and writes at most two bytes into format.
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_GET_COMPRESSION,
            std::ptr::null(),
            0,
            (&raw mut format).cast(),
            2,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if result == 0 && unsupported_ioctl(&io::Error::last_os_error()) {
        return Ok(0);
    }
    windows_result(result)?;
    Ok(format)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "restore the captured native compression mode through the borrowed handle"
)]
fn set_compression(file: &File, format: u16) -> io::Result<()> {
    use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_COMPRESSION};

    let mut returned = 0;
    // The synchronous call reads the two-byte format and has no output buffer.
    windows_result(unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_COMPRESSION,
            (&raw const format).cast(),
            2,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    })
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "identify WOF storage through the opened file without following its pathname"
)]
fn externally_backed(file: &File) -> io::Result<bool> {
    use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_GET_EXTERNAL_BACKING};

    let mut buffer = [0_u64; 32];
    let mut returned = 0;
    // This buffer accommodates the documented WOF external information and provider records.
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_GET_EXTERNAL_BACKING,
            std::ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            256,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if unsupported_ioctl(&error)
        || error.raw_os_error()
            == Some(Foundation::ERROR_OBJECT_NOT_EXTERNALLY_BACKED.cast_signed())
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn unsupported_ioctl(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        [
            Foundation::ERROR_INVALID_FUNCTION,
            Foundation::ERROR_NOT_SUPPORTED,
        ]
        .contains(&code.cast_unsigned())
    })
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "set only supported basic attributes through the already-open file"
)]
fn set_attributes(file: &File, attributes: u32) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    let current = file.metadata()?.file_attributes();
    let updated = (current
        & !(PRESERVED_ATTRIBUTES
            | FileSystem::FILE_ATTRIBUTE_TEMPORARY
            | FileSystem::FILE_ATTRIBUTE_NORMAL))
        | attributes;
    let basic = FileSystem::FILE_BASIC_INFO {
        FileAttributes: if updated == 0 {
            FileSystem::FILE_ATTRIBUTE_NORMAL
        } else {
            updated
        },
        ..FileSystem::FILE_BASIC_INFO::default()
    };
    // Zero timestamp fields leave the content write's timestamps intact.
    windows_result(unsafe {
        FileSystem::SetFileInformationByHandle(
            file.as_raw_handle(),
            FileSystem::FileBasicInfo,
            (&raw const basic).cast(),
            u32::try_from(std::mem::size_of_val(&basic)).expect("FILE_BASIC_INFO fits u32"),
        )
    })
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "query the opened file's native path into a length-checked UTF-16 buffer"
)]
fn opened_path(file: &File) -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;

    let mut buffer = vec![0_u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // Windows writes at most capacity units and reports a required length on short buffers.
        let length = unsafe {
            FileSystem::GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                0,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < capacity {
            return Ok(std::ffi::OsString::from_wide(&buffer[..length as usize]).into());
        }
        buffer.resize(length as usize + 1, 0);
    }
}

#[cfg(windows)]
fn streams(file: &File, external: bool) -> io::Result<std::collections::BTreeMap<Vec<u16>, u64>> {
    use std::io::Read as _;
    use std::os::windows::ffi::OsStringExt as _;

    let path = opened_path(file)?;
    let expected = identity(file)?;
    let mut streams = std::collections::BTreeMap::new();
    for name in stream_names(file)? {
        if name == "::$DATA".encode_utf16().collect::<Vec<_>>()
            || (external
                && name
                    == ":WofCompressedData:$DATA"
                        .encode_utf16()
                        .collect::<Vec<_>>())
        {
            continue;
        }
        let mut stream_path = path.as_os_str().to_owned();
        stream_path.push(std::ffi::OsString::from_wide(&name));
        let mut stream = File::open(stream_path)?;
        if identity(&stream)? != expected {
            return Err(io::Error::other(
                "the file was replaced while inspecting its streams",
            ));
        }
        let mut hash = xxhash_rust::xxh64::Xxh64::new(0);
        let mut buffer = [0; 8192];
        loop {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        streams.insert(name, hash.digest());
    }
    Ok(streams)
}

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "query variable-length native stream records into an aligned owned buffer"
)]
fn stream_names(file: &File) -> io::Result<Vec<Vec<u16>>> {
    let mut buffer = vec![0_u64; 512];
    loop {
        let capacity =
            u32::try_from(std::mem::size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
        // The aligned buffer's full byte length is supplied to the synchronous Windows query.
        let result = unsafe {
            FileSystem::GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileSystem::FileStreamInfo,
                buffer.as_mut_ptr().cast(),
                capacity,
            )
        };
        if result != 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(Foundation::ERROR_HANDLE_EOF.cast_signed()) {
            return Ok(Vec::new());
        }
        if error.raw_os_error() != Some(Foundation::ERROR_MORE_DATA.cast_signed()) {
            return Err(error);
        }
        buffer.resize(buffer.len() * 2, 0);
    }
    let bytes: Vec<_> = buffer
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    parse_stream_names(&bytes)
}

#[cfg(windows)]
fn parse_stream_names(bytes: &[u8]) -> io::Result<Vec<Vec<u16>>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid native stream information",
        )
    };
    let mut names = Vec::new();
    let mut offset = 0;
    loop {
        let header = bytes.get(offset..offset + 24).ok_or_else(invalid)?;
        let next = u32::from_ne_bytes(header[..4].try_into().map_err(|_| invalid())?) as usize;
        let size = u32::from_ne_bytes(header[4..8].try_into().map_err(|_| invalid())?) as usize;
        let name = bytes
            .get(offset + 24..offset + 24 + size)
            .ok_or_else(invalid)?;
        if !size.is_multiple_of(2) {
            return Err(invalid());
        }
        names.push(
            name.chunks_exact(2)
                .map(|unit| u16::from_ne_bytes([unit[0], unit[1]]))
                .collect(),
        );
        if next == 0 {
            break;
        }
        if next < 24 + size {
            return Err(invalid());
        }
        offset = offset.checked_add(next).ok_or_else(invalid)?;
    }
    Ok(names)
}

#[cfg(all(test, windows))]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;

    use super::*;
    use crate::tool::file::commit::{Phase, TemporaryFile, write, write_with};

    fn stream_path(path: &Path, name: &str) -> std::path::PathBuf {
        let mut path = path.as_os_str().to_owned();
        path.push(format!(":{name}"));
        path.into()
    }

    #[expect(
        unsafe_code,
        reason = "copy a test ACL from a live native descriptor into its owned aligned representation"
    )]
    fn descriptor_acl(sddl: &str, system: bool) -> Option<NativeBytes> {
        let descriptor = descriptor(&sddl.encode_utf16().chain([0]).collect::<Vec<_>>()).unwrap();
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        let query = if system {
            WinSecurity::GetSecurityDescriptorSacl
        } else {
            WinSecurity::GetSecurityDescriptorDacl
        };
        // The query borrows the live descriptor, whose ACL header reports the full copied size.
        unsafe {
            windows_result(query(
                descriptor.0.as_ptr(),
                &raw mut present,
                &raw mut acl,
                &raw mut defaulted,
            ))
            .unwrap();
            assert_ne!(present, 0);
            (!acl.is_null()).then(|| NativeBytes::copy(acl.cast(), usize::from((*acl).AclSize)))
        }
    }

    #[expect(
        unsafe_code,
        reason = "install an unprivileged resource-attribute fixture using its owned ACL buffer"
    )]
    fn set_resource_attributes(file: &File) -> io::Result<()> {
        let acl = descriptor_acl(r#"S:(RA;;;;;WD;("Department",TS,0x0,"Restricted"))"#, true);
        // ATTRIBUTE_SECURITY_INFORMATION needs WRITE_DAC. The owned ACL remains alive through the call.
        let error = unsafe {
            WinSecurity::Authorization::SetSecurityInfo(
                file.as_raw_handle(),
                WinSecurity::Authorization::SE_FILE_OBJECT,
                WinSecurity::ATTRIBUTE_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                NativeBytes::pointer(&acl).cast(),
            )
        };
        if error == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(error.cast_signed()))
        }
    }

    mod wide {
        use super::*;

        #[test]
        fn native_paths_preserve_verbatim_names_and_expand_dos_and_unc_prefixes() {
            for (source, expected) in [
                (r"C:\workspace\file", r"\\?\C:\workspace\file"),
                (r"\\server\share\file", r"\\?\UNC\server\share\file"),
                (r"\\?\C:\workspace\file", r"\\?\C:\workspace\file"),
            ] {
                let actual = wide(Path::new(source)).unwrap();

                assert_eq!(
                    actual,
                    expected.encode_utf16().chain([0]).collect::<Vec<_>>(),
                    "{source}"
                );
            }
            assert!(wide(Path::new("invalid\0path")).is_err());
        }
    }

    mod set_access {
        use super::*;

        #[test]
        fn null_and_empty_acl_states_remain_distinct() {
            let dir = tempfile::tempdir().unwrap();
            let file = create_private(&dir.path().join("file")).unwrap();
            let mut desired = access(&file).unwrap();
            desired.control = WinSecurity::SE_DACL_PRESENT | WinSecurity::SE_DACL_PROTECTED;

            for dacl in [
                None,
                Some(NativeBytes(vec![u32::from_ne_bytes([2, 0, 8, 0]), 0])),
            ] {
                desired.dacl = dacl;
                set_access(&file, &desired).unwrap();

                assert_eq!(access(&file).unwrap(), desired);
            }
            let path = dir.path().join("file");
            assert!(File::open(&path).is_err());
            desired.dacl = None;
            set_access(&file, &desired).unwrap();
            assert!(File::open(&path).is_ok());
        }

        #[test]
        fn a_denied_acl_setter_does_not_change_the_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, b"original").unwrap();
            let read_only = File::open(&path).unwrap();
            let before = access(&read_only).unwrap();
            let mut desired = before.clone();
            desired.dacl = None;

            assert!(set_access(&read_only, &desired).is_err());

            assert_eq!(access(&read_only).unwrap(), before);
            assert_eq!(fs::read(&path).unwrap(), b"original");
        }
    }

    mod publish {
        use super::*;

        #[test]
        fn extended_replacement_cannot_replace_a_delete_pinned_target() {
            let dir = tempfile::tempdir().unwrap();
            let target = TemporaryFile::staged(dir.path()).unwrap();
            let source = TemporaryFile::staged(dir.path()).unwrap();
            (&target.file).write_all(b"target").unwrap();
            (&source.file).write_all(b"source").unwrap();
            let identity_before = identity(&target.file).unwrap();

            assert!(publish(&source.file, &target.path, true).is_err());

            assert_eq!(
                identity(&File::open(&target.path).unwrap()).unwrap(),
                identity_before
            );
            assert_eq!(fs::read(&target.path).unwrap(), b"target");
            assert_eq!(fs::read(&source.path).unwrap(), b"source");
        }
    }

    mod check_copy_identity {
        use super::*;

        #[test]
        #[expect(
            unsafe_code,
            reason = "invoke the native callback with live borrowed file handles and its exact context"
        )]
        fn callback_compares_the_upper_half_of_the_file_id() {
            let dir = tempfile::tempdir().unwrap();
            let source = File::create(dir.path().join("source")).unwrap();
            let destination = File::create(dir.path().join("destination")).unwrap();
            let mut context = CopyIdentity {
                source: identity(&source).unwrap(),
                destination: identity(&destination).unwrap(),
                checked: AtomicBool::new(false),
                failed: AtomicBool::new(false),
                fail_after_chunk: false,
                copied_data: AtomicBool::new(false),
            };
            context.source.1[15] ^= 1;

            let result = unsafe {
                check_copy_identity(
                    0,
                    0,
                    0,
                    0,
                    1,
                    FileSystem::CALLBACK_STREAM_SWITCH,
                    source.as_raw_handle(),
                    destination.as_raw_handle(),
                    (&raw mut context).cast(),
                )
            };

            assert_eq!(result, FileSystem::PROGRESS_CANCEL);
            assert!(
                context.checked.load(Ordering::Relaxed) && context.failed.load(Ordering::Relaxed)
            );
        }
    }

    mod copy {
        use super::*;

        #[test]
        fn native_copy_preserves_application_metadata_while_the_stage_name_is_pinned() {
            for content in [b"".as_slice(), b"original"] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("source");
                fs::write(&path, content).unwrap();
                fs::write(stream_path(&path, "application"), b"\x00\xff\x01").unwrap();
                fs::write(stream_path(&path, "empty"), b"").unwrap();
                let source = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                extended::set(&source, b"OX.TEST", b"\x00\xff").unwrap();
                set_attributes(
                    &source,
                    PRESERVED_ATTRIBUTES & !FileSystem::FILE_ATTRIBUTE_READONLY,
                )
                .unwrap();
                let snapshot = Security::capture(&source).unwrap();
                let staged = TemporaryFile::staged(dir.path()).unwrap();
                let victim = dir.path().join("victim");
                fs::write(&victim, b"victim").unwrap();

                assert!(fs::rename(&staged.path, dir.path().join("moved")).is_err());
                assert!(fs::rename(&victim, &staged.path).is_err());
                copy(&source, &path, &staged.file, &staged.path).unwrap();
                snapshot.apply(&staged.file).unwrap();

                assert_eq!(Security::capture(&staged.file).unwrap(), snapshot);
                assert_eq!(fs::read(&victim).unwrap(), b"victim");
                assert_eq!(
                    fs::read(stream_path(&staged.path, "application")).unwrap(),
                    b"\x00\xff\x01"
                );
                assert_eq!(fs::read(stream_path(&staged.path, "empty")).unwrap(), b"");
                let stage_name = staged.path.to_path_buf();
                drop(staged);
                assert!(!stage_name.exists());

                write(&path, b"changed", Some(content)).unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"changed");
                assert_eq!(
                    Security::capture(&File::open(&path).unwrap()).unwrap(),
                    snapshot
                );
            }
        }

        #[test]
        fn a_partial_native_copy_failure_keeps_the_original_and_cleans_up() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file");
            let content = vec![b'x'; 1024 * 1024];
            fs::write(&path, &content).unwrap();
            let source = File::open(&path).unwrap();
            let staged = TemporaryFile::staged(dir.path()).unwrap();
            let stage_name = staged.path.to_path_buf();
            let context = CopyIdentity {
                source: identity(&source).unwrap(),
                destination: identity(&staged.file).unwrap(),
                checked: AtomicBool::new(false),
                failed: AtomicBool::new(false),
                fail_after_chunk: true,
                copied_data: AtomicBool::new(false),
            };

            assert!(copy_with(&path, &staged.path, &context).is_err());
            drop(staged);

            assert!(context.checked.load(Ordering::Relaxed));
            assert!(context.copied_data.load(Ordering::Relaxed));
            assert_eq!(fs::read(&path).unwrap(), content);
            assert_eq!(
                identity(&File::open(&path).unwrap()).unwrap(),
                identity(&source).unwrap()
            );
            assert!(!stage_name.exists());
        }

        #[test]
        fn copying_from_a_replaced_name_refuses_before_publication() {
            let dir = tempfile::tempdir().unwrap();
            let original = dir.path().join("original");
            let substituted = dir.path().join("substituted");
            fs::write(&original, b"original").unwrap();
            fs::write(&substituted, b"substituted").unwrap();
            let source = File::open(&original).unwrap();
            let staged = TemporaryFile::staged(dir.path()).unwrap();
            let stage_name = staged.path.to_path_buf();

            let error = copy(&source, &substituted, &staged.file, &staged.path).unwrap_err();
            drop(staged);

            assert!(error.to_string().contains("validate"), "{error}");
            assert!(!stage_name.exists());
            assert_eq!(fs::read(&original).unwrap(), b"original");
            assert_eq!(fs::read(&substituted).unwrap(), b"substituted");
        }
    }

    mod check_access_classes {
        use super::*;

        #[test]
        fn ordinary_read_control_can_query_every_required_class() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, b"ordinary").unwrap();
            let file = fs::OpenOptions::new()
                .access_mode(FileSystem::READ_CONTROL)
                .open(path)
                .unwrap();

            check_access_classes(|information| access_entries(&file, information)).unwrap();
        }

        #[test]
        fn unsupported_state_and_failed_queries_refuse_each_class() {
            for (information, name) in [
                (
                    WinSecurity::ATTRIBUTE_SECURITY_INFORMATION,
                    "resource attributes",
                ),
                (
                    WinSecurity::SCOPE_SECURITY_INFORMATION,
                    "central access policy",
                ),
                (
                    SystemServices::PROCESS_TRUST_LABEL_SECURITY_INFORMATION.cast_unsigned(),
                    "process trust label",
                ),
                (
                    SystemServices::ACCESS_FILTER_SECURITY_INFORMATION.cast_unsigned(),
                    "access filter",
                ),
            ] {
                for query_fails in [false, true] {
                    let error = check_access_classes(|queried| {
                        if queried != information {
                            return Ok(0);
                        }
                        if query_fails {
                            Err(io::Error::from_raw_os_error(
                                Foundation::ERROR_INVALID_PARAMETER.cast_signed(),
                            ))
                        } else {
                            Ok(1)
                        }
                    })
                    .unwrap_err();

                    assert!(error.to_string().contains(name), "{error}");
                    if query_fails {
                        assert!(error.to_string().contains("failed to inspect"), "{error}");
                    } else {
                        assert!(
                            error.to_string().contains("unsupported access metadata"),
                            "{error}"
                        );
                    }
                }
            }
        }
    }

    mod write {
        use super::*;

        #[test]
        fn long_and_native_paths_create_and_replace() {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir
                .path()
                .join("segment".repeat(15))
                .join("segment".repeat(15))
                .join("segment".repeat(15));
            fs::create_dir_all(&parent).unwrap();
            let name = std::ffi::OsString::from_wide(&[0xD800, 0xD83D, 0xDCC4]);
            let path = parent.join(name);
            assert!(path.as_os_str().encode_wide().count() > 260);

            write(&path, b"original", None).unwrap();
            write(&path, b"changed", Some(b"original")).unwrap();

            assert_eq!(fs::read(&path).unwrap(), b"changed");
        }

        #[test]
        fn compression_and_protected_access_survive_replacement() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("compressed");
            let content = b"compressible\n".repeat(4096);
            fs::write(&path, &content).unwrap();
            let source = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            set_compression(&source, FileSystem::COMPRESSION_FORMAT_LZNT1).unwrap();
            let mut policy = access(&source).unwrap();
            policy.control |= WinSecurity::SE_DACL_PROTECTED;
            set_access(&source, &policy).unwrap();
            let snapshot = Security::capture(&source).unwrap();
            assert_eq!(snapshot.compression, FileSystem::COMPRESSION_FORMAT_LZNT1);

            write(&path, b"new contents", Some(&content)).unwrap();

            assert_eq!(
                Security::capture(&File::open(&path).unwrap()).unwrap(),
                snapshot
            );
            assert_eq!(fs::read(&path).unwrap(), b"new contents");
        }

        #[test]
        fn new_files_receive_the_parents_normal_access_policy() {
            let dir = tempfile::tempdir().unwrap();
            let ordinary = dir.path().join("ordinary");
            let atomic = dir.path().join("atomic");
            fs::write(&ordinary, b"").unwrap();
            let expected = access(&File::open(&ordinary).unwrap()).unwrap();

            write(&atomic, b"content", None).unwrap();

            assert_eq!(access(&File::open(&atomic).unwrap()).unwrap(), expected);
        }

        #[test]
        fn new_file_probes_are_removed_under_inherited_delete_denial() {
            for fail in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("target");
                let ordinary = dir.path().join("ordinary");
                let parent = fs::OpenOptions::new()
                    .access_mode(Foundation::GENERIC_READ | FileSystem::WRITE_DAC)
                    .custom_flags(FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                    .open(dir.path())
                    .unwrap();
                let original_parent = access(&parent).unwrap();
                let mut policy = original_parent.clone();
                // cspell:ignore OICI OIIO
                policy.dacl = descriptor_acl(
                    "D:P(D;;0x00000040;;;WD)(D;OIIO;0x00010000;;;WD)(A;OICI;FA;;;OW)",
                    false,
                );
                policy.control |= WinSecurity::SE_DACL_PROTECTED;
                set_access(&parent, &policy).unwrap();
                let reference = fs::OpenOptions::new()
                    .create_new(true)
                    .access_mode(
                        Foundation::GENERIC_READ
                            | Foundation::GENERIC_WRITE
                            | FileSystem::WRITE_DAC,
                    )
                    .open(&ordinary)
                    .unwrap();
                let expected = access(&reference).unwrap();
                assert!(fs::remove_file(&ordinary).is_err());
                let mut removable = expected.clone();
                removable.dacl = None;
                removable.control |= WinSecurity::SE_DACL_PROTECTED;
                set_access(&reference, &removable).unwrap();
                drop(reference);
                fs::remove_file(&ordinary).unwrap();

                let result = write_with(&path, b"content", None, |phase, _| {
                    if phase == Phase::Prepared {
                        let probe = fs::read_dir(dir.path())?
                            .map(|entry| entry.unwrap().path())
                            .find(|path| {
                                path.file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .starts_with(".ox-mode.")
                            })
                            .unwrap();
                        assert_eq!(fs::read(&probe).unwrap(), b"");
                        assert_eq!(access(&File::open(probe).unwrap()).unwrap(), expected);
                    }
                    if fail && phase == Phase::Metadata {
                        return Err(io::Error::other("injected new-file preparation failure"));
                    }
                    Ok(())
                });
                let names: Vec<_> = fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                let actual = path.exists().then(|| {
                    (
                        fs::read(&path).unwrap(),
                        access(&File::open(&path).unwrap()).unwrap(),
                    )
                });
                set_access(&parent, &original_parent).unwrap();

                if fail {
                    let error = result.unwrap_err();
                    assert!(error.to_string().contains("injected new-file"), "{error}");
                    assert!(names.is_empty(), "{names:?}");
                    assert!(actual.is_none());
                } else {
                    result.unwrap();
                    assert_eq!(names, [std::ffi::OsString::from("target")]);
                    assert_eq!(actual.unwrap(), (b"content".to_vec(), expected));
                }
            }
        }

        #[test]
        fn preserves_a_mandatory_integrity_label() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file");
            let mut file = create_private(&path).unwrap();
            file.write_all(b"original").unwrap();
            let mut desired = access(&file).unwrap();
            desired.label = descriptor_acl("S:(ML;;NW;;;LW)", true);
            set_access(&file, &desired).unwrap();
            let before = access(&file).unwrap();
            drop(file);

            write(&path, b"changed", Some(b"original")).unwrap();

            assert_eq!(access(&File::open(&path).unwrap()).unwrap(), before);
            assert_eq!(fs::read(&path).unwrap(), b"changed");
        }

        #[test]
        fn staged_ea_and_stream_changes_refuse_publication() {
            for change_ea in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("source");
                fs::write(&path, b"original").unwrap();
                fs::write(stream_path(&path, "application"), b"old").unwrap();
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                extended::set(&file, b"OX.TEST", b"old").unwrap();
                let before = Security::capture(&file).unwrap();
                let id = identity(&file).unwrap();

                let error = write_with(&path, b"ours", Some(b"original"), |phase, staged| {
                    if phase == Phase::Validated {
                        if change_ea {
                            extended::set(staged, b"OX.TEST", b"new")?;
                        } else {
                            fs::write(stream_path(&opened_path(staged)?, "application"), b"new")?;
                        }
                    }
                    Ok(())
                })
                .unwrap_err();

                assert!(error.to_string().contains("metadata"), "{error}");
                assert_eq!(identity(&File::open(&path).unwrap()).unwrap(), id);
                assert_eq!(Security::capture(&file).unwrap(), before);
                assert_eq!(fs::read(&path).unwrap(), b"original");
                let names: Vec<_> = fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                assert_eq!(names, [std::ffi::OsString::from("source")]);
            }
        }

        #[test]
        fn ea_and_stream_only_changes_refuse_publication() {
            for change_ea in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("source");
                fs::write(&path, b"original").unwrap();
                fs::write(stream_path(&path, "application"), b"old").unwrap();
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                extended::set(&file, b"OX.TEST", b"old").unwrap();
                let before = identity(&file).unwrap();

                let result = write_with(&path, b"ours", Some(b"original"), |phase, _| {
                    if phase == Phase::Synced {
                        if change_ea {
                            extended::set(&file, b"OX.TEST", b"new")?;
                        } else {
                            fs::write(stream_path(&path, "application"), b"new")?;
                        }
                    }
                    Ok(())
                });

                assert!(result.is_err(), "change_ea={change_ea}");
                assert_eq!(identity(&File::open(&path).unwrap()).unwrap(), before);
                assert_eq!(fs::read(&path).unwrap(), b"original");
                if change_ea {
                    assert_eq!(
                        extended::capture(&file).unwrap()[b"OX.TEST".as_slice()].1,
                        b"new"
                    );
                } else {
                    assert_eq!(fs::read(stream_path(&path, "application")).unwrap(), b"new");
                }
            }
        }

        #[test]
        fn resource_access_metadata_refuses_initial_and_final_capture() {
            for (phase, staged_target) in [
                (None, false),
                (Some(Phase::Synced), false),
                (Some(Phase::Validated), true),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("source");
                fs::write(&path, b"original").unwrap();
                let file = fs::OpenOptions::new()
                    .access_mode(
                        Foundation::GENERIC_READ
                            | Foundation::GENERIC_WRITE
                            | FileSystem::WRITE_DAC,
                    )
                    .open(&path)
                    .unwrap();
                let before = identity(&file).unwrap();
                if phase.is_none() {
                    set_resource_attributes(&file).unwrap();
                }

                let error = write_with(&path, b"ours", Some(b"original"), |at, staged| {
                    if Some(at) == phase {
                        set_resource_attributes(if staged_target { staged } else { &file })?;
                    }
                    Ok(())
                })
                .unwrap_err();

                assert!(error.to_string().contains("resource attributes"), "{error}");
                assert_eq!(identity(&File::open(&path).unwrap()).unwrap(), before);
                assert_eq!(fs::read(&path).unwrap(), b"original");
                let names: Vec<_> = fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                assert_eq!(names, [std::ffi::OsString::from("source")]);
            }
        }

        #[test]
        fn failure_disposes_the_stage_despite_restored_delete_restrictions() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source");
            fs::write(&path, b"original").unwrap();
            let parent = fs::OpenOptions::new()
                .access_mode(Foundation::GENERIC_READ | FileSystem::WRITE_DAC)
                .custom_flags(FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(dir.path())
                .unwrap();
            let file = fs::OpenOptions::new()
                .access_mode(
                    Foundation::GENERIC_READ | Foundation::GENERIC_WRITE | FileSystem::WRITE_DAC,
                )
                .open(&path)
                .unwrap();
            let original_parent = access(&parent).unwrap();
            let original_file = access(&file).unwrap();
            let before = identity(&file).unwrap();
            let mut parent_policy = original_parent.clone();
            parent_policy.dacl = descriptor_acl("D:P(D;;0x00000040;;;WD)(A;;FA;;;OW)", false);
            parent_policy.control |= WinSecurity::SE_DACL_PROTECTED;
            set_access(&parent, &parent_policy).unwrap();
            let mut file_policy = original_file.clone();
            file_policy.dacl = descriptor_acl("D:P(D;;0x00010000;;;WD)(A;;FA;;;OW)", false);
            file_policy.control |= WinSecurity::SE_DACL_PROTECTED;
            set_access(&file, &file_policy).unwrap();
            assert!(fs::remove_file(&path).is_err());

            let error = write_with(&path, b"ours", Some(b"original"), |phase, _| {
                if phase == Phase::Metadata {
                    return Err(io::Error::other(
                        "injected failure after access restoration",
                    ));
                }
                Ok(())
            })
            .unwrap_err();
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            set_access(&file, &original_file).unwrap();
            set_access(&parent, &original_parent).unwrap();

            assert!(error.to_string().contains("injected failure"), "{error}");
            assert_eq!(names, [std::ffi::OsString::from("source")]);
            assert_eq!(identity(&File::open(&path).unwrap()).unwrap(), before);
            assert_eq!(fs::read(&path).unwrap(), b"original");
        }

        #[test]
        fn a_sharing_failure_keeps_the_original_and_cleans_up() {
            use std::os::windows::fs::OpenOptionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, b"original").unwrap();
            let held = fs::OpenOptions::new()
                .read(true)
                .share_mode(FileSystem::FILE_SHARE_READ | FileSystem::FILE_SHARE_WRITE)
                .open(&path)
                .unwrap();
            let before = identity(&held).unwrap();

            assert!(write(&path, b"changed", Some(b"original")).is_err());

            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(identity(&File::open(&path).unwrap()).unwrap(), before);
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, [std::ffi::OsString::from("file")]);
        }
    }
}
