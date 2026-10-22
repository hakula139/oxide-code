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
    use crate::slash::registry::testing::run_execute;

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
            let outcome = run_execute(&ClearCmd, "").expect("/clear must succeed");
            assert_eq!(outcome.into_forward(), Action::Op(Op::Clear));
        }
    }
}
