//! Tracing subscriber initialization. TUI mode routes to `$XDG_STATE_HOME/ox/log/oxide-code.log`
//! because `EnterAlternateScreen` swaps only stdout, so stderr output would paint over the frame.

use std::fs::{self, File};
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use super::env;
use super::fs::{create_private_dir_all, open_private_append};
use super::path::{XdgBase, app_dir};

const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";
const BACKUP_LOG_FILE: &str = "oxide-code.log.1";

/// Size past which the active log rotates into [`BACKUP_LOG_FILE`].
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Initializes the global tracing subscriber. In TUI mode it returns the file appender's guard,
/// which flushes on drop and must live until exit, since later lines are lost once it drops.
pub fn init_tracing(tui_mode: bool) -> Result<Option<WorkerGuard>> {
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
            .with_writer(io::stderr)
            // `with_ansi` overrides the subscriber's own `NO_COLOR` check.
            .with_ansi(io::stderr().is_terminal() && env::string("NO_COLOR").is_none())
            .init();
        Ok(None)
    }
}

/// The `RUST_LOG` directives, or `warn` when the variable sets none, skipping invalid ones.
fn make_filter() -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .from_env_lossy()
}

fn log_dir() -> Result<PathBuf> {
    app_dir(XdgBase::State)
        .map(|dir| dir.join(LOG_SUBDIR))
        .context("failed to determine the log directory")
}

fn open_file_appender(dir: &Path) -> Result<(NonBlocking, WorkerGuard)> {
    create_private_dir_all(dir)?;
    let file = RotatingFile::open(dir, MAX_LOG_BYTES)?;
    Ok(tracing_appender::non_blocking(file))
}

/// Private (0600) append-only log that moves itself over [`BACKUP_LOG_FILE`] when a write
/// would push it past `max_bytes`.
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
        fs::rename(&self.path, &self.backup)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::path::local_testing::abs;

    fn read_log(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    mod make_filter {
        use super::*;

        #[test]
        fn honors_rust_log_level() {
            temp_env::with_var("RUST_LOG", Some("debug"), || {
                assert_eq!(make_filter().to_string(), "debug");
            });
        }

        #[test]
        fn honors_rust_log_directive_language() {
            temp_env::with_var("RUST_LOG", Some("info,reqwest=warn"), || {
                assert_eq!(make_filter().to_string(), "reqwest=warn,info");
            });
        }

        #[test]
        fn defaults_to_warn_when_rust_log_is_unset_empty_or_invalid() {
            for value in [None, Some(""), Some("ox=loud")] {
                let filter = temp_env::with_var("RUST_LOG", value, make_filter);

                assert_eq!(filter.to_string(), "warn", "{value:?}");
            }
        }

        #[test]
        fn keeps_the_valid_directives_beside_invalid_ones() {
            temp_env::with_var("RUST_LOG", Some("info,ox=loud"), || {
                assert_eq!(make_filter().to_string(), "info");
            });
        }
    }

    mod log_dir {
        use super::*;

        #[test]
        fn sits_in_the_app_state_directory() {
            let state = abs("/state");

            let dir = temp_env::with_var("XDG_STATE_HOME", Some(&state), log_dir);

            assert_eq!(dir.unwrap(), Path::new(&state).join("ox").join("log"));
        }
    }

    mod open_file_appender {
        use super::*;

        #[test]
        fn creates_missing_directory() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("nested").join("log");
            assert!(!dir.exists());

            let (_writer, _guard) = open_file_appender(&dir).unwrap();
            assert!(dir.is_dir());
        }

        #[test]
        fn appends_across_sessions() {
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
        fn keeps_directory_and_file_private() {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("log");

            let (_writer, _guard) = open_file_appender(&dir).unwrap();
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join(LOG_FILE)), 0o600);
        }
    }

    mod rotating_file {
        use super::*;

        mod open {
            use super::*;

            #[test]
            fn counts_existing_bytes_toward_cap() {
                let tmp = tempfile::tempdir().unwrap();
                fs::write(tmp.path().join(LOG_FILE), b"0123456789").unwrap();

                let mut file = RotatingFile::open(tmp.path(), 12).unwrap();
                file.write_all(b"abc").unwrap();

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "0123456789");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "abc");
            }
        }

        mod write {
            use super::*;

            #[test]
            fn rotates_into_a_single_backup_past_the_cap() {
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
            fn keeps_an_oversized_record_in_an_empty_file() {
                let tmp = tempfile::tempdir().unwrap();
                let mut file = RotatingFile::open(tmp.path(), 4).unwrap();

                file.write_all(b"oversized\n").unwrap();
                assert_eq!(read_log(tmp.path(), LOG_FILE), "oversized\n");
                assert!(!tmp.path().join(BACKUP_LOG_FILE).exists());
            }
        }
    }
}
