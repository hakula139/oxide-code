//! Inputs to [`SlashCommand::execute`]: [`SlashContext`] borrows App-owned mutable state;
//! [`LiveSessionInfo`] is the session-level snapshot.

use std::borrow::Cow;

use crate::config::ConfigSnapshot;
use crate::model::marketing_or_id;
use crate::tui::components::chat::ChatView;
use crate::tui::modal::Modal;

/// Live snapshot of the running session handed to every slash command. Built at TUI startup and
/// rebound by `SessionRolled` / `ConfigChanged` so commands always see the current model, effort,
/// session id, and theme. Distinct from [`crate::session::entry::SessionInfo`], which is the
/// persisted JSONL record consumed by `--list`.
pub(crate) struct LiveSessionInfo {
    pub(crate) cwd: String,
    pub(crate) version: &'static str,
    pub(crate) session_id: String,
    pub(crate) config: ConfigSnapshot,
}

impl LiveSessionInfo {
    pub(crate) fn marketing_name(&self) -> Cow<'_, str> {
        marketing_or_id(&self.config.model_id)
    }
}

/// Borrowed App-owned state for one [`super::registry::SlashCommand::execute`] call. Open modals
/// via [`SlashContext::open_modal`]; the dispatcher harvests the slot after `execute`.
pub(crate) struct SlashContext<'a> {
    pub(crate) chat: &'a mut ChatView,
    pub(crate) info: &'a LiveSessionInfo,
    modal: Option<Box<dyn Modal>>,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(chat: &'a mut ChatView, info: &'a LiveSessionInfo) -> Self {
        Self {
            chat,
            info,
            modal: None,
        }
    }

    /// Open `modal` after this command finishes. One modal per dispatch.
    pub(crate) fn open_modal(&mut self, modal: Box<dyn Modal>) {
        debug_assert!(self.modal.is_none(), "modal slot set twice in one dispatch");
        self.modal = Some(modal);
    }

    pub(crate) fn take_modal(&mut self) -> Option<Box<dyn Modal>> {
        self.modal.take()
    }
}
