//! Tracing subscriber initialization. TUI mode routes to `$XDG_STATE_HOME/ox/log/oxide-code.log`
//! because `EnterAlternateScreen` swaps only stdout, so stderr output would paint over the frame.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::util::fs::create_private_dir_all;
use crate::util::path::{XdgBase, app_dir};

const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";
const BACKUP_LOG_FILE: &str = "oxide-code.log.1";

/// Size past which the active log rotates into [`BACKUP_LOG_FILE`].
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Initializes the global tracing subscriber.
///
/// The returned `WorkerGuard` (TUI mode only) flushes the non-blocking appender on drop and must
/// be held for the program lifetime, because dropping it early truncates buffered log lines.
pub(crate) fn init_tracing(tui_mode: bool) -> Result<Option<WorkerGuard>> {
    let filter = make_filter();
    if tui_mode {
        let (writer, guard) = open_file_appender(&log_dir()?)?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(false)
            .init();
        Ok(Some(guard))
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
        Ok(None)
    }
}

fn make_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
}

fn log_dir() -> Result<PathBuf> {
    app_dir(XdgBase::State)
        .map(|dir| dir.join(LOG_SUBDIR))
        .context("cannot determine log directory")
}

fn open_file_appender(dir: &Path) -> Result<(NonBlocking, WorkerGuard)> {
    create_private_dir_all(dir)?;
    let file = RotatingFile::open(dir, MAX_LOG_BYTES)?;
    Ok(tracing_appender::non_blocking(file))
}

/// Private (0o600) append-only log that moves itself to [`BACKUP_LOG_FILE`] when a write would
/// push it past `max_bytes`. Each rotation replaces the previous backup, so the directory holds at
/// most two log files.
struct RotatingFile {
    path: PathBuf,
    backup: PathBuf,
    file: File,
    len: u64,
    max_bytes: u64,
}

impl RotatingFile {
    fn open(dir: &Path, max_bytes: u64) -> Result<Self> {
        let path = dir.join(LOG_FILE);
        let file = open_private_append(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("failed to stat {}", path.display()))?
            .len();
        Ok(Self {
            path,
            backup: dir.join(BACKUP_LOG_FILE),
            file,
            len,
            max_bytes,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        std::fs::rename(&self.path, &self.backup)?;
        self.file = open_private_append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotating an empty file would only replace the backup with nothing.
        if self.len > 0 && self.len + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Opens `path` for appending with 0o600 perms on Unix, also tightening an existing file's mode.
fn open_private_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::util::path::testing::abs;

    // ── make_filter ──

    #[test]
    fn make_filter_defaults_to_warn_when_rust_log_unset() {
        temp_env::with_var_unset("RUST_LOG", || {
            assert_eq!(make_filter().to_string(), "warn");
        });
    }

    #[test]
    fn make_filter_empty_rust_log_yields_empty_filter() {
        // Empty `RUST_LOG` parses to an empty filter, not the warn floor.
        temp_env::with_var("RUST_LOG", Some(""), || {
            assert_eq!(make_filter().to_string(), "");
        });
    }

    #[test]
    fn make_filter_honors_rust_log_level() {
        temp_env::with_var("RUST_LOG", Some("debug"), || {
            assert_eq!(make_filter().to_string(), "debug");
        });
    }

    #[test]
    fn make_filter_honors_rust_log_directive_language() {
        temp_env::with_var("RUST_LOG", Some("info,reqwest=warn"), || {
            assert_eq!(make_filter().to_string(), "reqwest=warn,info");
        });
    }

    // ── log_dir ──

    #[test]
    fn log_dir_sits_in_the_app_state_directory() {
        let state = abs("/state");

        let dir = temp_env::with_var("XDG_STATE_HOME", Some(&state), log_dir);

        assert_eq!(dir.unwrap(), Path::new(&state).join("ox").join("log"));
    }

    // ── open_file_appender ──

    #[test]
    fn open_file_appender_creates_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("log");
        assert!(!dir.exists());

        let (_writer, _guard) = open_file_appender(&dir).unwrap();
        assert!(dir.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn open_file_appender_keeps_directory_and_file_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("log");

        let (_writer, _guard) = open_file_appender(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(LOG_FILE)), 0o600);
    }

    #[test]
    fn open_file_appender_appends_across_sessions() {
        // Below the size cap, reopening must append to the same file.
        let tmp = tempfile::tempdir().unwrap();
        for line in ["first\n", "second\n"] {
            let (mut writer, guard) = open_file_appender(tmp.path()).unwrap();
            writer.write_all(line.as_bytes()).unwrap();
            drop(guard);
        }

        assert_eq!(read_log(tmp.path(), LOG_FILE), "first\nsecond\n");
    }

    #[cfg(unix)]
    #[test]
    fn open_file_appender_fails_when_parent_is_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"").unwrap();
        // Treating a regular file as a directory parent yields ENOTDIR on Unix.
        let dir = blocker.join("log");

        let err = open_file_appender(&dir)
            .expect_err("create_dir_all should fail under a regular-file parent");

        assert_eq!(
            err.to_string(),
            format!("failed to create {}", dir.display()),
        );
    }

    // ── RotatingFile::open ──

    #[test]
    fn rotating_file_open_counts_existing_bytes_toward_cap() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LOG_FILE), b"0123456789").unwrap();

        let mut file = RotatingFile::open(tmp.path(), 12).unwrap();
        file.write_all(b"abc").unwrap();

        assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "0123456789");
        assert_eq!(read_log(tmp.path(), LOG_FILE), "abc");
    }

    #[cfg(unix)]
    #[test]
    fn rotating_file_open_tightens_lax_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LOG_FILE);
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        RotatingFile::open(tmp.path(), MAX_LOG_BYTES).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    // ── RotatingFile::write ──

    #[test]
    fn rotating_file_write_rotates_into_a_single_backup_past_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::open(tmp.path(), 14).unwrap();

        file.write_all(b"first-line\n").unwrap();
        file.write_all(b"second\n").unwrap();
        assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "first-line\n");
        assert_eq!(read_log(tmp.path(), LOG_FILE), "second\n");

        file.write_all(b"third\n").unwrap();
        assert_eq!(read_log(tmp.path(), LOG_FILE), "second\nthird\n");

        file.write_all(b"fourth\n").unwrap();
        assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "second\nthird\n");
        assert_eq!(read_log(tmp.path(), LOG_FILE), "fourth\n");
        #[cfg(unix)]
        assert_eq!(mode(&tmp.path().join(LOG_FILE)), 0o600);
    }

    #[test]
    fn rotating_file_write_keeps_an_oversized_record_in_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::open(tmp.path(), 4).unwrap();

        file.write_all(b"oversized\n").unwrap();
        assert_eq!(read_log(tmp.path(), LOG_FILE), "oversized\n");
        assert!(!tmp.path().join(BACKUP_LOG_FILE).exists());
    }

    fn read_log(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
