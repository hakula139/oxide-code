//! TUI driver: the [`App`] event loop over the engine's handle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, warn};

use super::{Exit, shutdown_signal};
use crate::config::ConfigSnapshot;
use crate::engine::{Engine, EngineHandle};
use crate::session::entry::CompactInfo;
use crate::slash::{LiveSessionInfo, ResumePicker};
use crate::tool::ToolMetadata;
use crate::tui::app::{App, AppHistory};
use crate::tui::terminal;
use crate::tui::theme::Theme;
use crate::util;
use crate::util::path::tildify;

/// Startup state the TUI shows besides the engine's messages.
pub(crate) struct Startup {
    pub(crate) compact: Option<CompactInfo>,
    /// The `ox --resume` session picker, opened over the first frame.
    pub(crate) picker: Option<ResumePicker>,
    pub(crate) tool_metadata: HashMap<String, ToolMetadata>,
    pub(crate) title: Option<String>,
    /// Resuming the session unarchived it.
    pub(crate) unarchived: bool,
}

/// Runs the TUI until the user quits or a signal arrives, then waits for the engine to finalize the
/// live session.
pub(crate) async fn run(
    engine: Engine,
    theme: &Theme,
    config: ConfigSnapshot,
    show_thinking: bool,
    startup: Startup,
    drifted: &[PathBuf],
) -> Result<Exit> {
    terminal::install_panic_hook();

    let cwd = engine.cwd();
    let unsupported_notice = config.model.unsupported_notice();
    let session_info = LiveSessionInfo {
        cwd: tildify(cwd),
        git_cwd: Some(cwd.to_owned()),
        git_branch: util::git::current_branch(cwd),
        version: env!("CARGO_PKG_VERSION"),
        session_id: engine.session_id().to_owned(),
        config,
    };
    let tools = Arc::clone(engine.tools());
    let messages = engine.messages().to_vec();

    let mut tui = terminal::init()?;
    let EngineHandle { ops, events, task } = engine.spawn(drifted);
    let mut app = App::new(
        theme,
        session_info,
        show_thinking,
        events,
        ops,
        tools,
        AppHistory {
            messages: &messages,
            compact: startup.compact.as_ref(),
            tool_metadata: &startup.tool_metadata,
            title: startup.title,
            unarchived: startup.unarchived,
        },
    );
    if let Some(notice) = unsupported_notice {
        app.push_system_message(notice);
    }
    if let Some(picker) = startup.picker {
        app.push_modal(Box::new(picker));
    }

    let result = tokio::select! {
        result = app.run(&mut tui) => result.map(|()| Exit::Success),
        signal = shutdown_signal() => {
            debug!("TUI received {signal:?}, tearing down");
            Ok(Exit::Signal(signal))
        }
    };

    terminal::restore();

    // Dropping the app closes the op channel, which the engine reads as `Quit`.
    drop(app);
    if let Some(failure) = task.join().await {
        warn!("session finish failed: {failure}");
    }

    result
}
