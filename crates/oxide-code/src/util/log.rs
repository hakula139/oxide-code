//! Tracing subscriber initialization for `ox`.
//!
//! In TUI mode, diagnostics route to `$XDG_STATE_HOME/ox/log/oxide-code.log`
//! so they never bleed onto the alternate screen — `EnterAlternateScreen`
//! only swaps stdout's buffer, and stderr (where `tracing::fmt()` writes by
//! default) keeps painting the underlying terminal. Without this routing
//! every `warn!` call lands on the rendered frame and persists until
//! ratatui's next redraw covers it.
//!
//! In `--no-tui` REPL, headless, and `--list` modes the subscriber keeps
//! the default stderr destination — the natural surface for command-line
//! output, with no alternate screen to corrupt.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::util::path::xdg_dir;

const APP_DIR: &str = "ox";
const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";

/// Resolved sink for the global subscriber. Construction is pulled out
/// of [`init_tracing`] so the path-and-appender wiring is testable
/// without installing a process-global subscriber.
#[derive(Debug)]
enum LogTarget {
    File {
        writer: NonBlocking,
        guard: WorkerGuard,
    },
    Stderr,
}

/// Initializes the global `tracing` subscriber.
///
/// Returns `Some(WorkerGuard)` when `tui_mode` is true. The guard owns
/// the non-blocking appender's worker thread and flushes pending writes
/// on `Drop`; callers must keep it bound for the program lifetime so
/// teardown warnings (panic hook output, agent-loop errors,
/// session-finish failures) reach disk before the process exits. The
/// stderr branch has no async worker and returns `None`.
///
/// Honors `RUST_LOG` when set, falling back to `warn`. The `warn` floor
/// applies in both modes — a hidden file does not warrant a chattier
/// default than the stderr path it shadows; `RUST_LOG=info` overrides
/// per-invocation when more signal is needed.
pub(crate) fn init_tracing(tui_mode: bool) -> Result<Option<WorkerGuard>> {
    let filter = make_filter();
    Ok(match build_log_target(tui_mode)? {
        LogTarget::File { writer, guard } => {
            // `with_ansi(false)` keeps the file plain text — ratatui themes
            // do not apply, and ANSI escape codes would clutter `cat` /
            // `less` output.
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .init();
            Some(guard)
        }
        LogTarget::Stderr => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
            None
        }
    })
}

/// Builds the [`EnvFilter`] used by the subscriber: honors `RUST_LOG`
/// when set, otherwise applies a `warn` floor.
fn make_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
}

/// Decides where logs go, creates the on-disk directory in TUI mode,
/// and returns the writer + guard pair. Pulled out of [`init_tracing`]
/// so tests can exercise it without touching the global subscriber.
fn build_log_target(tui_mode: bool) -> Result<LogTarget> {
    if !tui_mode {
        return Ok(LogTarget::Stderr);
    }
    let dir = resolve_log_dir().context("cannot determine log directory")?;
    let (writer, guard) = open_file_appender(&dir)?;
    Ok(LogTarget::File { writer, guard })
}

/// Creates `dir` if missing and opens a non-blocking appender on
/// [`LOG_FILE`] inside it. `never` writes a single file without
/// rotation — the crate emits a handful of warn lines per session at
/// most, so an unrotated file stays bounded for years of normal use;
/// switch to `daily` here if dogfooding shows the file growing fast.
fn open_file_appender(dir: &Path) -> Result<(NonBlocking, WorkerGuard)> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let appender = tracing_appender::rolling::never(dir, LOG_FILE);
    Ok(tracing_appender::non_blocking(appender))
}

/// Resolves `$XDG_STATE_HOME/ox/log`, falling back to
/// `$HOME/.local/state/ox/log` per the XDG Base Directory spec. Returns
/// `None` only in exotic environments without `HOME` or `XDG_STATE_HOME`.
fn resolve_log_dir() -> Option<PathBuf> {
    log_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Pure form of [`resolve_log_dir`] with explicit inputs, exposed so
/// tests can pin both bases without mutating process-global env.
fn log_dir_from(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_dir(
        xdg,
        home,
        Path::new(".local/state"),
        &Path::new(APP_DIR).join(LOG_SUBDIR),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── make_filter ──

    #[test]
    fn make_filter_defaults_to_warn_when_rust_log_unset() {
        temp_env::with_var_unset("RUST_LOG", || {
            assert_eq!(make_filter().to_string(), "warn");
        });
    }

    #[test]
    fn make_filter_defaults_to_warn_when_rust_log_empty() {
        // Empty `RUST_LOG` parses to an empty filter, not the warn floor —
        // pin current behavior so future refactors don't silently change it.
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

    // ── open_file_appender ──

    #[test]
    fn open_file_appender_creates_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("log");
        assert!(!dir.exists());

        let result = open_file_appender(&dir);
        assert!(
            result.is_ok(),
            "open_file_appender failed: {:?}",
            result.err()
        );
        assert!(dir.is_dir());
    }

    #[test]
    fn open_file_appender_idempotent_on_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        // First call creates it; second call must not error. Binding
        // both tuple values keeps clippy quiet about the must-use
        // `WorkerGuard`.
        let _first = open_file_appender(tmp.path()).unwrap();
        let _second = open_file_appender(tmp.path()).unwrap();
    }

    #[test]
    fn open_file_appender_errors_when_path_blocked_by_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"").unwrap();
        // Treating a regular file as a directory parent yields ENOTDIR.
        let dir = blocker.join("log");

        let err = open_file_appender(&dir)
            .expect_err("expected create_dir_all to fail under a regular-file parent");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to create"), "msg={msg}");
        assert!(msg.contains(&dir.display().to_string()), "msg={msg}");
    }

    // ── build_log_target ──

    #[test]
    fn build_log_target_stderr_when_tui_mode_false() {
        // Stderr branch takes no env or filesystem path — assert without
        // touching either. Match on the variant; the writer / guard fields
        // would otherwise force naming the inner types.
        assert!(matches!(
            build_log_target(false).unwrap(),
            LogTarget::Stderr
        ));
    }

    #[test]
    fn build_log_target_file_when_tui_mode_true() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().to_string_lossy().into_owned();
        temp_env::with_vars(
            [
                ("XDG_STATE_HOME", Some(xdg.as_str())),
                ("HOME", Some("/home/u")),
            ],
            || {
                let target = build_log_target(true).unwrap();
                assert!(matches!(target, LogTarget::File { .. }));
                assert!(tmp.path().join("ox").join("log").is_dir());
            },
        );
    }

    // The `resolve_log_dir() -> None` arm is unreachable from unit tests:
    // `dirs::home_dir()` falls back to `getpwuid_r` on Unix, so unsetting
    // `HOME` doesn't actually return `None`. The `.context()` wrapping is
    // covered by `open_file_appender_errors_when_path_blocked_by_file`,
    // which exercises the same `?` propagation shape.

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
        // Mirrors `xdg_dir`'s rejection of relative XDG values — a
        // relative `$XDG_STATE_HOME` would resolve against the cwd and
        // produce surprising layouts under `cd`.
        let resolved = log_dir_from(
            Some(PathBuf::from("relative/state")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/home/u/.local/state/ox/log")));
    }

    #[test]
    fn log_dir_from_returns_none_without_xdg_or_home() {
        assert!(log_dir_from(None, None).is_none());
    }
}
