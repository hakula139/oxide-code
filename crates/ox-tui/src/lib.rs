//! Terminal UI built on ratatui and crossterm, driven by a `tokio::select!` event loop.

mod action;
mod app;
mod clipboard;
mod components;
mod cursor;
mod git_probes;
pub mod glyphs;
mod keymap;
mod labels;
mod markdown;
mod modal;
mod pending_calls;
mod session_info;
pub mod slash;
pub mod terminal;
#[cfg(test)]
mod testing;
pub mod theme;
mod viewport;
mod wrap;

use std::fmt::Debug;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, warn};

use ox_core::config::ConfigSnapshot;
use ox_core::engine::{Engine, EngineHandle};
use ox_core::session::entry::ArchiveState;
use ox_core::util::path::tildify;

use self::action::Action;
use self::app::{App, AppHistory};
use self::session_info::LiveSessionInfo;
use self::slash::ResumePicker;
use self::theme::ResolvedTheme;

// ── Constants ──

/// How long a timed notice, such as the outcome of a copy or of a picker action, stays shown.
const NOTICE_DURATION: Duration = Duration::from_secs(3);

// ── Driver ──

/// Startup state the TUI shows besides the engine's history.
pub struct Startup {
    /// The branch checked out at startup, which the status bar shows until its first git probe.
    pub git_branch: Option<String>,
    /// The session picker that `--resume` without a session opens over the first frame.
    pub picker: Option<ResumePicker>,
    /// The `ox <prompt>` text, submitted as if typed once the TUI opens.
    pub prompt: Option<String>,
    pub title: Option<String>,
    /// The session's state before startup resumed it, which unarchives an archived session.
    pub prior: ArchiveState,
}

/// Runs the TUI until the user quits or `shutdown` resolves, then waits for the engine to finalize
/// the live session. Shows the notices of `theme` in the chat. Returns what `shutdown` resolved to,
/// or `None` when the user quit. An engine panic is returned after the terminal is restored, which
/// prints the deferred panic report.
pub async fn run<S: Debug>(
    mut engine: Engine,
    theme: ResolvedTheme,
    config: ConfigSnapshot,
    startup: Startup,
    shutdown: impl Future<Output = S>,
) -> Result<Option<S>> {
    terminal::install_panic_hook();

    let ResolvedTheme {
        theme,
        overrides,
        notices: theme_notices,
    } = theme;
    let cwd = engine.cwd();
    let session_info = LiveSessionInfo {
        cwd: cwd.to_owned(),
        display_cwd: tildify(cwd),
        project_root: engine.project_root().to_owned(),
        version: env!("CARGO_PKG_VERSION"),
        session_id: engine.session_id().to_owned(),
        session_allow: Vec::new(),
        auto_compaction_paused_after: None,
        background_shells: Vec::new(),
        config,
        theme_overrides: overrides.slots(),
    };
    let interactions = engine.take_history();

    let mut tui = terminal::init()?;
    let EngineHandle { ops, events, task } = engine.spawn();
    let mut app = App::new(
        theme,
        overrides,
        session_info,
        startup.git_branch,
        events,
        ops,
        AppHistory {
            interactions,
            title: startup.title,
            prior: startup.prior,
        },
    );
    for notice in theme_notices {
        app.push_system_message(notice);
    }
    if let Some(picker) = startup.picker {
        app.push_modal(Box::new(picker));
    }
    if let Some(prompt) = startup.prompt {
        app.dispatch_action(Action::Submit(prompt));
    }

    let result = tokio::select! {
        result = app.run(&mut tui) => result.map(|()| None),
        signal = shutdown => {
            debug!(?signal, "TUI received shutdown signal, tearing down");
            Ok(Some(signal))
        }
    };

    terminal::restore();

    // Dropping the app closes the op channel, which the engine reads as `Shutdown`.
    drop(app);
    if let Some(failure) = task.join().await? {
        warn!(error = %failure, "session finish failed");
    }

    result
}

// ── Helpers ──

fn saturating_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Reads the tokio clock, so a test runtime with paused time controls every UI deadline. Outside a
/// runtime it reads the system clock.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}
