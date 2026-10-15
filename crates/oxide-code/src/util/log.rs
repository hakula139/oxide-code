//! Tracing subscriber initialization. TUI mode routes to `$XDG_STATE_HOME/ox/log/oxide-code.log`
//! because `EnterAlternateScreen` swaps only stdout, so stderr output would paint over the frame.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::util::fs::create_private_dir_all;
use crate::util::path::xdg_dir;

const APP_DIR: &str = "ox";
const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";
const BACKUP_LOG_FILE: &str = "oxide-code.log.1";

/// Size past which the active log rotates into [`BACKUP_LOG_FILE`].
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Initializes the global tracing subscriber.
///
/// The returned `WorkerGuard` (TUI mode only) flushes the non-blocking appender on drop and must
/// be held for the program lifetime; dropping it early truncates buffered log lines.
pub(crate) fn init_tracing(tui_mode: bool) -> Result<Option<WorkerGuard>> {
    let filter = make_filter();
    Ok(if let Some((writer, guard)) = build_log_target(tui_mode)? {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(false)
            .init();
        Some(guard)
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
        None
    })
}

fn make_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
}

fn build_log_target(tui_mode: bool) -> Result<Option<(NonBlocking, WorkerGuard)>> {
    if !tui_mode {
        return Ok(None);
    }
    let dir = resolve_log_dir().context("cannot determine log directory")?;
    Ok(Some(open_file_appender(&dir)?))
}

fn resolve_log_dir() -> Option<PathBuf> {
    log_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::home_dir(),
    )
}

fn log_dir_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_dir(
        xdg,
        home,
        Path::new(".local/state"),
        &Path::new(APP_DIR).join(LOG_SUBDIR),
    )
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

/// Opens `path` for appending with 0o600 perms on Unix, tightening a file an older build left
/// world-readable.
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
            let rendered = make_filter().to_string();
            assert!(rendered.contains("info"), "rendered={rendered}");
            assert!(rendered.contains("reqwest=warn"), "rendered={rendered}");
        });
    }

    // ── build_log_target ──

    #[test]
    fn build_log_target_is_none_when_tui_mode_false() {
        assert!(build_log_target(false).unwrap().is_none());
    }

    #[test]
    fn build_log_target_writes_to_log_file_when_tui_mode_true() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().to_string_lossy().into_owned();
        temp_env::with_vars(
            [
                ("XDG_STATE_HOME", Some(xdg.as_str())),
                ("HOME", Some("/home/u")),
            ],
            || {
                let (mut writer, guard) =
                    build_log_target(true).unwrap().expect("file mode in TUI");
                let log_dir = tmp.path().join("ox").join("log");
                assert!(log_dir.is_dir());

                writer.write_all(b"sentinel-line\n").unwrap();
                drop(guard);

                let written = std::fs::read_to_string(log_dir.join(LOG_FILE)).unwrap();
                assert!(written.contains("sentinel-line"), "written={written}");
            },
        );
    }

    // ── log_dir_from ──

    #[test]
    fn log_dir_from_uses_xdg_state_home_when_set_and_absolute() {
        let resolved = log_dir_from(
            Some(PathBuf::from("/run/user/1000/state")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/run/user/1000/state/ox/log")));
    }

    #[test]
    fn log_dir_from_falls_back_to_home_local_state_when_xdg_unset() {
        let resolved = log_dir_from(None, Some(PathBuf::from("/home/u")));
        assert_eq!(resolved, Some(PathBuf::from("/home/u/.local/state/ox/log")));
    }

    #[test]
    fn log_dir_from_ignores_relative_xdg_and_uses_home_fallback() {
        // Relative `$XDG_STATE_HOME` would resolve against cwd; `xdg_dir` rejects it.
        let resolved = log_dir_from(
            Some(PathBuf::from("relative/state")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/home/u/.local/state/ox/log")));
    }

    #[test]
    fn log_dir_from_is_none_without_xdg_or_home() {
        assert!(log_dir_from(None, None).is_none());
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

        let written = std::fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert!(written.contains("first"), "written={written}");
        assert!(written.contains("second"), "written={written}");
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
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to create"), "msg={msg}");
        assert!(msg.contains(&dir.display().to_string()), "msg={msg}");
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
