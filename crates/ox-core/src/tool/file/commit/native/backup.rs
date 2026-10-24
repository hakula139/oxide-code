//! Handle-bound transfer of EAs and application streams through native backup records.

#[cfg(windows)]
use std::fs::File;
use std::io::{self, BufRead};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem;

const BUFFER_SIZE: usize = 8192;
const ALTERNATE_DATA: u32 = 4;

#[cfg(windows)]
pub(super) fn copy(source: &File, destination: &File) -> io::Result<()> {
    copy_with(source, destination, || Ok(()))
}

#[cfg(windows)]
pub(super) fn copy_with(
    source: &File,
    destination: &File,
    mut after_write: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Seek as _;

    let external = super::externally_backed(source)?;
    let mut source = source;
    source.rewind()?;
    let mut reader =
        io::BufReader::with_capacity(BUFFER_SIZE, Reader(Context::new(source, Direction::Read)));
    let mut writer = Writer(Context::new(destination, Direction::Write));
    let result = transfer(&mut reader, external, |bytes| {
        writer.write_exact(bytes)?;
        after_write()
    });
    let read_cleanup = reader.into_inner().0.finish();
    let write_cleanup = writer.0.finish();
    result.and(read_cleanup).and(write_cleanup)
}

fn transfer(
    reader: &mut impl BufRead,
    external: bool,
    mut emit: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let provider: Vec<_> = ":WofCompressedData:$DATA"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut owner = None;
    while !reader.fill_buf()?.is_empty() {
        // Serialized headers omit WIN32_STREAM_ID's trailing alignment padding.
        let mut header = [0; 20];
        reader.read_exact(&mut header)?;
        let kind = u32::from_le_bytes(header[..4].try_into().expect("four-byte stream kind"));
        let size = i64::from_le_bytes(header[8..16].try_into().expect("eight-byte stream size"));
        let name_size =
            u32::from_le_bytes(header[16..].try_into().expect("four-byte stream name size"));
        if size < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native metadata stream exceeds the supported signed size",
            ));
        }
        if name_size > 65_536
            || !name_size.is_multiple_of(2)
            || (kind != ALTERNATE_DATA && name_size != 0)
        {
            return Err(invalid());
        }
        let mut name = vec![0; name_size as usize];
        reader.read_exact(&mut name)?;
        let retain = match kind {
            // The caller writes replacement unnamed data after metadata transfer.
            1 => {
                owner = Some(false);
                false
            }
            2 => true, // BACKUP_EA_DATA
            ALTERNATE_DATA => {
                let retain = !external || name != provider;
                owner = Some(retain);
                retain
            }
            // BACKUP_SPARSE_BLOCK
            9 => {
                if size < 8 {
                    return Err(invalid());
                }
                owner.ok_or_else(invalid)?
            }
            3 | 5..=8 | 10 | 11 => false,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported native metadata stream kind",
                ));
            }
        };
        if retain {
            emit(&header)?;
            if !name.is_empty() {
                emit(&name)?;
            }
        }
        payload(reader, size.cast_unsigned(), retain, &mut emit)?;
    }
    Ok(())
}

fn payload(
    reader: &mut impl io::Read,
    mut remaining: u64,
    retain: bool,
    emit: &mut impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut buffer = [0; BUFFER_SIZE];
    while remaining != 0 {
        let count = usize::try_from(remaining.min(BUFFER_SIZE as u64))
            .expect("a buffer-sized payload fits usize");
        reader.read_exact(&mut buffer[..count])?;
        if retain {
            emit(&buffer[..count])?;
        }
        remaining -= count as u64;
    }
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native metadata stream")
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
enum Direction {
    Read,
    Write,
}

#[cfg(windows)]
struct Context<'a> {
    file: &'a File,
    direction: Direction,
    pointer: *mut std::ffi::c_void,
    finished: bool,
}

#[cfg(windows)]
impl<'a> Context<'a> {
    fn new(file: &'a File, direction: Direction) -> Self {
        Self {
            file,
            direction,
            pointer: std::ptr::null_mut(),
            finished: false,
        }
    }

    fn finish(mut self) -> io::Result<()> {
        self.abort()
    }

    #[expect(
        unsafe_code,
        reason = "release the matching opaque backup context exactly once before its borrowed file closes"
    )]
    fn abort(&mut self) -> io::Result<()> {
        self.finished = true;
        // Abort consumes this API-owned context. All parameters except its address are ignored.
        let result = unsafe {
            match self.direction {
                Direction::Read => FileSystem::BackupRead(
                    self.file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    1,
                    0,
                    &raw mut self.pointer,
                ),
                Direction::Write => FileSystem::BackupWrite(
                    self.file.as_raw_handle(),
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    1,
                    0,
                    &raw mut self.pointer,
                ),
            }
        };
        super::windows_result(result)
    }
}

#[cfg(windows)]
impl Drop for Context<'_> {
    fn drop(&mut self) {
        if !self.finished {
            _ = self.abort();
        }
    }
}

#[cfg(windows)]
struct Reader<'a>(Context<'a>);

#[cfg(windows)]
impl io::Read for Reader<'_> {
    #[expect(
        unsafe_code,
        reason = "read backup records from a synchronous borrowed handle into the supplied buffer"
    )]
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let capacity = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        let mut count = 0;
        // BufReader supplies an 8 KiB buffer. The context and output remain live until completion.
        super::windows_result(unsafe {
            FileSystem::BackupRead(
                self.0.file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                &raw mut count,
                0,
                0,
                &raw mut self.0.pointer,
            )
        })?;
        if count > capacity {
            return Err(invalid());
        }
        Ok(count as usize)
    }
}

#[cfg(windows)]
struct Writer<'a>(Context<'a>);

#[cfg(windows)]
impl Writer<'_> {
    #[expect(
        unsafe_code,
        reason = "restore one exact backup fragment through the pinned synchronous destination handle"
    )]
    fn write_exact(&mut self, bytes: &[u8]) -> io::Result<()> {
        let size = u32::try_from(bytes.len()).map_err(io::Error::other)?;
        let mut count = 0;
        // Only EA/ADS framing reaches this context. Security processing stays disabled.
        super::windows_result(unsafe {
            FileSystem::BackupWrite(
                self.0.file.as_raw_handle(),
                bytes.as_ptr(),
                size,
                &raw mut count,
                0,
                0,
                &raw mut self.0.pointer,
            )
        })?;
        if count != size {
            return Err(io::Error::other(
                "native metadata write did not consume its complete buffer",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod transfer {
        use super::*;

        fn record(kind: u32, name: &[u16], bytes: &[u8]) -> Vec<u8> {
            [
                kind.to_le_bytes().to_vec(),
                0_u32.to_le_bytes().to_vec(),
                (bytes.len() as u64).to_le_bytes().to_vec(),
                u32::try_from(name.len() * 2)
                    .expect("fixture names fit u32")
                    .to_le_bytes()
                    .to_vec(),
                name.iter().flat_map(|unit| unit.to_le_bytes()).collect(),
                bytes.to_vec(),
            ]
            .concat()
        }

        #[test]
        fn preserves_application_records_and_their_sparse_blocks() {
            let ea = record(2, &[], b"extended");
            let application = record(4, &[u16::from(b':'), 0xD800], b"short");
            let empty = record(4, &[u16::from(b':'), u16::from(b'e')], b"");
            let sparse = record(9, &[], &[7; 11]);
            let unnamed = record(1, &[], b"original");
            let provider = record(
                4,
                &":WofCompressedData:$DATA"
                    .encode_utf16()
                    .collect::<Vec<_>>(),
                b"storage",
            );
            let opaque: Vec<_> = [3, 5, 6, 7, 8, 10, 11]
                .into_iter()
                .flat_map(|kind| record(kind, &[], b"opaque"))
                .collect();
            let input = [
                application.clone(),
                ea.clone(),
                sparse.clone(),
                unnamed,
                sparse.clone(),
                provider.clone(),
                sparse.clone(),
                empty.clone(),
                sparse.clone(),
                opaque,
            ]
            .concat();

            for external in [false, true] {
                let mut output = Vec::new();
                transfer(&mut io::Cursor::new(&input), external, |bytes| {
                    output.extend_from_slice(bytes);
                    Ok(())
                })
                .unwrap();

                let mut expected = [application.clone(), ea.clone(), sparse.clone()].concat();
                if !external {
                    expected.extend_from_slice(&provider);
                    expected.extend_from_slice(&sparse);
                }
                expected.extend_from_slice(&empty);
                expected.extend_from_slice(&sparse);
                assert_eq!(output, expected, "external={external}");
            }
        }

        #[test]
        fn preserves_payloads_across_short_reads_and_buffer_boundaries() {
            struct ShortReads(io::Cursor<Vec<u8>>);

            impl io::Read for ShortReads {
                fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                    let count = bytes.len().min(3);
                    self.0.read(&mut bytes[..count])
                }
            }

            let payload: Vec<_> = (0..=255).cycle().take(BUFFER_SIZE + 1).collect();
            let expected = record(4, &[0xD800], &payload);
            let mut input = io::BufReader::new(ShortReads(io::Cursor::new(expected.clone())));
            let mut actual = Vec::new();

            transfer(&mut input, false, |bytes| {
                actual.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();

            assert_eq!(actual, expected);
        }

        #[test]
        fn rejects_malformed_or_unsupported_records_and_incomplete_payloads() {
            let valid = record(4, &[0xD800, 0xDFFF], b"payload");
            let mut odd_name = valid.clone();
            odd_name[16..20].copy_from_slice(&3_u32.to_le_bytes());
            let mut long_name = valid.clone();
            long_name[16..20].copy_from_slice(&65_538_u32.to_le_bytes());
            let mut large_payload = valid.clone();
            large_payload[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
            for (name, input) in [
                ("unknown", record(12, &[], b"")),
                ("orphan sparse", record(9, &[], &[0; 8])),
                (
                    "short sparse",
                    [record(4, &[], b""), record(9, &[], &[0; 7])].concat(),
                ),
                ("named ordinary", record(1, &[1], b"")),
                ("odd name", odd_name),
                ("long name", long_name),
                ("large payload", large_payload),
                ("header", valid[..19].to_vec()),
                ("name", valid[..21].to_vec()),
                ("payload", valid[..valid.len() - 1].to_vec()),
            ] {
                let result = transfer(&mut io::Cursor::new(input), false, |_| Ok(()));

                assert!(result.is_err(), "{name}");
            }
        }

        #[test]
        fn returns_read_errors_during_payload_transfer() {
            use std::io::Read as _;

            struct Failed;

            impl io::Read for Failed {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    Err(io::Error::other("injected input failure"))
                }
            }

            let record = record(2, &[], b"payload");
            let mut input = io::BufReader::new(io::Cursor::new(&record[..23]).chain(Failed));
            let mut output = Vec::new();

            let error = transfer(&mut input, false, |bytes| {
                output.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap_err();

            assert_eq!(error.to_string(), "injected input failure");
            assert_eq!(output, record[..20]);
        }

        #[test]
        fn returns_a_partial_transfer_error_without_reading_later_records() {
            let first = record(4, &[u16::from(b'a')], b"first");
            let later = record(12, &[], b"later");
            let mut input = io::Cursor::new([first.clone(), later.clone()].concat());
            let mut emitted = Vec::new();

            let error = transfer(&mut input, false, |bytes| {
                emitted.extend_from_slice(bytes);
                Err(io::Error::other("injected output failure"))
            })
            .unwrap_err();

            assert_eq!(error.to_string(), "injected output failure");
            assert_eq!(emitted, first[..20]);
            assert_eq!(
                input.fill_buf().unwrap(),
                [b"first".to_vec(), later].concat()
            );
        }
    }
}
