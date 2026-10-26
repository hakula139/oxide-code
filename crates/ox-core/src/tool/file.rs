//! File access for the file tools: target validation, reads checked on the opened file, and writes
//! that never follow a symlink at the final component.

mod commit;

use std::fs::Metadata;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use strum::Display;

use crate::file_tracker::MAX_TRACKED_FILE_SIZE;
use crate::util::fs::{FileReadError, read_regular_file, validate_regular_file};
use crate::util::path::lexical_normalize;
use crate::util::text::format_size;

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

/// What a file tool does with its target, which its refusals name.
#[derive(Debug, Clone, Copy, Display)]
#[strum(serialize_all = "lowercase")]
pub(super) enum FileVerb {
    Read,
    Edit,
    Write,
}

/// Rejects targets the [`FileTracker`](crate::file_tracker::FileTracker) cannot gate, phrasing each
/// refusal for `verb`.
pub(super) fn validate_file_target(
    path: &Path,
    metadata: &Metadata,
    verb: FileVerb,
) -> Result<(), String> {
    validate_regular_file(metadata, MAX_TRACKED_FILE_SIZE)
        .map_err(|error| file_read_error(path, error, verb))
}

fn file_read_error(path: &Path, error: FileReadError, verb: FileVerb) -> String {
    match error {
        FileReadError::Io(error) => format!("Failed to read {}: {error}", path.display()),
        FileReadError::Directory => match verb {
            FileVerb::Read => format!(
                "{} is a directory. Use the glob tool to list directory contents.",
                path.display()
            ),
            FileVerb::Edit | FileVerb::Write => {
                format!("{} is a directory. Refusing to {verb}.", path.display())
            }
        },
        FileReadError::NonRegular => format!(
            "{} is not a regular file (fifo, socket, or device). Refusing to {verb}.",
            path.display()
        ),
        FileReadError::TooLarge { size, max } => {
            let (size, max) = (format_size(size), format_size(max));
            format!(
                "File is too large ({size}, max {max}). Use the bash tool to {verb} large files.",
            )
        }
    }
}

// ── Binary Detection ──

const BINARY_CHECK_SIZE: usize = 8192;

/// Returns whether `bytes` hold a NUL in the first 8 KiB, which Git takes to mean binary.
pub(super) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

// ── File Reading ──

/// Reads a bounded regular file, checking its metadata before opening and through the opened
/// no-follow handle. Errors name `path` and phrase refusals for `verb`.
pub(super) async fn read_validated(
    path: &Path,
    verb: FileVerb,
) -> Result<(Metadata, Vec<u8>), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Failed to access {}: {e}", path.display()))?;
    validate_file_target(path, &metadata, verb)?;

    let shown = path.display().to_string();
    let read_path = path.to_owned();
    tokio::task::spawn_blocking(move || read_regular_file(&read_path, MAX_TRACKED_FILE_SIZE))
        .await
        .map_err(|error| format!("Failed to read {shown}: {error}"))?
        .map_err(|error| file_read_error(path, error, verb))
}

// ── File Writing ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteMode {
    CreateNew,
    Overwrite,
}

/// Atomically publishes `bytes` after checking the original `expected` bytes and metadata.
/// `None` requires a new path. The caller must use the tracker's owned write operation.
pub(super) async fn write_file(
    path: PathBuf,
    bytes: Vec<u8>,
    expected: Option<Vec<u8>>,
) -> anyhow::Result<(PathBuf, Vec<u8>)> {
    let key = if expected.is_none() {
        let parent = path
            .parent()
            .expect("prepare resolves an absolute file path");
        let name = path.file_name().expect("a file path names a file");
        let parent = tokio::fs::canonicalize(parent).await?;
        dunce::simplified(&parent).join(name)
    } else {
        tracker_key(&path).await
    };
    tokio::task::spawn_blocking(move || {
        commit::write(&path, &bytes, expected.as_deref())
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok((key, bytes))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::util::fs::testing::symlink_file;
    use crate::util::path::local_testing::canonical_tempdir;

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

            let (metadata, bytes) = read_validated(&target, FileVerb::Read).await.unwrap();
            assert_eq!(
                (metadata.len(), bytes.as_slice()),
                (8, b"original".as_slice())
            );
            let err = read_validated(&link, FileVerb::Read).await.unwrap_err();
            #[cfg(unix)]
            let cause = std::io::Error::from_raw_os_error(nix::errno::Errno::ELOOP as i32);
            #[cfg(windows)]
            let cause = "the final path component is a symlink";
            assert_eq!(err, format!("Failed to read {}: {cause}", link.display()));
        }

        #[tokio::test]
        async fn preserves_each_mutating_tools_size_refusal() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notes.md");
            fs::File::create(&path)
                .unwrap()
                .set_len(MAX_TRACKED_FILE_SIZE + 1)
                .unwrap();

            for verb in [FileVerb::Read, FileVerb::Edit, FileVerb::Write] {
                let err = read_validated(&path, verb).await.unwrap_err();

                assert_eq!(
                    err,
                    format!(
                        "File is too large (10.0 MiB, max 10 MiB). Use the bash tool to {verb} large files."
                    )
                );
            }
        }
    }

    mod write {
        use super::*;

        #[test]
        fn overwrite_replaces_a_regular_file() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file.txt");
            fs::write(&path, "a much longer original").unwrap();

            let result = commit::write(&path, b"short", Some(b"a much longer original"));

            result.unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "short");
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
            let attempt = crate::tool::local_testing::WorkspaceWrite::new(&existing);

            for path in [&existing, &link] {
                let before = identity(path);

                let error = commit::write(path, b"x", None).unwrap_err();

                assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                assert_eq!(identity(path), before);
                attempt.assert_unchanged();
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

            let err = commit::write(&link, b"x", Some(b"original")).unwrap_err();
            #[cfg(unix)]
            assert_eq!(err.raw_os_error(), Some(nix::errno::Errno::ELOOP as i32));
            #[cfg(windows)]
            assert_eq!(err.to_string(), "the final path component is a symlink");
            assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        }
    }

    mod write_file {
        #[cfg(unix)]
        use super::*;

        #[cfg(unix)]
        #[tokio::test]
        async fn overwrite_refuses_a_fifo_without_blocking() {
            let dir = tempfile::tempdir().unwrap();
            let fifo = dir.path().join("pipe");
            nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

            let write = write_file(fifo.clone(), b"x".to_vec(), Some(Vec::new()));
            let error = tokio::time::timeout(std::time::Duration::from_secs(5), write)
                .await
                .expect("the open must not block")
                .unwrap_err();

            assert_eq!(
                format!("{error:#}"),
                format!("Failed to write {}: not a regular file", fifo.display()),
            );
        }
    }

    #[cfg(windows)]
    mod compressed_files {
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

            let (_, bytes) = read_validated(&path, FileVerb::Read).await.unwrap();
            assert_eq!(bytes, content.as_bytes());
            commit::write(&path, b"rewritten", Some(content.as_bytes())).unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"rewritten");
        }
    }
}
