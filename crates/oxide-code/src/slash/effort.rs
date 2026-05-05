//! `/effort <level>` — direct effort swap. The bare form errors with a usage hint; users adjust
//! effort interactively via the `/model` picker (Left / Right on the effort row).

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::config::Effort;
use crate::model::{capabilities_for, marketing_or_id};

pub(super) struct EffortCmd;

impl SlashCommand for EffortCmd {
    fn name(&self) -> &'static str {
        "effort"
    }

    fn description(&self) -> &'static str {
        "Set the effort tier with `/effort <level>`"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        // Both bare (error response) and typed (real swap) paths reach `execute`; the typed
        // path races the in-flight client, so gate as Mutating.
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("<level>")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            return Err(format!(
                "Usage: /effort <level>. Valid: {}. Or use /model to pick interactively.",
                Effort::VALID_VALUES,
            ));
        }
        let pick = parse_effort_arg(arg)?;
        // Preflight: setting an explicit level on a no-effort model is
        // a clear user mistake — surface upfront instead of letting it
        // silently resolve to None.
        let caps = capabilities_for(&ctx.info.config.model_id);
        if !caps.effort {
            return Err(format!(
                "{} has no effort tier. Pick an effort-capable model first with /model (e.g. /model opus, /model sonnet).",
                marketing_or_id(&ctx.info.config.model_id),
            ));
        }
        Ok(SlashOutcome::Forward(UserAction::SwapConfig {
            model: None,
            effort: Some(pick),
        }))
    }
}

fn parse_effort_arg(arg: &str) -> Result<Effort, String> {
    let lower = arg.to_ascii_lowercase();
    lower
        .parse()
        .map_err(|_| format!("Unknown effort: `{arg}`. Valid: {}.", Effort::VALID_VALUES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    // ── EffortCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(EffortCmd.name(), "effort");
        assert!(EffortCmd.aliases().is_empty());
        assert!(!EffortCmd.description().is_empty());
        assert_eq!(EffortCmd.usage(), Some("<level>"));
    }

    #[test]
    fn classify_is_mutating_regardless_of_args() {
        assert_eq!(EffortCmd.classify(""), SlashKind::Mutating);
        assert_eq!(EffortCmd.classify("   "), SlashKind::Mutating);
        assert_eq!(EffortCmd.classify("xhigh"), SlashKind::Mutating);
    }

    // ── EffortCmd::execute ──

    fn run_execute(args: &str) -> (ChatView, Result<SlashOutcome, String>) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let outcome = EffortCmd.execute(args, &mut SlashContext::new(&mut chat, &info));
        (chat, outcome)
    }

    fn run_execute_with_model(
        model_id: &str,
        args: &str,
    ) -> (ChatView, Result<SlashOutcome, String>) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let mut info = test_session_info();
        info.config.model_id = model_id.to_owned();
        let outcome = EffortCmd.execute(args, &mut SlashContext::new(&mut chat, &info));
        (chat, outcome)
    }

    #[test]
    fn execute_no_args_errors_with_usage_hint_and_model_pointer() {
        // Bare `/effort` is invalid usage — the typed-arg form is the only direct shortcut;
        // interactive adjustment lives in `/model`.
        let (chat, outcome) = run_execute("");
        let msg = outcome.expect_err("bare /effort must error");
        assert!(msg.contains("Usage: /effort <level>"), "{msg}");
        assert!(msg.contains("/model"), "must point at the picker: {msg}");
        for valid in Effort::VALID_VALUES.split(", ") {
            assert!(msg.contains(valid), "lists `{valid}`: {msg}");
        }
        assert_eq!(chat.entry_count(), 0, "execute must not push on Err");
    }

    #[test]
    fn execute_with_level_forwards_swap_config_with_effort_only() {
        for (arg, level) in [
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::Xhigh),
            ("max", Effort::Max),
        ] {
            let (_, outcome) = run_execute(arg);
            assert_eq!(
                outcome,
                Ok(SlashOutcome::Forward(UserAction::SwapConfig {
                    model: None,
                    effort: Some(level),
                })),
                "`{arg}` should forward SwapConfig {{ effort: Some({level:?}) }}",
            );
        }
    }

    #[test]
    fn execute_explicit_level_on_no_tier_model_errors_with_recovery_hint() {
        // Setting xhigh on Haiku silently resolves to None (no
        // effort), confusing the user. Reject upfront.
        let (chat, outcome) = run_execute_with_model("claude-haiku-4-5", "xhigh");
        let msg = outcome.expect_err("must error");
        assert!(
            msg.contains("Claude Haiku 4.5") && msg.contains("/model"),
            "marketing name + recovery hint: {msg}",
        );
        assert_eq!(chat.entry_count(), 0, "execute must not push on Err");
    }

    #[test]
    fn execute_unknown_level_errors_listing_valid_options() {
        let (chat, outcome) = run_execute("turbo");
        let msg = outcome.expect_err("unknown level must error");
        assert!(msg.starts_with("Unknown effort: `turbo`."), "{msg}");
        for valid in Effort::VALID_VALUES.split(", ") {
            assert!(msg.contains(valid), "lists `{valid}`: {msg}");
        }
        assert!(!msg.contains("auto"), "{msg}");
        assert_eq!(chat.entry_count(), 0, "execute must not push on Err");
    }

    #[test]
    fn execute_auto_is_unknown() {
        let (_, outcome) = run_execute("auto");
        let msg = outcome.expect_err("auto is not a selectable effort");
        assert!(msg.starts_with("Unknown effort: `auto`."), "{msg}");
    }

    // ── parse_effort_arg ──

    #[test]
    fn parse_effort_arg_is_case_insensitive() {
        assert_eq!(parse_effort_arg("XHIGH"), Ok(Effort::Xhigh));
        assert_eq!(parse_effort_arg("MaX"), Ok(Effort::Max));
    }
}
