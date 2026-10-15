//! `/compact [instructions]` compresses the conversation into a summary. Bare runs the default
//! rubric. Trailing text becomes user-supplied focus instructions appended to the rubric.
//! Always [`SlashKind::Mutating`], so it is refused mid-turn while the in-flight reply finishes.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::protocol::Op;
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
        let trimmed = args.trim();
        let instructions = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        Ok(SlashOutcome::Forward(Action::Op(Op::Compact {
            instructions,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    fn run_execute(args: &str) -> Action {
        let info = test_session_info();
        CompactCmd
            .execute(args, &SlashContext::new(&info, None))
            .unwrap()
            .into_forward()
    }

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
    fn execute_bare_forwards_compact_action_with_no_instructions() {
        assert_eq!(
            run_execute(""),
            Action::Op(Op::Compact { instructions: None }),
        );
    }

    #[test]
    fn execute_with_args_forwards_trimmed_instructions() {
        assert_eq!(
            run_execute("  focus on the build error  "),
            Action::Op(Op::Compact {
                instructions: Some("focus on the build error".to_owned()),
            }),
        );
    }

    #[test]
    fn execute_whitespace_only_args_treated_as_bare() {
        assert_eq!(
            run_execute("   \n\t  "),
            Action::Op(Op::Compact { instructions: None }),
        );
    }
}
