//! File access for the file tools: target validation, reads checked on the opened file, and writes
//! that never follow a symlink at the final component.

use std::fs::{File, Metadata, OpenOptions};
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::ToolCtx;
use crate::file_tracker::{TrackableFileError, validate_trackable_file};
use crate::util::path::lexical_normalize;

// ── Tracker Keys ──

/// The [`FileTracker`](crate::file_tracker::FileTracker) key for the absolute `path` a file tool
/// acts on. An existing path canonicalizes, so dotted and symlinked spellings of one file share an
/// entry. A missing path falls back to its lexically clean form.
pub(super) async fn tracker_key(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path).await.map_or_else(
        |_| lexical_normalize(path),
        |path| dunce::simplified(&path).to_path_buf(),
    )
}

// ── File Target Validation ──

/// Rejects targets the [`FileTracker`](crate::file_tracker::FileTracker) cannot gate, phrasing each
/// refusal for `verb` (`read`, `edit`, `write`).
pub(super) fn validate_file_target(
    path: &str,
    metadata: &Metadata,
    verb: &str,
) -> Result<(), String> {
    match validate_trackable_file(metadata) {
        Ok(()) => Ok(()),
        Err(TrackableFileError::Directory) if verb == "read" => Err(format!(
            "{path} is a directory. Use the glob tool to list directory contents."
        )),
        Err(TrackableFileError::Directory) => {
            Err(format!("{path} is a directory. Refusing to {verb}."))
        }
        Err(TrackableFileError::NonRegular) => Err(format!(
            "{path} is not a regular file (fifo, socket, or device). Refusing to {verb}.",
        )),
        Err(TrackableFileError::TooLarge { size, max }) => {
            let mb = bytes_to_mb(size);
            let limit_mb = max / (1024 * 1024);
            Err(format!(
                "File is too large ({mb:.1} MB, max {limit_mb} MB). \
                 Use the bash tool to {verb} large files.",
            ))
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "MB display loses precision only above 2^53 bytes, far beyond any file size cap"
)]
pub(super) fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// ── Binary Detection ──

const BINARY_CHECK_SIZE: usize = 8192;

/// Returns whether `bytes` hold a NUL in the first 8 KiB, which Git takes to mean binary.
pub(super) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

// ── File Reading ──

/// Reads `path` once `validate` accepts its metadata, checked by path before the open and again on
/// the opened file, so a file swapped in between cannot skip the check. The open refuses a symlink
/// at the final component and never blocks on a fifo, as [`open_no_follow`] describes. Errors name
/// the file as `shown`.
pub(super) async fn read_validated(
    path: &Path,
    shown: &str,
    validate: impl Fn(&Metadata) -> Result<(), String>,
) -> Result<(Metadata, Vec<u8>), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Failed to access {shown}: {e}"))?;
    validate(&metadata)?;

    let error = |e: std::io::Error| format!("Failed to read {shown}: {e}");
    let mut options = OpenOptions::new();
    options.read(true);
    let mut file = open_no_follow(path, options).await.map_err(error)?;
    let metadata = file.metadata().await.map_err(error)?;
    validate(&metadata)?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.map_err(error)?;
    Ok((metadata, bytes))
}

// ── File Writing ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteMode {
    CreateNew,
    Overwrite,
}

/// Writes `bytes` through [`open_no_follow`], refusing existing entries in `CreateNew` mode and
/// truncating only after a successful open in `Overwrite` mode.
async fn write_no_follow(path: &Path, bytes: &[u8], mode: WriteMode) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(matches!(mode, WriteMode::CreateNew));
    let mut file = open_no_follow(path, options).await?;

    if matches!(mode, WriteMode::Overwrite) {
        file.set_len(0).await?;
    }
    file.write_all(bytes).await?;
    file.flush().await
}

/// Writes `bytes` to the absolute `path` like [`write_no_follow`] and records them in the context's
/// tracker under the path's [`tracker_key`] as one unit, so a caller dropped mid-write cannot leave
/// the file written but unrecorded.
pub(super) async fn write_tracked(
    ctx: &ToolCtx,
    path: &Path,
    bytes: Vec<u8>,
    mode: WriteMode,
) -> std::io::Result<()> {
    let path = path.to_owned();
    ctx.tracker
        .write_and_record(&ctx.tool_use_id, async move {
            write_no_follow(&path, &bytes, mode).await?;
            Ok((tracker_key(&path).await, bytes))
        })
        .await
}

// ── No-Follow Open ──

/// Opens `path` without following a symlink at the final component, so a link swapped in after the
/// gate resolved the path cannot redirect the call, and without blocking on a fifo.
async fn open_no_follow(path: &Path, options: OpenOptions) -> std::io::Result<tokio::fs::File> {
    let path = path.to_owned();
    let file =
        tokio::task::spawn_blocking(move || open_no_follow_blocking(&path, options)).await??;
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(unix)]
pub(super) fn open_no_follow_blocking(
    path: &Path,
    mut options: OpenOptions,
) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    use nix::fcntl::OFlag;

    options
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
}

/// `FILE_FLAG_OPEN_REPARSE_POINT` opens a reparse point itself, so a symlink or junction there
/// fails the open as `O_NOFOLLOW` makes it fail on Unix. Any other reparse point, such as a
/// deduplicated file, reads only through the file system filter the flag bypasses, so it opens
/// again without the flag and keeps that handle only when it reaches the same file.
#[cfg(windows)]
pub(super) fn open_no_follow_blocking(
    path: &Path,
    mut options: OpenOptions,
) -> std::io::Result<File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let file = options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(file);
    }
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "the final path component is a symlink",
        ));
    }

    let reopened = options.custom_flags(0).open(path)?;
    if file_id(&reopened)? == file_id(&file)? {
        Ok(reopened)
    } else {
        Err(std::io::Error::other(
            "the file changed while it was opened",
        ))
    }
}

/// The volume and file index that identify a file across handles.
#[cfg(windows)]
fn file_id(file: &File) -> std::io::Result<(u64, u64)> {
    let info = winapi_util::file::information(file)?;
    Ok((info.volume_serial_number(), info.file_index()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tool::local_testing::canonical_tempdir;
    use crate::util::fs::testing::symlink_file;

    mod tracker_key {
        use super::*;

        #[tokio::test]
        async fn resolves_dotted_spellings_to_one_key() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("src")).unwrap();
            let file = dir.path().join("src/main.rs");
            fs::write(&file, "").unwrap();

            assert_eq!(tracker_key(&file).await, file);
            assert_eq!(
                tracker_key(&dir.path().join("./src/../src/main.rs")).await,
                file
            );
        }

        #[tokio::test]
        async fn missing_path_falls_back_to_its_lexically_clean_form() {
            let dir = canonical_tempdir();
            assert_eq!(
                tracker_key(&dir.path().join("missing/../a.rs")).await,
                dir.path().join("a.rs")
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn resolves_a_symlinked_spelling_to_the_target() {
            let dir = canonical_tempdir();
            fs::create_dir(dir.path().join("real")).unwrap();
            fs::write(dir.path().join("real/a.rs"), "").unwrap();
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

            assert_eq!(
                tracker_key(&dir.path().join("link/a.rs")).await,
                dir.path().join("real/a.rs"),
            );
        }
    }

    mod read_validated {
        use super::*;

        #[tokio::test]
        async fn reads_a_regular_file_but_refuses_a_symlink() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.txt");
            fs::write(&target, "original").unwrap();
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);

            let accept = |_: &Metadata| Ok(());
            let (metadata, bytes) = read_validated(&target, "target.txt", accept).await.unwrap();
            assert_eq!(
                (metadata.len(), bytes.as_slice()),
                (8, b"original".as_slice())
            );
            let err = read_validated(&link, "link.txt", accept).await.unwrap_err();
            #[cfg(unix)]
            let cause = std::io::Error::from_raw_os_error(nix::errno::Errno::ELOOP as i32);
            #[cfg(windows)]
            let cause = "the final path component is a symlink";
            assert_eq!(err, format!("Failed to read link.txt: {cause}"));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn rechecks_a_file_swapped_after_the_path_check() {
            // Each swap runs once the path check passes, as a racing writer's would: a larger file
            // the size limit must catch, then a fifo the open must not block on.
            fn grow(path: &Path) {
                let staged = path.with_extension("new");
                fs::write(&staged, "SECRET=1").unwrap();
                fs::rename(&staged, path).unwrap();
            }
            fn fifo(path: &Path) {
                fs::remove_file(path).unwrap();
                nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRWXU).unwrap();
            }

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notes.md");
            for swap in [grow as fn(&Path), fifo] {
                fs::write(&path, "ok").unwrap();
                let swapped = std::cell::Cell::new(false);
                let validate = |meta: &Metadata| {
                    if !meta.is_file() || meta.len() > 4 {
                        return Err("rejected".to_owned());
                    }
                    if !swapped.replace(true) {
                        swap(&path);
                    }
                    Ok(())
                };
                let err = read_validated(&path, "notes.md", validate)
                    .await
                    .unwrap_err();
                assert_eq!(err, "rejected");
            }
        }
    }

    mod write_no_follow {
        use super::*;

        #[tokio::test]
        async fn overwrite_truncates_a_regular_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file.txt");
            fs::write(&path, "a much longer original").unwrap();

            write_no_follow(&path, b"short", WriteMode::Overwrite)
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "short");
        }

        #[tokio::test]
        async fn create_new_refuses_existing_entries_and_dangling_links() {
            let dir = tempfile::tempdir().unwrap();
            let existing = dir.path().join("existing.txt");
            fs::write(&existing, "original").unwrap();
            let target = dir.path().join("target.txt");
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);

            assert!(
                write_no_follow(&existing, b"x", WriteMode::CreateNew)
                    .await
                    .is_err()
            );
            assert_eq!(fs::read_to_string(&existing).unwrap(), "original");
            assert!(
                write_no_follow(&link, b"x", WriteMode::CreateNew)
                    .await
                    .is_err()
            );
            assert!(!target.exists(), "the link target must not be created");
        }

        #[tokio::test]
        async fn overwrite_refuses_a_symlink() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.txt");
            fs::write(&target, "original").unwrap();
            let link = dir.path().join("link.txt");
            symlink_file(&target, &link);

            let err = write_no_follow(&link, b"x", WriteMode::Overwrite)
                .await
                .unwrap_err();
            #[cfg(unix)]
            assert_eq!(err.raw_os_error(), Some(nix::errno::Errno::ELOOP as i32));
            #[cfg(windows)]
            assert_eq!(err.to_string(), "the final path component is a symlink");
            assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn overwrite_refuses_a_fifo_without_blocking() {
            let dir = tempfile::tempdir().unwrap();
            let fifo = dir.path().join("pipe");
            nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

            let write = write_no_follow(&fifo, b"x", WriteMode::Overwrite);
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), write).await;
            assert!(result.expect("the open must not block").is_err());
        }
    }

    #[cfg(windows)]
    mod open_no_follow {
        use super::*;

        #[tokio::test]
        async fn reads_and_rewrites_a_compressed_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notes.txt");
            let content = "a compressible line\n".repeat(4096);
            fs::write(&path, &content).unwrap();
            // `compact /EXE` stores the data compressed behind the WOF filter, which hides the
            // file's reparse point from user mode, so the no-follow open keeps its first handle.
            let compact = std::process::Command::new("compact.exe")
                .args(["/C", "/EXE:XPRESS4K"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                compact.status.success(),
                "{}",
                String::from_utf8_lossy(&compact.stdout)
            );

            let accept = |_: &Metadata| Ok(());
            let (_, bytes) = read_validated(&path, "notes.txt", accept).await.unwrap();
            assert_eq!(bytes, content.as_bytes());
            write_no_follow(&path, b"rewritten", WriteMode::Overwrite)
                .await
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"rewritten");
        }
    }
}
