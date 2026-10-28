//! Tracing subscriber initialization. TUI mode routes to `$XDG_STATE_HOME/ox/log/oxide-code.log`
//! because `EnterAlternateScreen` swaps only stdout, so stderr output would paint over the frame.

#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::fs::{self, File};
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use super::env;
use super::fs::{FileLock, create_private_dir_all, open_private_append, open_private_file};
use super::path::{XdgBase, app_dir};

const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";
const BACKUP_LOG_FILE: &str = "oxide-code.log.1";
const LOCK_FILE: &str = "oxide-code.log.lock";

/// Size past which the active log rotates into [`BACKUP_LOG_FILE`].
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Initializes tracing. TUI mode returns a file-appender guard that must live until exit to flush
/// all records.
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

/// Serializes rotation and writes across processes through a stable sidecar lock. Complete
/// events must be passed to [`Write::write_all`].
struct RotatingFile {
    path: PathBuf,
    backup: PathBuf,
    lock_path: PathBuf,
    max_bytes: u64,
}

impl RotatingFile {
    fn open(dir: &Path, max_bytes: u64) -> Result<Self> {
        let writer = Self {
            path: dir.join(LOG_FILE),
            backup: dir.join(BACKUP_LOG_FILE),
            lock_path: dir.join(LOCK_FILE),
            max_bytes,
        };
        let _lock = writer
            .lock()
            .with_context(|| format!("failed to lock {}", writer.lock_path.display()))?;
        open_private_append(&writer.path)
            .with_context(|| format!("failed to open {}", writer.path.display()))?;
        Ok(writer)
    }

    fn lock(&self) -> io::Result<FileLock> {
        FileLock::lock(open_private_file(&self.lock_path)?)
    }

    fn append<T>(
        &self,
        buf: &[u8],
        write: impl FnOnce(&mut File, &[u8]) -> io::Result<T>,
    ) -> io::Result<T> {
        let _lock = self.lock()?;
        let mut file = open_private_append(&self.path)?;
        let len = file.metadata()?.len();

        // Rotating an empty file would only replace the backup with nothing.
        if len > 0 && len.saturating_add(buf.len() as u64) > self.max_bytes {
            drop(file);
            fs::rename(&self.path, &self.backup)?;
            file = open_private_append(&self.path)?;
        }
        write(&mut file, buf)
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.append(buf, Write::write)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.append(buf, Write::write_all)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Read as _};
    use std::process::{Command, Stdio};

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
        fn honors_valid_directives_and_defaults_to_warn() {
            for (case, value, expected) in [
                ("level", Some("debug"), "debug"),
                (
                    "directive language",
                    Some("info,reqwest=warn"),
                    "reqwest=warn,info",
                ),
                ("unset", None, "warn"),
                ("empty", Some(""), "warn"),
                ("invalid", Some("ox=loud"), "warn"),
                ("mixed validity", Some("info,ox=loud"), "info"),
            ] {
                let filter = temp_env::with_var("RUST_LOG", value, make_filter);

                assert_eq!(filter.to_string(), expected, "{case}");
            }
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
            assert_eq!(mode(&dir.join(LOCK_FILE)), 0o600);
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

            #[test]
            fn names_the_sidecar_when_it_cannot_be_opened() {
                let tmp = tempfile::tempdir().unwrap();
                let lock = tmp.path().join(LOCK_FILE);
                fs::create_dir(&lock).unwrap();

                let error = RotatingFile::open(tmp.path(), 4)
                    .err()
                    .expect("a directory cannot be opened as a lock file");

                assert_eq!(
                    error.to_string(),
                    format!("failed to lock {}", lock.display())
                );
                assert!(error.downcast_ref::<io::Error>().is_some());
            }
        }

        mod append {
            use super::*;

            struct ShortWriter<'a> {
                file: &'a mut File,
                lock_path: &'a Path,
            }

            impl Write for ShortWriter<'_> {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    let contender = open_private_file(self.lock_path)?;
                    assert!(matches!(
                        contender.try_lock(),
                        Err(fs::TryLockError::WouldBlock)
                    ));
                    self.file.write(&buf[..buf.len().min(2)])
                }

                fn flush(&mut self) -> io::Result<()> {
                    self.file.flush()
                }
            }

            #[test]
            fn holds_the_same_lock_through_every_short_write() {
                let tmp = tempfile::tempdir().unwrap();
                let mut writer = RotatingFile::open(tmp.path(), 4).unwrap();
                writer.write_all(b"old\n").unwrap();

                writer
                    .append(b"whole\nevent\n", |file, bytes| {
                        ShortWriter {
                            file,
                            lock_path: &writer.lock_path,
                        }
                        .write_all(bytes)
                    })
                    .unwrap();

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "old\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "whole\nevent\n");

                writer.write_all(b"next\n").unwrap();

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "whole\nevent\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "next\n");
            }

            #[test]
            fn preserves_partial_bytes_and_releases_the_lock_on_failure() {
                let tmp = tempfile::tempdir().unwrap();
                let mut writer = RotatingFile::open(tmp.path(), 4).unwrap();
                writer.write_all(b"old\n").unwrap();

                let result: io::Result<()> = writer.append(b"partial\n", |file, bytes| {
                    file.write_all(&bytes[..3])?;
                    Err(io::Error::other("write failed"))
                });

                assert_eq!(result.unwrap_err().to_string(), "write failed");
                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "old\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "par");

                writer.write_all(b"next\n").unwrap();

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "par");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "next\n");
            }
        }

        mod write {
            use super::*;

            #[test]
            fn reports_appended_bytes_and_leaves_an_empty_write_unchanged() {
                let tmp = tempfile::tempdir().unwrap();
                let mut file = RotatingFile::open(tmp.path(), 4).unwrap();

                let written = file.write(b"oversized\n").unwrap();

                assert_eq!(written, 10);
                assert_eq!(read_log(tmp.path(), LOG_FILE), "oversized\n");

                assert_eq!(file.write(b"").unwrap(), 0);

                assert_eq!(read_log(tmp.path(), LOG_FILE), "oversized\n");
                assert!(!tmp.path().join(BACKUP_LOG_FILE).exists());
            }
        }

        mod write_all {
            use super::*;

            fn read_ack(output: &mut impl io::BufRead, expected: &str) {
                loop {
                    let mut line = String::new();
                    assert_ne!(output.read_line(&mut line).unwrap(), 0);
                    if line.contains(expected) {
                        return;
                    }
                }
            }

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

                file.write_all(b"").unwrap();

                assert_eq!(read_log(tmp.path(), LOG_FILE), "oversized\n");
                assert!(!tmp.path().join(BACKUP_LOG_FILE).exists());
            }

            #[test]
            fn keeps_events_whole_through_the_subscriber_and_worker() {
                let tmp = tempfile::tempdir().unwrap();
                let file = RotatingFile::open(tmp.path(), 12).unwrap();
                let (writer, guard) = tracing_appender::non_blocking(file);
                let subscriber = tracing_subscriber::fmt()
                    .without_time()
                    .with_level(false)
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(writer)
                    .finish();

                tracing::subscriber::with_default(subscriber, || {
                    tracing::info!("first\nbody");
                    tracing::info!("second\nbody");
                });
                drop(guard);

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "first\nbody\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "second\nbody\n");
            }

            #[test]
            fn rotates_while_a_reader_holds_the_old_file_open() {
                let tmp = tempfile::tempdir().unwrap();
                let mut file = RotatingFile::open(tmp.path(), 4).unwrap();
                file.write_all(b"old\n").unwrap();
                let mut reader = File::open(tmp.path().join(LOG_FILE)).unwrap();

                file.write_all(b"new\n").unwrap();
                file.write_all(b"end\n").unwrap();

                let mut previous = String::new();
                reader.read_to_string(&mut previous).unwrap();
                assert_eq!(previous, "old\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "end\n");
                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "new\n");
            }

            #[test]
            fn independent_writers_follow_the_active_file_after_repeated_rotation() {
                let tmp = tempfile::tempdir().unwrap();
                let mut first = RotatingFile::open(tmp.path(), 8).unwrap();
                let mut second = RotatingFile::open(tmp.path(), 8).unwrap();

                for record in [b"A00\n", b"A01\n", b"A02\n", b"A03\n", b"A04\n"] {
                    first.write_all(record).unwrap();
                }
                second.write_all(b"B00\n").unwrap();

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "A02\nA03\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "A04\nB00\n");

                first.write_all(b"A05\n").unwrap();
                second.write_all(b"B01\n").unwrap();

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "A04\nB00\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "A05\nB01\n");
            }

            #[test]
            fn subprocess_writer_follows_the_active_file_after_repeated_rotation() {
                const CHILD_DIR: &str = "OX_TEST_LOG_CHILD_DIR";
                if let Some(dir) = std::env::var_os(CHILD_DIR) {
                    let mut file = RotatingFile::open(Path::new(&dir), 8).unwrap();
                    println!("log-writer-ready");
                    let mut proceed = String::new();
                    io::stdin().read_line(&mut proceed).unwrap();
                    assert_eq!(proceed, "check\n");
                    let contender = open_private_file(&file.lock_path).unwrap();
                    assert!(matches!(
                        contender.try_lock(),
                        Err(fs::TryLockError::WouldBlock)
                    ));
                    println!("log-lock-held");
                    proceed.clear();
                    io::stdin().read_line(&mut proceed).unwrap();
                    assert_eq!(proceed, "write\n");
                    file.write_all(b"B00\n").unwrap();
                    return;
                }

                let tmp = tempfile::tempdir().unwrap();
                let test = concat!(
                    module_path!(),
                    "::subprocess_writer_follows_the_active_file_after_repeated_rotation"
                );
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", test.split_once("::").unwrap().1, "--nocapture"])
                    .env(CHILD_DIR, tmp.path())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut output = BufReader::new(child.stdout.take().unwrap());
                read_ack(&mut output, "log-writer-ready");
                let mut first = RotatingFile::open(tmp.path(), 8).unwrap();
                let lock = first.lock().unwrap();
                let mut input = child.stdin.take().unwrap();

                input.write_all(b"check\n").unwrap();
                read_ack(&mut output, "log-lock-held");
                drop(lock);

                for record in [b"A00\n", b"A01\n", b"A02\n", b"A03\n", b"A04\n"] {
                    first.write_all(record).unwrap();
                }
                input.write_all(b"write\n").unwrap();
                assert!(child.wait().unwrap().success());

                assert_eq!(read_log(tmp.path(), BACKUP_LOG_FILE), "A02\nA03\n");
                assert_eq!(read_log(tmp.path(), LOG_FILE), "A04\nB00\n");
            }
        }
    }
}
