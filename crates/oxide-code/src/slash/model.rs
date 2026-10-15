//! `/model` opens the picker, or swaps with `/model <id>`. Resolution: exact catalogue id →
//! unique substring, so the family names `opus`, `sonnet`, and `haiku` work as aliases.

use std::borrow::Cow;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::model::{MODELS, ModelInfo, ResolvedModelId, replacement_for};

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

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
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

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(
                super::picker::ModelEffortPicker::new(ctx.info),
            )));
        }
        let id = resolve_model_arg(arg)?;
        Ok(SlashOutcome::Forward(UserAction::SwapConfig {
            model: Some(id),
            effort: None,
        }))
    }
}

// ── Resolver ──

fn resolve_model_arg(arg: &str) -> Result<ResolvedModelId, String> {
    let arg = arg.to_ascii_lowercase();
    let hits: Vec<&ModelInfo> = match MODELS.iter().find(|info| info.id == arg) {
        Some(exact) => vec![exact],
        None => MODELS
            .iter()
            .filter(|info| info.id.contains(&arg))
            .collect(),
    };
    match hits.as_slice() {
        [info] => Ok(ResolvedModelId::new(info.id.to_owned())),
        [_, ..] => Err(format!(
            "`{arg}` is ambiguous. Pick one or use a short alias (`opus`, `sonnet`, `haiku`):\n\n{}",
            format_models(hits),
        )),
        [] => {
            let hint = replacement_for(&arg)
                .map_or_else(String::new, |info| format!(" Did you mean `{}`?", info.id));
            Err(format!(
                "Unknown model: `{arg}`.{hint} Supported models:\n\n{}",
                format_models(MODELS),
            ))
        }
    }
}

/// Markdown bullet list of `id — display name` rows.
fn format_models<'a>(models: impl IntoIterator<Item = &'a ModelInfo>) -> String {
    models
        .into_iter()
        .map(|info| format!("- `{}` — {}", info.id, info.display_name))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;

    fn swap_model(id: &str) -> UserAction {
        UserAction::SwapConfig {
            model: Some(ResolvedModelId::new(id.to_owned())),
            effort: None,
        }
    }

    // ── ModelCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ModelCmd.name(), "model");
        assert_eq!(ModelCmd.aliases(), [] as [&str; 0]);
        assert_ne!(ModelCmd.description(), "");
        assert_eq!(ModelCmd.usage(), Some("[<id>]"));
    }

    #[test]
    fn classify_is_mutating_for_picker_and_direct_forms() {
        assert_eq!(ModelCmd.classify(""), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("   "), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("opus"), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("claude-opus-5-5"), SlashKind::Mutating);
    }

    // ── ModelCmd::complete_arg ──

    fn arg_rows(prefix: &str) -> Vec<(String, String)> {
        ModelCmd
            .complete_arg(prefix)
            .into_iter()
            .map(|c| (c.value.into_owned(), c.description.into_owned()))
            .collect()
    }

    #[test]
    fn complete_arg_empty_prefix_lists_catalogue_in_picker_order() {
        assert_eq!(
            arg_rows(""),
            [
                ("claude-opus-5-5", "Claude Opus 5.5"),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                ("claude-haiku-5-5", "Claude Haiku 5.5"),
            ]
            .map(|(id, name)| (id.to_owned(), name.to_owned())),
        );
    }

    #[test]
    fn complete_arg_filter_is_case_insensitive() {
        let got: Vec<String> = arg_rows("HAIKU").into_iter().map(|(v, _)| v).collect();
        assert_eq!(got, vec!["claude-haiku-5-5"]);
    }

    // ── ModelCmd::execute ──

    fn run_execute(args: &str) -> Result<SlashOutcome, String> {
        ModelCmd.execute(args, &SlashContext::new(&test_session_info(), None))
    }

    fn run_forward(args: &str) -> UserAction {
        run_execute(args).unwrap().into_forward()
    }

    #[test]
    fn execute_no_args_opens_the_picker() {
        run_execute("").unwrap().into_modal();
    }

    #[test]
    fn execute_family_alias_resolves_to_catalogue_id() {
        for (alias, expected) in [
            ("opus", "claude-opus-5-5"),
            ("sonnet", "claude-sonnet-5-5"),
            ("haiku", "claude-haiku-5-5"),
        ] {
            assert_eq!(
                run_forward(alias),
                swap_model(expected),
                "alias `{alias}` should route to `{expected}`",
            );
        }
    }

    #[test]
    fn execute_trims_and_lowercases_arg() {
        assert_eq!(
            run_forward("  Claude-Sonnet-5-5  "),
            swap_model("claude-sonnet-5-5")
        );
    }

    #[test]
    fn execute_unknown_arg_errors_with_full_listing() {
        // Retired ids and a `[1m]` suffix also get the 5.5 model they name as a suggestion.
        for (arg, head) in [
            ("gpt-4", "Unknown model: `gpt-4`. Supported models:"),
            (
                "claude-opus-4-8",
                "Unknown model: `claude-opus-4-8`. Did you mean `claude-opus-5-5`? Supported models:",
            ),
            (
                "sonnet[1m]",
                "Unknown model: `sonnet[1m]`. Did you mean `claude-sonnet-5-5`? Supported models:",
            ),
        ] {
            let msg = run_execute(arg).expect_err("unknown arg must error");
            assert!(msg.starts_with(head), "{msg}");
            for info in MODELS {
                assert!(
                    msg.contains(&format!("- `{}` — {}", info.id, info.display_name)),
                    "{} should appear as a bullet: {msg}",
                    info.id,
                );
            }
        }
    }

    // ── resolve_model_arg ──

    #[test]
    fn resolve_model_arg_round_trips_every_models_row() {
        for info in MODELS {
            assert_eq!(
                resolve_model_arg(info.id)
                    .as_ref()
                    .map(ResolvedModelId::as_str),
                Ok(info.id),
            );
        }
    }

    #[test]
    fn resolve_model_arg_ambiguous_substring_lists_only_matching_rows() {
        let msg = resolve_model_arg("o").expect_err("ambiguous arg must error");
        assert_eq!(
            msg,
            "`o` is ambiguous. Pick one or use a short alias (`opus`, `sonnet`, `haiku`):\n\n\
             - `claude-opus-5-5` — Claude Opus 5.5\n\
             - `claude-sonnet-5-5` — Claude Sonnet 5.5",
        );
    }
}
