//! Inputs to [`SlashCommand::execute`](super::registry::SlashCommand::execute): [`SlashContext`]
//! borrows App-owned state, and [`LiveSessionInfo`] is the session-level snapshot.

use std::borrow::Cow;
use std::path::PathBuf;

use crate::config::ConfigSnapshot;
use crate::model::{display_name, short_display_name};

/// Live snapshot of the running session handed to every slash command. Built at TUI startup and
/// rebound by `SessionRolled` / `ConfigChanged` so commands always see the current model, effort,
/// session id, and theme. Distinct from [`crate::session::entry::SessionInfo`], which is the
/// persisted JSONL record consumed by `--list`.
pub(crate) struct LiveSessionInfo {
    pub(crate) cwd: String,
    /// The real path behind the display `cwd`, for the status bar's git probes and `/diff`.
    pub(crate) git_cwd: Option<PathBuf>,
    pub(crate) git_branch: Option<String>,
    pub(crate) version: &'static str,
    pub(crate) session_id: String,
    pub(crate) config: ConfigSnapshot,
}

impl LiveSessionInfo {
    pub(crate) fn display_name(&self) -> Cow<'_, str> {
        display_name(&self.config.model_id)
    }

    pub(crate) fn short_display_name(&self) -> Cow<'_, str> {
        short_display_name(&self.config.model_id)
    }
}

/// Read-only App state for one [`super::registry::SlashCommand::execute`] call. Commands report
/// their effects through the returned [`super::registry::SlashOutcome`].
pub(crate) struct SlashContext<'a> {
    pub(crate) info: &'a LiveSessionInfo,
    /// Live session title, which `/rename` pre-fills its modal from. `None` until seeded.
    pub(crate) title: Option<&'a str>,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(info: &'a LiveSessionInfo, title: Option<&'a str>) -> Self {
        Self { info, title }
    }
}
