//! `/clear` — returns [`UserAction::Clear`] so the dispatcher forwards
//! it to the agent loop, which rolls the session.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};
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

    fn is_read_only(&self, _args: &str) -> bool {
        false
    }

    fn execute(&self, _: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        ctx.chat.clear_history();
        ctx.chat
            .push_system_message("Conversation cleared. Next message starts fresh.");
        Ok(SlashOutcome::Action(UserAction::Clear))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    // ── ClearCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ClearCmd.name(), "clear");
        assert_eq!(ClearCmd.aliases(), &["new", "reset"]);
        assert!(!ClearCmd.description().is_empty());
    }

    #[test]
    fn is_read_only_is_false() {
        // Override to `false` — refuses mid-turn rather than racing
        // the live `messages` / session writer.
        assert!(!ClearCmd.is_read_only(""));
    }

    // ── ClearCmd::execute ──

    #[test]
    fn execute_drops_history_pushes_confirmation_and_returns_clear_action() {
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.push_user_message("prompt".to_owned());
        chat.push_tool_call("$", "ls");
        chat.push_user_message("/clear".to_owned());
        let info = test_session_info();

        let outcome = ClearCmd
            .execute("", &mut SlashContext::new(&mut chat, &info))
            .expect("/clear must succeed");

        assert_eq!(chat.entry_count(), 1, "only the system message remains");
        assert!(!chat.last_is_error());
        assert_eq!(
            chat.last_system_text(),
            Some("Conversation cleared. Next message starts fresh."),
            "confirmation body must match — wording regressions surface here",
        );
        assert_eq!(outcome, SlashOutcome::Action(UserAction::Clear));
    }
}
