//! Inputs handed to [`SlashCommand::execute`]. [`SlashContext`] holds
//! borrowed handles to App-owned mutable state; [`SessionInfo`] is the
//! session-level descriptor snapshot. Splitting the two keeps the
//! borrow story clean.

use std::borrow::Cow;

use crate::config::ConfigSnapshot;
use crate::model::marketing_or_id;
use crate::tui::components::chat::ChatView;

/// Session-level descriptors surfaced by read-only slash commands.
/// Built at TUI startup, then rebound mid-session by
/// [`AgentEvent::SessionRolled`](crate::agent::event::AgentEvent::SessionRolled)
/// (`/clear`),
/// [`AgentEvent::ModelSwitched`](crate::agent::event::AgentEvent::ModelSwitched)
/// (`/model`), and
/// [`AgentEvent::EffortSwitched`](crate::agent::event::AgentEvent::EffortSwitched)
/// (`/effort`). Embeds [`ConfigSnapshot`] so `/config` reads from a
/// single source.
pub(crate) struct SessionInfo {
    /// Tildified working directory (`$HOME` rewritten as `~`).
    pub(crate) cwd: String,
    /// Crate version (`env!("CARGO_PKG_VERSION")`).
    pub(crate) version: &'static str,
    /// Active session UUID — useful for `--continue` lookups.
    pub(crate) session_id: String,
    /// Resolved-config view (auth method, model id, effort, ...).
    pub(crate) config: ConfigSnapshot,
}

impl SessionInfo {
    /// Marketing display name derived from the live `config.model_id`
    /// (e.g. `"Claude Sonnet 4.6"`). Single seam — caching the name
    /// alongside `model_id` would just be a stale-state risk.
    pub(crate) fn marketing_name(&self) -> Cow<'_, str> {
        marketing_or_id(&self.config.model_id)
    }
}

/// Borrowed view of App-owned state for one
/// [`super::registry::SlashCommand::execute`] call. Never stored.
/// State-mutating commands return [`super::registry::SlashOutcome::Action`];
/// the dispatcher owns forwarding to the agent loop.
pub(crate) struct SlashContext<'a> {
    pub(crate) chat: &'a mut ChatView,
    pub(crate) info: &'a SessionInfo,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(chat: &'a mut ChatView, info: &'a SessionInfo) -> Self {
        Self { chat, info }
    }
}
