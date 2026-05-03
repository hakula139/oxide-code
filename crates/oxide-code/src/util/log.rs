//! Tracing subscriber initialization.
//!
//! TUI mode routes to `$XDG_STATE_HOME/ox/log/oxide-code.log`:
//! `EnterAlternateScreen` only swaps stdout, so stderr (the default
//! `tracing::fmt()` writer) would paint over the frame. Other modes
//! keep stderr.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::util::path::xdg_dir;

const APP_DIR: &str = "ox";
const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";

/// Initializes the global `tracing` subscriber.
///
/// Bind the returned `WorkerGuard` (TUI mode only) for the program
/// lifetime — its `Drop` flushes the non-blocking appender. Honors
/// `RUST_LOG` with a `warn` floor.
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

/// `$XDG_STATE_HOME/ox/log`, falling back to `$HOME/.local/state/ox/log`.
fn resolve_log_dir() -> Option<PathBuf> {
    log_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Pure form of [`resolve_log_dir`] with explicit inputs.
fn log_dir_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_dir(
        xdg,
        home,
        Path::new(".local/state"),
        &Path::new(APP_DIR).join(LOG_SUBDIR),
    )
}

/// `never` keeps a single unrotated file — the crate emits a handful
/// of warns per session. Switch to `daily` if `RUST_LOG=debug`
/// dogfooding shows growth.
fn open_file_appender(dir: &Path) -> Result<(NonBlocking, WorkerGuard)> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let appender = tracing_appender::rolling::never(dir, LOG_FILE);
    Ok(tracing_appender::non_blocking(appender))
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
        // Relative `$XDG_STATE_HOME` would resolve against the cwd; `xdg_dir` rejects it.
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

    #[test]
    fn open_file_appender_appends_across_sessions() {
        // `never` rotation must keep both lines — pin so a future
        // switch to `daily` shows up as a test diff.
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
}
