//! Mutable view passed into [`SlashCommand::execute`].
//!
//! Holds borrowed handles to the App-owned state each command might
//! need to touch. The struct grows as commands grow — v1 starts with
//! `chat` (push system messages, push errors).

use crate::tui::components::chat::ChatView;

/// Borrowed view of App-owned state for the duration of one
/// `SlashCommand::execute` call. Constructed by the dispatcher, never
/// stored.
pub(crate) struct SlashContext<'a> {
    pub(crate) chat: &'a mut ChatView,
}

impl<'a> SlashContext<'a> {
    pub(crate) fn new(chat: &'a mut ChatView) -> Self {
        Self { chat }
    }
}
