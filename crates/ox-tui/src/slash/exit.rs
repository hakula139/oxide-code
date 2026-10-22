//! `/exit` forwards [`Op::Shutdown`], which quits `ox` as Ctrl+D on an empty idle input does.

use ox_core::protocol::Op;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;

// ── ExitCmd ──

pub(super) struct ExitCmd;

impl SlashCommand for ExitCmd {
    fn name(&self) -> &'static str {
        "exit"
    }

    fn description(&self) -> &'static str {
        "Quit ox"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Steering
    }

    fn execute(&self, _args: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(Action::Op(Op::Shutdown)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn forwards_the_shutdown() {
            let outcome = run_execute(&ExitCmd, "").expect("/exit must succeed");

            assert_eq!(outcome.into_forward(), Action::Op(Op::Shutdown));
        }
    }
}
