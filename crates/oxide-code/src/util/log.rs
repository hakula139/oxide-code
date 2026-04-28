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
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::util::path::xdg_dir;

const APP_DIR: &str = "ox";
const LOG_SUBDIR: &str = "log";
const LOG_FILE: &str = "oxide-code.log";

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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    if tui_mode {
        let dir = resolve_log_dir().context("cannot determine log directory")?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        // `never` writes to a single file without rotation. The crate
        // emits a handful of warn lines per session at most, so an
        // unrotated file stays bounded for years of normal use; switch
        // to `daily` here if dogfooding shows the file growing fast.
        let appender = tracing_appender::rolling::never(&dir, LOG_FILE);
        let (writer, guard) = tracing_appender::non_blocking(appender);
        // `with_ansi(false)` keeps the file plain text — ratatui themes
        // do not apply, and ANSI escape codes would clutter `cat` /
        // `less` output.
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
