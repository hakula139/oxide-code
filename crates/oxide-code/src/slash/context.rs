//! Inputs handed to [`SlashCommand::execute`]. [`SlashContext`] holds
//! borrowed handles to App-owned mutable state; [`SessionInfo`] is a
//! frozen snapshot of read-only session descriptors. Splitting the two
//! keeps the borrow story clean.

use crate::config::ConfigSnapshot;
use crate::tui::components::chat::ChatView;

/// Read-only snapshot of session-level descriptors. Built once at TUI
/// startup; embeds [`ConfigSnapshot`] so `/config` reads its fields
/// without a second plumbing path.
pub(crate) struct SessionInfo {
    /// Marketing display name (e.g. `"Claude Sonnet 4.6"`).
    pub(crate) model: String,
    /// Tildified working directory (`$HOME` rewritten as `~`).
    pub(crate) cwd: String,
    /// Crate version (`env!("CARGO_PKG_VERSION")`).
    pub(crate) version: &'static str,
    /// Active session UUID — useful for `--continue` lookups.
    pub(crate) session_id: String,
    /// Resolved-config view (auth method, model id, effort, ...).
    pub(crate) config: ConfigSnapshot,
}

/// Borrowed view of App-owned state for one
/// [`super::registry::SlashCommand::execute`] call. Never stored.
pub(crate) struct SlashContext<'a> {
    pub(crate) chat: &'a mut ChatView,
    pub(crate) info: &'a SessionInfo,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(chat: &'a mut ChatView, info: &'a SessionInfo) -> Self {
        Self { chat, info }
    }
}
