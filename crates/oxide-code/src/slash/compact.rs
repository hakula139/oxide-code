//! `/compact [instructions]` compresses the conversation into a summary. Bare runs the default
//! rubric. Trailing text becomes user-supplied focus instructions appended to the rubric.
//! Always [`SlashKind::Mutating`], so it is refused mid-turn while the in-flight reply finishes.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::command;
use crate::tui::action::Action;

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
    use super::*;
    use crate::protocol::Op;
    use crate::slash::registry::testing::run_execute;

    // ── CompactCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(CompactCmd.name(), "compact");
        assert_eq!(CompactCmd.aliases(), [] as [&str; 0]);
        assert_ne!(CompactCmd.description(), "");
        assert_eq!(CompactCmd.usage(), Some("[<instructions>]"));
    }

    #[test]
    fn classify_is_always_mutating() {
        assert_eq!(CompactCmd.classify(""), SlashKind::Mutating);
        assert_eq!(CompactCmd.classify("focus on the bug"), SlashKind::Mutating);
    }

    // ── CompactCmd::execute ──

    #[test]
    fn execute_with_args_forwards_trimmed_instructions() {
        assert_eq!(
            run_execute(&CompactCmd, "  focus on the build error  ")
                .unwrap()
                .into_forward(),
            Action::Op(Op::Compact {
                instructions: Some("focus on the build error".to_owned()),
            }),
        );
    }
}
