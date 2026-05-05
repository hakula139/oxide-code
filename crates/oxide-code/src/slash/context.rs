//! Inputs handed to [`SlashCommand::execute`]. [`SlashContext`] borrows App-owned mutable state;
//! [`SessionInfo`] is the session-level descriptor snapshot.

use std::borrow::Cow;

use crate::config::ConfigSnapshot;
use crate::model::marketing_or_id;
use crate::tui::components::chat::ChatView;
use crate::tui::modal::Modal;

/// Session-level descriptors surfaced by read-only slash commands. Built at TUI startup, rebound
/// mid-session by `SessionRolled` / `ConfigChanged` events.
pub(crate) struct SessionInfo {
    pub(crate) cwd: String,
    pub(crate) version: &'static str,
    pub(crate) session_id: String,
    pub(crate) config: ConfigSnapshot,
}

impl SessionInfo {
    /// Marketing display name derived from the live `config.model_id`.
    pub(crate) fn marketing_name(&self) -> Cow<'_, str> {
        marketing_or_id(&self.config.model_id)
    }
}

/// Borrowed App-owned state for one [`super::registry::SlashCommand::execute`] call. Commands
/// open modals via [`SlashContext::open_modal`]; the dispatcher harvests the slot after `execute`.
pub(crate) struct SlashContext<'a> {
    pub(crate) chat: &'a mut ChatView,
    pub(crate) info: &'a SessionInfo,
    modal: Option<Box<dyn Modal>>,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(chat: &'a mut ChatView, info: &'a SessionInfo) -> Self {
        Self {
            chat,
            info,
            modal: None,
        }
    }

    /// Open `modal` after this command finishes. Only one modal per dispatch is meaningful.
    pub(crate) fn open_modal(&mut self, modal: Box<dyn Modal>) {
        debug_assert!(self.modal.is_none(), "modal slot set twice in one dispatch");
        self.modal = Some(modal);
    }

    /// Take the modal slot, if any. Called once by the dispatcher after `execute` returns.
    pub(crate) fn take_modal(&mut self) -> Option<Box<dyn Modal>> {
        self.modal.take()
    }
}
