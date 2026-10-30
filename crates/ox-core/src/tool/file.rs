//! File access for the file tools: target validation, reads checked on the opened file, and writes
//! that never follow a symlink at the final component.

mod commit;

pub(super) use self::commit::Checked;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use strum::Display;

use crate::file_tracker::{FileTracker, GatePurpose, MAX_TRACKED_FILE_SIZE};
use crate::util::fs::{FileReadError, read_regular_file, validate_regular_file};
use crate::util::path::lexical_normalize;
use crate::util::text::format_size;

// ── Tracker Keys ──

/// The [`FileTracker`] key for the absolute `path` a file tool
/// acts on. An existing path canonicalizes, so dotted and symlinked spellings of one file share an
/// entry. A missing path falls back to its lexically clean form. The gate's resolved path cannot
/// stand in, since it keeps each name as spelled, while `canonicalize` returns the on-disk case on
/// a case-insensitive volume.
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

/// Rejects targets the [`FileTracker`] cannot gate by their metadata before an open, phrasing each
/// refusal for `verb`. Blocks, so callers run it on the blocking pool.
fn validate_file_target(path: &Path, verb: FileVerb) -> Result<(), String> {
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("Failed to access {}: {e}", path.display()))?;
    validate_regular_file(&metadata, MAX_TRACKED_FILE_SIZE)
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

// ── Text Rules ──

const BINARY_CHECK_SIZE: usize = 8192;

/// Returns whether `bytes` hold a NUL in the first 8 KiB, which Git takes to mean binary. `read`
/// refuses such a file, and `grep` skips it.
pub(super) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_CHECK_SIZE).any(|&b| b == 0)
}

/// `text` without a leading UTF-8 BOM, which `read` and `grep` drop from a file's first line.
pub(super) fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

// ── File Reading ──

/// Reads a bounded regular file, checking its metadata before opening and through the opened
/// no-follow handle. Errors name `path` and phrase refusals for `verb`.
pub(super) async fn read_validated(path: &Path, verb: FileVerb) -> Result<Vec<u8>, String> {
    let read_path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        validate_file_target(&read_path, verb)?;
        read_regular_file(&read_path, MAX_TRACKED_FILE_SIZE)
            .map_err(|error| file_read_error(&read_path, error, verb))
    })
    .await
    .map_err(|error| format!("Failed to read {}: {error}", path.display()))?
}

// ── File Writing ──

/// Opens the existing file at `path` for an edit or an overwrite, checking its metadata before the
/// open and through the opened no-follow handle, and passes the content it read through the
/// Read-before-Edit gate under the tracker `key`. Errors name `path` and phrase refusals for the
/// purpose. Blocks, so callers run it on the blocking pool.
pub(super) fn open_gated(
    path: &Path,
    key: &Path,
    tracker: &FileTracker,
    purpose: GatePurpose,
) -> Result<Checked, String> {
    let verb = match purpose {
        GatePurpose::Edit => FileVerb::Edit,
        GatePurpose::Write => FileVerb::Write,
    };
    validate_file_target(path, verb)?;
    let checked = Checked::open(path).map_err(|error| file_read_error(path, error, verb))?;
    tracker
        .verify_current_content(key, checked.content(), purpose)
        .map_err(|e| e.to_string())?;
    Ok(checked)
}

/// Rewrites the file `checked` holds with `bytes` and returns them under the tracker `key`. The
/// caller must use the tracker's owned write operation.
pub(super) async fn rewrite_file(
    checked: Checked,
    key: PathBuf,
    bytes: Vec<u8>,
) -> anyhow::Result<(PathBuf, Vec<u8>)> {
    tokio::task::spawn_blocking(move || {
        let path = checked.path().to_owned();
        checked
            .rewrite(&bytes)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok((key, bytes))
    })
    .await?
}

/// Creates the new file `path` holding `bytes` and returns them under its tracker key. The caller
/// must use the tracker's owned write operation.
pub(super) async fn create_file(
    path: PathBuf,
    bytes: Vec<u8>,
) -> anyhow::Result<(PathBuf, Vec<u8>)> {
    let parent = path
        .parent()
        .expect("prepare resolves an absolute file path");
    let name = path.file_name().expect("a file path names a file");
    let parent = tokio::fs::canonicalize(parent).await?;
    let key = dunce::simplified(&parent).join(name);
    tokio::task::spawn_blocking(move || {
        commit::create(&path, &bytes)
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

    mod strip_bom {
        use super::*;

        #[test]
        fn removes_bom() {
            assert_eq!(strip_bom("\u{feff}hello"), "hello");
        }

        #[test]
        fn keeps_text_without_a_bom() {
            assert_eq!(strip_bom("hello"), "hello");
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

            let bytes = read_validated(&target, FileVerb::Read).await.unwrap();
            assert_eq!(bytes, b"original");
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

            let bytes = read_validated(&path, FileVerb::Read).await.unwrap();
            assert_eq!(bytes, content.as_bytes());
            let checked = Checked::open(&path).unwrap();
            assert_eq!(checked.content(), content.as_bytes());
            checked.rewrite(b"rewritten").unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"rewritten");
        }
    }
}
