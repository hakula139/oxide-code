//! `/status` — open the read-only [`StatusModal`](super::status_modal::StatusModal). No args,
//! no chat output — the modal is the surface.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};

pub(super) struct StatusCmd;

impl SlashCommand for StatusCmd {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "Show session info: model, effort, version, working directory, auth source, and session ID"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        ctx.open_modal(Box::new(super::status_modal::StatusModal::new(ctx.info)));
        Ok(SlashOutcome::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    // ── StatusCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(StatusCmd.name(), "status");
        assert!(StatusCmd.aliases().is_empty());
        assert!(!StatusCmd.description().is_empty());
    }

    // ── StatusCmd::execute ──

    #[test]
    fn execute_opens_the_status_modal_via_ctx_and_pushes_no_chat_block() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        let outcome = StatusCmd.execute("", &mut ctx);
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        assert!(
            ctx.take_modal().is_some(),
            "/status must populate the modal slot",
        );
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }
}
