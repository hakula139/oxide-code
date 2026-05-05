//! `/effort` — open the slider, or swap with `/effort <level>`. Bare form opens the
//! Speed↔Intelligence slider (see [`super::effort_slider`]); typed arg shortcuts the picker.

use super::context::SlashContext;
use super::effort_slider::EffortSlider;
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
        "Open the effort slider or switch directly with `/effort <level>`"
    }

    fn classify(&self, args: &str) -> SlashKind {
        if args.trim().is_empty() {
            SlashKind::ReadOnly
        } else {
            SlashKind::Mutating
        }
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<level>]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            // No-effort model has nothing to slide — error with the same recovery hint the
            // typed-arg path uses.
            let Some(slider) = EffortSlider::new(ctx.info) else {
                return Err(no_effort_tier_msg(&ctx.info.config.model_id));
            };
            ctx.open_modal(Box::new(slider));
            return Ok(SlashOutcome::Done);
        }
        let pick = parse_effort_arg(arg)?;
        let caps = capabilities_for(&ctx.info.config.model_id);
        if !caps.effort {
            return Err(no_effort_tier_msg(&ctx.info.config.model_id));
        }
        Ok(SlashOutcome::Forward(UserAction::SwapConfig {
            model: None,
            effort: Some(pick),
        }))
    }
}

fn no_effort_tier_msg(model_id: &str) -> String {
    format!(
        "{} has no effort tier. Pick an effort-capable model first with /model (e.g. /model opus, /model sonnet).",
        marketing_or_id(model_id),
    )
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
        assert_eq!(EffortCmd.usage(), Some("[<level>]"));
    }

    #[test]
    fn classify_splits_on_args() {
        // Bare form opens the slider (read-only); typed arg races the client (mutating).
        assert_eq!(EffortCmd.classify(""), SlashKind::ReadOnly);
        assert_eq!(EffortCmd.classify("   "), SlashKind::ReadOnly);
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
    fn execute_no_args_opens_slider_via_ctx_and_pushes_no_chat_block() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        let outcome = EffortCmd.execute("", &mut ctx);
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        assert!(
            ctx.take_modal().is_some(),
            "bare /effort must populate the modal slot",
        );
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }

    #[test]
    fn execute_no_args_on_no_tier_model_errors_with_recovery_hint() {
        // Slider can't render zero tiers — error before opening the modal.
        let (chat, outcome) = run_execute_with_model("claude-haiku-4-5", "");
        let msg = outcome.expect_err("must error");
        assert!(
            msg.contains("Claude Haiku 4.5") && msg.contains("/model"),
            "marketing name + recovery hint: {msg}",
        );
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
        // Setting xhigh on Haiku silently resolves to None (no effort), confusing the user.
        // Reject upfront.
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
