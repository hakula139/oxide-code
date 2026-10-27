//! `/model` opens the picker, or swaps with `/model <id>`, which [`command::model`] resolves.

use std::borrow::Cow;

use ox_core::command;
use ox_core::model::MODELS;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::picker::ModelEffortPicker;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;

// ── ModelCmd ──

pub(super) struct ModelCmd;

impl SlashCommand for ModelCmd {
    fn name(&self) -> &'static str {
        "model"
    }

    fn description(&self) -> &'static str {
        "Switch the model"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id>]")
    }

    fn complete_arg(&self, prefix: &str) -> Vec<ArgCompletion> {
        rank_by_prefix(MODELS, prefix, |info| info.id)
            .into_iter()
            .map(|info| ArgCompletion {
                value: Cow::Borrowed(info.id),
                description: Cow::Borrowed(info.display_name),
            })
            .collect()
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, arg: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if arg.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(ModelEffortPicker::new(
                ctx.info,
            ))));
        }
        Ok(SlashOutcome::Forward(Action::Op(command::model(arg)?)))
    }
}

#[cfg(test)]
mod tests {
    use ox_core::model::ResolvedModelId;
    use ox_core::protocol::{ConfigPick, Op};

    use super::*;
    use crate::slash::registry::testing::run_execute;

    mod classify {
        use super::*;

        #[test]
        fn is_mutating_for_picker_and_direct_forms() {
            assert_eq!(ModelCmd.classify(""), SlashKind::Mutating);
            assert_eq!(ModelCmd.classify("opus"), SlashKind::Mutating);
            assert_eq!(ModelCmd.classify("claude-opus-5-5"), SlashKind::Mutating);
        }
    }

    mod complete_arg {
        use super::*;

        fn arg_rows(prefix: &str) -> Vec<(String, String)> {
            ModelCmd
                .complete_arg(prefix)
                .into_iter()
                .map(|c| (c.value.into_owned(), c.description.into_owned()))
                .collect()
        }

        #[test]
        fn empty_prefix_lists_catalog_in_picker_order() {
            assert_eq!(
                arg_rows(""),
                [
                    ("claude-fable-5-1", "Claude Fable 5.1"),
                    ("claude-opus-5-5", "Claude Opus 5.5"),
                    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                    ("claude-haiku-5-5", "Claude Haiku 5.5"),
                ]
                .map(|(id, name)| (id.to_owned(), name.to_owned())),
            );
        }

        #[test]
        fn filter_is_case_insensitive() {
            let got: Vec<String> = arg_rows("HAIKU").into_iter().map(|(v, _)| v).collect();
            assert_eq!(got, vec!["claude-haiku-5-5"]);
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn no_args_opens_the_picker() {
            run_execute(&ModelCmd, "").unwrap().into_modal();
        }

        #[test]
        fn typed_id_forwards_the_resolved_model() {
            assert_eq!(
                run_execute(&ModelCmd, "sonnet").unwrap().into_forward(),
                Action::Op(Op::SetModel(ConfigPick::Model {
                    id: ResolvedModelId::new("claude-sonnet-5-5"),
                    effort: None,
                })),
            );
        }

        #[test]
        fn unknown_id_forwards_verbatim() {
            assert_eq!(
                run_execute(&ModelCmd, "gpt-4").unwrap().into_forward(),
                Action::Op(Op::SetModel(ConfigPick::Model {
                    id: ResolvedModelId::new("gpt-4"),
                    effort: None,
                })),
            );
        }
    }
}
