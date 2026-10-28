//! `/compact [instructions]` compresses the conversation into a summary, appending any trailing
//! text to the rubric as focus instructions.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome, typed_form};

// ── CompactCmd ──

pub(super) struct CompactCmd;

impl SlashCommand for CompactCmd {
    fn name(&self) -> &'static str {
        "compact"
    }

    fn description(&self) -> &'static str {
        "Compress conversation context into a summary"
    }

    fn idle_only(&self, _args: &str) -> bool {
        true
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<instructions>]")
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        typed_form(self.name(), args, ctx)
    }
}

#[cfg(test)]
mod tests {
    use ox_core::protocol::Op;

    use super::*;
    use crate::action::Action;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn with_args_forwards_the_instructions() {
            assert_eq!(
                run_execute(&CompactCmd, "focus on the build error")
                    .unwrap()
                    .into_forward(),
                Action::Op(Op::Compact {
                    instructions: Some("focus on the build error".to_owned()),
                }),
            );
        }
    }
}
