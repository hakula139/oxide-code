//! `/compact [instructions]` compresses the conversation into a summary, appending any trailing
//! text to the rubric as focus instructions.

use ox_core::command;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;

// ── CompactCmd ──

pub(super) struct CompactCmd;

impl SlashCommand for CompactCmd {
    fn name(&self) -> &'static str {
        "compact"
    }

    fn description(&self) -> &'static str {
        "Compress conversation context into a summary"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<instructions>]")
    }

    fn execute(&self, args: &str, _ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(Action::Op(command::compact(args))))
    }
}

#[cfg(test)]
mod tests {
    use ox_core::protocol::Op;

    use super::*;
    use crate::slash::registry::testing::run_execute;

    mod classify {
        use super::*;

        #[test]
        fn is_always_mutating() {
            assert_eq!(CompactCmd.classify(""), SlashKind::Mutating);
            assert_eq!(CompactCmd.classify("focus on the bug"), SlashKind::Mutating);
        }
    }

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
