//! `/clear` forwards [`Op::Clear`]. The engine rolls the session.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::protocol::Op;
use crate::tui::action::Action;

// ── ClearCmd ──

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

    fn execute(&self, _args: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(Action::Op(Op::Clear)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    // ── ClearCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(ClearCmd.name(), "clear");
        assert_eq!(ClearCmd.aliases(), &["new", "reset"]);
        assert_ne!(ClearCmd.description(), "");
    }

    // ── ClearCmd::classify ──

    #[test]
    fn classify_is_mutating() {
        assert_eq!(ClearCmd.classify(""), SlashKind::Mutating);
    }

    // ── ClearCmd::execute ──

    #[test]
    fn execute_forwards_the_clear_action() {
        let info = test_session_info();
        let outcome = ClearCmd
            .execute("", &SlashContext::new(&info, None))
            .expect("/clear must succeed");
        assert_eq!(outcome.into_forward(), Action::Op(Op::Clear));
    }
}
