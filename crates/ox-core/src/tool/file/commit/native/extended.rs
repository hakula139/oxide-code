//! Visible native extended attributes, excluding record offsets and padding from comparisons.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};

use windows_sys::Wdk::Storage::FileSystem::NtQueryEaFile;
use windows_sys::Win32::{Foundation, Storage::FileSystem, System::IO::IO_STATUS_BLOCK};

pub(super) type Snapshot = BTreeMap<Vec<u8>, (u8, Vec<u8>)>;

#[expect(
    unsafe_code,
    reason = "reopen the same file synchronously so native EA queries cannot outlive stack outputs"
)]
fn reopen(file: &File, access: u32) -> io::Result<File> {
    // ReOpenFile binds to the existing object. Omitting OVERLAPPED gives synchronous I/O.
    let handle = unsafe {
        FileSystem::ReOpenFile(
            file.as_raw_handle(),
            access | FileSystem::SYNCHRONIZE,
            FileSystem::FILE_SHARE_READ
                | FileSystem::FILE_SHARE_WRITE
                | FileSystem::FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == Foundation::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // ReOpenFile created an independent handle, so this guard cannot close the caller's file.
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[expect(
    unsafe_code,
    reason = "query native EA records through a synchronous handle into an aligned owned buffer"
)]
pub(super) fn capture(file: &File) -> io::Result<Snapshot> {
    let file = reopen(file, FileSystem::FILE_READ_EA)?;
    let mut buffer = vec![0_u64; 512];
    loop {
        let capacity =
            u32::try_from(std::mem::size_of_val(buffer.as_slice())).map_err(io::Error::other)?;
        let mut status = IO_STATUS_BLOCK::default();
        // The synchronous file keeps both stack status and buffer alive until the query completes.
        let result = unsafe {
            NtQueryEaFile(
                file.as_raw_handle(),
                &raw mut status,
                buffer.as_mut_ptr().cast(),
                capacity,
                false,
                std::ptr::null(),
                0,
                std::ptr::null(),
                true,
            )
        };
        match result {
            Foundation::STATUS_SUCCESS => {
                let bytes: Vec<_> = buffer
                    .iter()
                    .flat_map(|value| value.to_ne_bytes())
                    .collect();
                let bytes = bytes.get(..status.Information).ok_or_else(invalid)?;
                return parse(bytes);
            }
            Foundation::STATUS_NO_EAS_ON_FILE | Foundation::STATUS_EAS_NOT_SUPPORTED => {
                return Ok(Snapshot::new());
            }
            Foundation::STATUS_BUFFER_OVERFLOW | Foundation::STATUS_BUFFER_TOO_SMALL => {
                buffer.resize(buffer.len() * 2, 0)
            }
            _ => {
                let error = unsafe { Foundation::RtlNtStatusToDosError(result) };
                return Err(io::Error::from_raw_os_error(error.cast_signed()));
            }
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid native extended attributes",
    )
}

fn parse(bytes: &[u8]) -> io::Result<Snapshot> {
    let mut attributes = Snapshot::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset + 8).ok_or_else(invalid)?;
        let next = u32::from_ne_bytes(header[..4].try_into().map_err(|_| invalid())?) as usize;
        let flags = header[4];
        let name_length = usize::from(header[5]);
        let value_length = usize::from(u16::from_ne_bytes([header[6], header[7]]));
        let name_start = offset + 8;
        let name_end = name_start + name_length;
        let value_end = name_end + 1 + value_length;
        let name = bytes.get(name_start..name_end).ok_or_else(invalid)?;
        let value = bytes.get(name_end + 1..value_end).ok_or_else(invalid)?;
        if bytes.get(name_end) != Some(&0)
            || attributes
                .insert(name.to_vec(), (flags, value.to_vec()))
                .is_some()
        {
            return Err(invalid());
        }
        if next == 0 {
            break;
        }
        if next < value_end - offset || next >= bytes.len() - offset {
            return Err(invalid());
        }
        offset = offset.checked_add(next).ok_or_else(invalid)?;
    }
    Ok(attributes)
}

#[cfg(test)]
#[expect(
    unsafe_code,
    reason = "create an EA fixture through a synchronous handle with a sized native record"
)]
pub(super) fn set(file: &File, name: &[u8], value: &[u8]) -> io::Result<()> {
    use windows_sys::Wdk::Storage::FileSystem::NtSetEaFile;

    let file = reopen(file, FileSystem::FILE_WRITE_EA)?;
    let mut bytes = vec![0; 4];
    bytes.push(0);
    bytes.push(u8::try_from(name.len()).map_err(io::Error::other)?);
    bytes.extend(
        u16::try_from(value.len())
            .map_err(io::Error::other)?
            .to_ne_bytes(),
    );
    bytes.extend(name);
    bytes.push(0);
    bytes.extend(value);
    let mut buffer = vec![0_u64; bytes.len().div_ceil(8)];
    let mut status = IO_STATUS_BLOCK::default();
    let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    // The aligned buffer contains the full EA record and remains live through synchronous completion.
    let result = unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.as_mut_ptr().cast(), bytes.len());
        NtSetEaFile(
            file.as_raw_handle(),
            &raw mut status,
            buffer.as_ptr().cast(),
            length,
        )
    };
    if result == Foundation::STATUS_SUCCESS {
        return Ok(());
    }
    let error = unsafe { Foundation::RtlNtStatusToDosError(result) };
    Err(io::Error::from_raw_os_error(error.cast_signed()))
}
