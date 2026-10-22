//! Bare `/effort` opens the Speed ↔ Intelligence slider in [`super::effort_slider`], and
//! `/effort <level>` switches directly.

use std::borrow::Cow;

use ox_core::command;
use ox_core::config::Effort;
use ox_core::model::ModelSpec;

use super::context::SlashContext;
use super::effort_slider::EffortSlider;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;

// ── EffortCmd ──

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
        !args.is_empty()
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

    fn execute(&self, arg: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if arg.is_empty() {
            // An unknown model has nothing to slide, so error with the same recovery hint the
            // typed-arg path uses.
            let Some(slider) = EffortSlider::new(ctx.info) else {
                return Err(no_effort_tier_msg(&ctx.info.config.model));
            };
            return Ok(SlashOutcome::Modal(Box::new(slider)));
        }
        let op = command::effort(arg)?;
        if ctx.info.config.model.info().is_none() {
            return Err(no_effort_tier_msg(&ctx.info.config.model));
        }
        Ok(SlashOutcome::Forward(Action::Op(op)))
    }
}

fn no_effort_tier_msg(model: &ModelSpec) -> String {
    format!(
        "{} has no effort tier. \
         Switch to a supported model first with `/model`, for example `/model opus`.",
        model.display_name(),
    )
}

#[cfg(test)]
mod tests {
    use ox_core::protocol::{ConfigPick, Op};

    use super::*;
    use crate::slash::registry::testing::run_execute;
    use crate::slash::testing::test_session_info;

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(EffortCmd.name(), "effort");
        assert_ne!(EffortCmd.description(), "");
        assert_eq!(EffortCmd.usage(), Some("[<level>]"));
    }

    mod classify {
        use super::*;

        #[test]
        fn is_mutating_for_modal_and_direct_forms() {
            assert_eq!(EffortCmd.classify(""), SlashKind::Mutating);
            assert_eq!(EffortCmd.classify("xhigh"), SlashKind::Mutating);
        }
    }

    mod complete_arg {
        use super::*;

        fn arg_values(prefix: &str) -> Vec<String> {
            EffortCmd
                .complete_arg(prefix)
                .into_iter()
                .map(|c| c.value.into_owned())
                .collect()
        }

        #[test]
        fn empty_prefix_lists_full_ladder_in_intensity_order() {
            assert_eq!(
                arg_values(""),
                vec!["low", "medium", "high", "xhigh", "max"]
            );
        }

        #[test]
        fn prefix_match_pulls_xhigh_above_max_substring() {
            assert_eq!(arg_values("x"), vec!["xhigh", "max"]);
        }

        #[test]
        fn substring_match_lands_after_prefix_tier() {
            assert_eq!(arg_values("high"), vec!["high", "xhigh"]);
        }

        #[test]
        fn is_case_insensitive() {
            assert_eq!(arg_values("MAX"), vec!["max"]);
        }
    }

    mod execute {
        use super::*;

        const NO_TIER_ERROR: &str = "custom-model has no effort tier. \
                                     Switch to a supported model first with `/model`, \
                                     for example `/model opus`.";

        fn run_execute_with_model(model_id: &str, args: &str) -> Result<SlashOutcome, String> {
            let mut info = test_session_info();
            info.config.model = ModelSpec::resolve(model_id);
            EffortCmd.execute(args, &SlashContext::new(&info, None))
        }

        #[test]
        fn no_args_opens_the_slider() {
            run_execute(&EffortCmd, "").unwrap().into_modal();
        }

        #[test]
        fn no_args_on_no_tier_model_errors_with_recovery_hint() {
            let err = run_execute_with_model("custom-model", "").unwrap_err();

            assert_eq!(err, NO_TIER_ERROR);
        }

        #[test]
        fn with_level_forwards_set_model_with_effort_only() {
            assert_eq!(
                run_execute(&EffortCmd, "xhigh").unwrap().into_forward(),
                Action::Op(Op::SetModel(ConfigPick::Effort(Effort::Xhigh))),
            );
        }

        #[test]
        fn unknown_level_errors_before_the_tier_check() {
            let err = run_execute_with_model("custom-model", "turbo").unwrap_err();

            assert_eq!(
                err,
                "Unknown effort: `turbo`. Valid: low, medium, high, xhigh, max."
            );
        }

        #[test]
        fn explicit_level_on_no_tier_model_errors_with_recovery_hint() {
            let err = run_execute_with_model("custom-model", "xhigh").unwrap_err();

            assert_eq!(err, NO_TIER_ERROR);
        }
    }
}
