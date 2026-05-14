//! `/compact [instructions]` compresses the conversation into a summary. Bare runs the default
//! rubric. Trailing text becomes user-supplied focus instructions appended to the rubric.
//! Always [`SlashKind::Mutating`], so it is refused mid-turn while the in-flight reply finishes.

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;

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

    fn execute(&self, args: &str, _ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let trimmed = args.trim();
        let instructions = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        Ok(SlashOutcome::Forward(UserAction::Compact { instructions }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_chat() -> ChatView {
        ChatView::new(&Theme::default(), false)
    }

    // ── CompactCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(CompactCmd.name(), "compact");
        assert!(CompactCmd.aliases().is_empty());
        assert!(!CompactCmd.description().is_empty());
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
        let mut chat = fresh_chat();
        let info = test_session_info();
        let outcome = CompactCmd
            .execute("", &mut SlashContext::new(&mut chat, &info))
            .unwrap();

        assert_eq!(
            outcome,
            SlashOutcome::Forward(UserAction::Compact { instructions: None }),
        );
    }

    #[test]
    fn execute_with_args_forwards_trimmed_instructions() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let outcome = CompactCmd
            .execute(
                "  focus on the build error  ",
                &mut SlashContext::new(&mut chat, &info),
            )
            .unwrap();

        assert_eq!(
            outcome,
            SlashOutcome::Forward(UserAction::Compact {
                instructions: Some("focus on the build error".to_owned()),
            }),
        );
    }

    #[test]
    fn execute_whitespace_only_args_treated_as_bare() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let outcome = CompactCmd
            .execute("   \n\t  ", &mut SlashContext::new(&mut chat, &info))
            .unwrap();
        assert_eq!(
            outcome,
            SlashOutcome::Forward(UserAction::Compact { instructions: None }),
        );
    }
}
