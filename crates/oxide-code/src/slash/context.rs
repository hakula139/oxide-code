//! Inputs to [`SlashCommand::execute`](super::registry::SlashCommand::execute): [`SlashContext`]
//! borrows App-owned state, and [`LiveSessionInfo`] is the session-level snapshot.

use std::path::PathBuf;

use crate::config::ConfigSnapshot;
use crate::permission::rule::Rule;

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
    /// Rules granted from approvals for the rest of this session.
    pub(crate) session_allow: Vec<Rule>,
    pub(crate) config: ConfigSnapshot,
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
