//! `/effort` opens the slider, or swaps with `/effort <level>`. Bare form opens the
//! Speed ↔ Intelligence slider (see [`super::effort_slider`]). Typed arg shortcuts the picker.

use std::borrow::Cow;

use super::context::SlashContext;
use super::effort_slider::EffortSlider;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::config::Effort;
use crate::model::ModelSpec;
use crate::protocol::Op;
use crate::tui::action::Action;

pub(super) struct EffortCmd;

impl SlashCommand for EffortCmd {
    fn name(&self) -> &'static str {
        "effort"
    }

    fn description(&self) -> &'static str {
        "Adjust reasoning effort"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<level>]")
    }

    fn complete_arg(&self, prefix: &str) -> Vec<ArgCompletion> {
        rank_by_prefix(&Effort::ALL, prefix, |level| level.as_str())
            .into_iter()
            .map(|level| ArgCompletion {
                value: Cow::Borrowed(level.as_str()),
                description: Cow::Borrowed(level.description()),
            })
            .collect()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            // An unknown model has nothing to slide, so error with the same recovery hint the
            // typed-arg path uses.
            let Some(slider) = EffortSlider::new(ctx.info) else {
                return Err(no_effort_tier_msg(&ctx.info.config.model));
            };
            return Ok(SlashOutcome::Modal(Box::new(slider)));
        }
        let pick = parse_effort_arg(arg)?;
        if ctx.info.config.model.info().is_none() {
            return Err(no_effort_tier_msg(&ctx.info.config.model));
        }
        Ok(SlashOutcome::Forward(Action::Op(Op::SetModel {
            model: None,
            effort: Some(pick),
        })))
    }
}

fn no_effort_tier_msg(model: &ModelSpec) -> String {
    format!(
        "{} has no effort tier. Switch to a supported model first with /model (e.g. /model opus).",
        model.display_name(),
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

    // ── EffortCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(EffortCmd.name(), "effort");
        assert_eq!(EffortCmd.aliases(), [] as [&str; 0]);
        assert_ne!(EffortCmd.description(), "");
        assert_eq!(EffortCmd.usage(), Some("[<level>]"));
    }

    #[test]
    fn classify_is_mutating_for_modal_and_direct_forms() {
        assert_eq!(EffortCmd.classify(""), SlashKind::Mutating);
        assert_eq!(EffortCmd.classify("   "), SlashKind::Mutating);
        assert_eq!(EffortCmd.classify("xhigh"), SlashKind::Mutating);
    }

    // ── EffortCmd::complete_arg ──

    fn arg_values(prefix: &str) -> Vec<String> {
        EffortCmd
            .complete_arg(prefix)
            .into_iter()
            .map(|c| c.value.into_owned())
            .collect()
    }

    #[test]
    fn complete_arg_empty_prefix_lists_full_ladder_in_intensity_order() {
        assert_eq!(
            arg_values(""),
            vec!["low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn complete_arg_prefix_match_pulls_xhigh_above_max_substring() {
        // `x` prefixes `xhigh`; `max` ends in `x` (substring). Prefix tier wins.
        assert_eq!(arg_values("x"), vec!["xhigh", "max"]);
    }

    #[test]
    fn complete_arg_substring_match_lands_after_prefix_tier() {
        // `high` is a substring of `xhigh` and a prefix of `high` — both tiers fire.
        assert_eq!(arg_values("high"), vec!["high", "xhigh"]);
    }

    #[test]
    fn complete_arg_is_case_insensitive() {
        assert_eq!(arg_values("MAX"), vec!["max"]);
    }

    // ── EffortCmd::execute ──

    fn run_execute(args: &str) -> Result<SlashOutcome, String> {
        run_execute_with_model(test_session_info().config.model.id(), args)
    }

    fn run_execute_with_model(model_id: &str, args: &str) -> Result<SlashOutcome, String> {
        let mut info = test_session_info();
        info.config.model = ModelSpec::resolve(model_id.to_owned());
        EffortCmd.execute(args, &SlashContext::new(&info, None))
    }

    #[test]
    fn execute_no_args_opens_the_slider() {
        run_execute("").unwrap().into_modal();
    }

    #[test]
    fn execute_no_args_on_no_tier_model_errors_with_recovery_hint() {
        let msg = run_execute_with_model("custom-model", "").expect_err("must error");
        assert!(
            msg.contains("custom-model") && msg.contains("/model"),
            "display name + recovery hint: {msg}",
        );
    }

    #[test]
    fn execute_with_level_forwards_set_model_with_effort_only() {
        for (arg, level) in [
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::Xhigh),
            ("max", Effort::Max),
        ] {
            assert_eq!(
                run_execute(arg).unwrap().into_forward(),
                Action::Op(Op::SetModel {
                    model: None,
                    effort: Some(level),
                }),
                "`{arg}` should forward SetModel {{ effort: Some({level:?}) }}",
            );
        }
    }

    #[test]
    fn execute_explicit_level_on_no_tier_model_errors_with_recovery_hint() {
        // Without this, the pick silently resolves to None and confuses the user.
        let msg = run_execute_with_model("custom-model", "xhigh").expect_err("must error");
        assert!(
            msg.contains("custom-model") && msg.contains("/model"),
            "display name + recovery hint: {msg}",
        );
    }

    #[test]
    fn execute_unknown_level_errors_listing_valid_options() {
        let msg = run_execute("turbo").expect_err("unknown level must error");
        assert!(msg.starts_with("Unknown effort: `turbo`."), "{msg}");
        for valid in Effort::VALID_VALUES.split(", ") {
            assert!(msg.contains(valid), "lists `{valid}`: {msg}");
        }
        assert!(!msg.contains("auto"), "{msg}");
    }

    #[test]
    fn execute_auto_is_unknown() {
        let msg = run_execute("auto").expect_err("auto is not a selectable effort");
        assert!(msg.starts_with("Unknown effort: `auto`."), "{msg}");
    }

    // ── parse_effort_arg ──

    #[test]
    fn parse_effort_arg_is_case_insensitive() {
        assert_eq!(parse_effort_arg("XHIGH"), Ok(Effort::Xhigh));
        assert_eq!(parse_effort_arg("MaX"), Ok(Effort::Max));
    }
}
