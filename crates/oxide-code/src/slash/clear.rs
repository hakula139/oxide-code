//! `/clear` — forwards [`UserAction::Clear`] so the agent loop rolls
//! the session, then drops the chat-history view. Order matters: if
//! the action can't be queued, the chat stays intact and the user
//! sees only the error.

use super::context::SlashContext;
use super::registry::SlashCommand;
use crate::agent::event::UserAction;

pub(super) struct ClearCmd;

impl SlashCommand for ClearCmd {
    fn name(&self) -> &'static str {
        "clear"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["new", "reset"]
    }

    fn description(&self) -> &'static str {
        "Reset the conversation context"
    }

    fn execute(&self, _: &str, ctx: &mut SlashContext<'_>) -> Result<(), String> {
        ctx.user_tx
            .try_send(UserAction::Clear)
            .map_err(|e| format!("could not signal agent to clear: {e}"))?;
        ctx.chat.clear_history();
        ctx.chat
            .push_system_message("Conversation cleared. Next message starts fresh.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::{SessionInfo, test_session_info, test_user_tx};
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_ctx<'a>(
        chat: &'a mut ChatView,
        info: &'a SessionInfo,
        user_tx: &'a tokio::sync::mpsc::Sender<UserAction>,
    ) -> SlashContext<'a> {
        SlashContext::new(chat, info, user_tx)
    }

    // ── ClearCmd::execute ──

    #[test]
    fn execute_drops_history_pushes_confirmation_and_forwards_user_action() {
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.push_user_message("prompt".to_owned());
        chat.push_tool_call("$", "ls");
        chat.push_user_message("/clear".to_owned());
        let info = test_session_info();
        let (user_tx, mut user_rx) = test_user_tx();

        ClearCmd
            .execute("", &mut fresh_ctx(&mut chat, &info, &user_tx))
            .unwrap();

        assert_eq!(chat.entry_count(), 1, "only the system message remains");
        assert!(!chat.last_is_error());
        assert_eq!(
            chat.last_system_text(),
            Some("Conversation cleared. Next message starts fresh."),
            "confirmation body must match — wording regressions surface here",
        );
        assert!(matches!(user_rx.try_recv(), Ok(UserAction::Clear)));
    }

    #[test]
    fn execute_preserves_chat_when_channel_closed() {
        // Send-first ordering — if the agent can't accept the Clear
        // action, the visible chat must stay intact so the user isn't
        // staring at an empty pane while seeing an error block below.
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.push_user_message("prompt".to_owned());
        chat.push_tool_call("$", "ls");
        let pre_count = chat.entry_count();
        let info = test_session_info();
        let (user_tx, user_rx) = test_user_tx();
        drop(user_rx);

        let err = ClearCmd
            .execute("", &mut fresh_ctx(&mut chat, &info, &user_tx))
            .expect_err("closed channel must error");
        assert!(err.contains("could not signal agent to clear"), "{err}");
        assert_eq!(chat.entry_count(), pre_count, "chat must survive failure");
        assert!(!chat.last_is_error());
    }

    // ── ClearCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ClearCmd.name(), "clear");
        assert_eq!(ClearCmd.aliases(), &["new", "reset"]);
        assert!(!ClearCmd.description().is_empty());
    }
}
