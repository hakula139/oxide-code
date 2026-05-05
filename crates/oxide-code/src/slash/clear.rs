//! `/clear` — returns [`UserAction::Clear`] so the dispatcher forwards
//! it to the agent loop, which rolls the session.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
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

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn execute(&self, _args: &str, _ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(UserAction::Clear))
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
    fn classify_is_mutating() {
        assert_eq!(ClearCmd.classify(""), SlashKind::Mutating);
    }

    // ── ClearCmd::execute ──

    #[test]
    fn execute_produces_clear_action_without_local_side_effects() {
        let mut chat = ChatView::new(&Theme::default(), false);
        chat.push_user_message("prompt".to_owned());
        let info = test_session_info();

        let outcome = ClearCmd
            .execute("", &mut SlashContext::new(&mut chat, &info))
            .expect("/clear must succeed");

        assert_eq!(outcome, SlashOutcome::Forward(UserAction::Clear));
        assert_eq!(chat.entry_count(), 1, "execute must not touch the chat");
    }
}
