//! `/clear` forwards [`Op::Clear`]. The engine rolls the session.

use ox_core::protocol::Op;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;

// ── ClearCmd ──

pub(super) struct ClearCmd;

impl SlashCommand for ClearCmd {
    fn name(&self) -> &'static str {
        "clear"
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
    use crate::slash::testing::test_session_info;

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(ClearCmd.name(), "clear");
        assert_ne!(ClearCmd.description(), "");
    }

    mod classify {
        use super::*;

        #[test]
        fn is_mutating() {
            assert_eq!(ClearCmd.classify(""), SlashKind::Mutating);
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn forwards_the_clear_action() {
            let info = test_session_info();
            let outcome = ClearCmd
                .execute("", &SlashContext::new(&info, None))
                .expect("/clear must succeed");
            assert_eq!(outcome.into_forward(), Action::Op(Op::Clear));
        }
    }
}
