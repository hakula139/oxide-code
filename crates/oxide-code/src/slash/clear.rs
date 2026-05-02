//! `/clear` — drops chat history client-side, then forwards
//! [`UserAction::Clear`] so the agent loop rolls the session.

use super::context::SlashContext;
use super::registry::SlashCommand;
use crate::agent::event::UserAction;

pub(super) struct ClearCmd;

impl SlashCommand for ClearCmd {
    fn name(&self) -> &'static str {
        "clear"
    }

    fn description(&self) -> &'static str {
        "Reset the Conversation Context"
    }

    fn execute(&self, _: &str, ctx: &mut SlashContext<'_>) -> Result<(), String> {
        ctx.chat.clear_history();
        ctx.chat
            .push_system_message("Conversation cleared. Next message starts fresh.");
        ctx.user_tx
            .try_send(UserAction::Clear)
            .map_err(|e| format!("could not signal agent to clear: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_ctx<'a>(
        chat: &'a mut ChatView,
        info: &'a crate::slash::SessionInfo,
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
        let info = crate::slash::test_session_info();
        let (user_tx, mut user_rx) = crate::slash::test_user_tx();

        ClearCmd
            .execute("", &mut fresh_ctx(&mut chat, &info, &user_tx))
            .unwrap();

        assert_eq!(chat.entry_count(), 1, "only the system message remains");
        assert!(!chat.last_is_error());
        assert!(matches!(user_rx.try_recv(), Ok(UserAction::Clear)));
    }

    #[test]
    fn execute_surfaces_send_failure_when_channel_closed() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = crate::slash::test_session_info();
        let (user_tx, user_rx) = crate::slash::test_user_tx();
        drop(user_rx);

        let err = ClearCmd
            .execute("", &mut fresh_ctx(&mut chat, &info, &user_tx))
            .expect_err("closed channel must error");
        assert!(
            err.contains("could not signal agent to clear"),
            "actionable wording: {err}",
        );
    }

    // ── ClearCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ClearCmd.name(), "clear");
        assert!(!ClearCmd.description().is_empty());
        assert!(ClearCmd.aliases().is_empty());
    }
}
