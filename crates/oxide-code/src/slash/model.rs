//! `/model` opens the picker, or swaps with `/model <id>`. Resolution: alias → exact / dated-id →
//! unique suffix → unique substring. `[1m]` rejected on models lacking `context_1m`.

use std::borrow::Cow;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::picker::LISTED_MODELS;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::model::{MODELS, ResolvedModelId, display_name, lookup};

// ── Constants ──

const TAG_1M: &str = "[1m]";

const ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-7"),
    ("sonnet", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5"),
];

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
        rank_by_prefix(LISTED_MODELS, prefix, |id| *id)
            .into_iter()
            .map(|id| ArgCompletion {
                value: Cow::Borrowed(*id),
                description: display_name(id),
            })
            .collect()
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            ctx.open_modal(Box::new(super::picker::ModelEffortPicker::new(ctx.info)));
            return Ok(SlashOutcome::Done);
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
    let (base_arg, want_1m) = match arg.strip_suffix(TAG_1M) {
        Some(rest) => (rest, true),
        None => (arg.as_str(), false),
    };
    if base_arg.is_empty() {
        return Err(format!(
            "`{TAG_1M}` is a tag, not a model. Try `/model opus{TAG_1M}` or `/model claude-opus-4-7{TAG_1M}`.",
        ));
    }
    let base_id = resolve_base(base_arg)?;
    if !want_1m {
        return Ok(ResolvedModelId::new(base_id));
    }
    let info = lookup(&base_id).expect("base_id resolves via lookup");
    if !info.capabilities.context_1m {
        return Err(format!(
            "{}: 1M context not supported. Drop the `{TAG_1M}` tag.",
            info.display_name,
        ));
    }
    Ok(ResolvedModelId::new(format!("{base_id}{TAG_1M}")))
}

/// Resolution tiers (first hit wins): short alias → dated id (`<id>-YYYYMMDD`) → known canonical
/// id → unique suffix → unique substring.
fn resolve_base(arg: &str) -> Result<String, String> {
    if let Some(&(_, target)) = ALIASES.iter().find(|(name, _)| *name == arg) {
        return Ok(target.to_owned());
    }
    if is_dated_model_id(arg) {
        return Ok(arg.to_owned());
    }
    if is_selectable_known_id(arg) {
        return Ok(arg.to_owned());
    }
    let suffix_hits = candidates(|id| id.ends_with(arg));
    if let [id] = suffix_hits.as_slice() {
        return Ok((*id).to_owned());
    }
    let visible = candidates(|id| id.contains(arg));
    match visible.as_slice() {
        [id] => Ok((*id).to_owned()),
        [_, ..] => Err(format!(
            "`{arg}` is ambiguous. Pick one or use a short alias (`opus`, `sonnet`, `haiku`):\n\n{list}",
            list = listed_models_matching(arg),
        )),
        [] => Err(format!(
            "Unknown model: `{arg}`. Supported models:\n\n{list}",
            list = format_supported_models(LISTED_MODELS),
        )),
    }
}

fn is_dated_model_id(arg: &str) -> bool {
    MODELS.iter().any(|m| has_dated_suffix(arg, m.id_substr))
}

fn has_dated_suffix(arg: &str, base: &str) -> bool {
    let Some(suffix) = arg.strip_prefix(base) else {
        return false;
    };
    let Some(date) = suffix.strip_prefix('-') else {
        return false;
    };
    date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())
}

fn is_selectable_known_id(arg: &str) -> bool {
    MODELS.iter().any(|m| m.id_substr == arg)
}

fn candidates(pred: impl Fn(&str) -> bool) -> Vec<&'static str> {
    MODELS
        .iter()
        .map(|m| m.id_substr)
        .filter(|id| pred(id))
        .collect()
}

/// `LISTED_MODELS` rows containing `arg` (case-insensitive); falls back to the full curated
/// roster when the filter yields nothing, so the listing always surfaces actionable picks.
fn listed_models_matching(arg: &str) -> String {
    let filtered: Vec<&'static str> = LISTED_MODELS
        .iter()
        .copied()
        .filter(|id| id.contains(arg))
        .collect();
    if filtered.is_empty() {
        format_supported_models(LISTED_MODELS)
    } else {
        format_supported_models(&filtered)
    }
}

/// Markdown bullet list of `id — display name` rows.
fn format_supported_models(ids: &[&'static str]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for id in ids {
        let label = display_name(id);
        _ = writeln!(out, "- `{id}` — {label}");
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn resolved(id: &str) -> ResolvedModelId {
        ResolvedModelId::new(id.to_owned())
    }

    fn swap_model(id: &str) -> SlashOutcome {
        SlashOutcome::Forward(UserAction::SwapConfig {
            model: Some(resolved(id)),
            effort: None,
        })
    }

    // ── ModelCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ModelCmd.name(), "model");
        assert!(ModelCmd.aliases().is_empty());
        assert!(!ModelCmd.description().is_empty());
        assert_eq!(ModelCmd.usage(), Some("[<id>]"));
    }

    #[test]
    fn classify_is_mutating_for_picker_and_direct_forms() {
        assert_eq!(ModelCmd.classify(""), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("   "), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("opus"), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("claude-opus-4-7"), SlashKind::Mutating);
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
    fn complete_arg_empty_prefix_lists_curated_roster_in_picker_order() {
        let expected: Vec<String> = LISTED_MODELS.iter().map(|id| (*id).to_owned()).collect();
        let got: Vec<String> = arg_rows("").into_iter().map(|(v, _)| v).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn complete_arg_prefix_filter_narrows_to_matching_ids() {
        let got: Vec<String> = arg_rows("claude-opus")
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(got, vec!["claude-opus-4-7", "claude-opus-4-7[1m]"]);
    }

    #[test]
    fn complete_arg_appends_1m_context_suffix_only_for_1m_variants() {
        let rows = arg_rows("claude-opus-4-7");
        let one_m = rows
            .iter()
            .find(|(v, _)| v == "claude-opus-4-7[1m]")
            .expect("1M variant present");
        let plain = rows
            .iter()
            .find(|(v, _)| v == "claude-opus-4-7")
            .expect("plain variant present");
        assert!(
            one_m.1.contains("1M context"),
            "1M description: {:?}",
            one_m.1,
        );
        assert!(
            !plain.1.contains("1M context"),
            "plain description must not carry the 1M marker: {:?}",
            plain.1,
        );
    }

    #[test]
    fn complete_arg_is_case_insensitive() {
        let got: Vec<String> = arg_rows("HAIKU").into_iter().map(|(v, _)| v).collect();
        assert_eq!(got, vec!["claude-haiku-4-5"]);
    }

    // ── ModelCmd::execute ──

    fn run_execute(args: &str) -> (ChatView, Result<SlashOutcome, String>) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let outcome = ModelCmd.execute(args, &mut SlashContext::new(&mut chat, &info));
        (chat, outcome)
    }

    #[test]
    fn execute_no_args_opens_picker_via_ctx_and_pushes_no_chat_block() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        let outcome = ModelCmd.execute("", &mut ctx);
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        assert!(
            ctx.take_modal().is_some(),
            "bare /model must populate the modal slot",
        );
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }

    #[test]
    fn execute_with_alias_resolves_to_canonical_id() {
        for (alias, expected) in [
            ("opus", "claude-opus-4-7"),
            ("opus[1m]", "claude-opus-4-7[1m]"),
            ("sonnet", "claude-sonnet-4-6"),
            ("sonnet[1m]", "claude-sonnet-4-6[1m]"),
            ("haiku", "claude-haiku-4-5"),
        ] {
            let (_, outcome) = run_execute(alias);
            assert_eq!(
                outcome,
                Ok(swap_model(expected)),
                "alias `{alias}` should route to `{expected}`",
            );
        }
    }

    #[test]
    fn execute_canonical_id_round_trips_for_bare_and_1m_variants() {
        for id in [
            "claude-opus-4-7",
            "claude-opus-4-7[1m]",
            "claude-opus-4-6",
            "claude-opus-4-6[1m]",
            "claude-sonnet-4-6[1m]",
            "claude-haiku-4-5",
        ] {
            let (_, outcome) = run_execute(id);
            assert_eq!(
                outcome,
                Ok(swap_model(id)),
                "canonical `{id}` must round-trip",
            );
        }
    }

    #[test]
    fn execute_short_id_resolves_via_suffix_tier() {
        for (arg, expected) in [
            ("haiku-4-5", "claude-haiku-4-5"),
            ("opus-4-1", "claude-opus-4-1"),
            ("sonnet-4-5", "claude-sonnet-4-5"),
            ("sonnet-4-6", "claude-sonnet-4-6"),
            ("opus-4-6[1m]", "claude-opus-4-6[1m]"),
        ] {
            let (_, outcome) = run_execute(arg);
            assert_eq!(
                outcome,
                Ok(swap_model(expected)),
                "`{arg}` should resolve to `{expected}`",
            );
        }
    }

    #[test]
    fn execute_unique_substring_resolves_after_suffix_tier_misses() {
        let (_, outcome) = run_execute("haiku-4-");
        assert_eq!(outcome, Ok(swap_model("claude-haiku-4-5")));
    }

    #[test]
    fn execute_haiku_4_resolves_to_only_extant_descendant() {
        // `claude-haiku-4` substring-matches only `claude-haiku-4-5` (Haiku 4 was never released).
        let (_, outcome) = run_execute("claude-haiku-4");
        assert_eq!(outcome, Ok(swap_model("claude-haiku-4-5")));
    }

    #[test]
    fn execute_trims_whitespace_around_arg() {
        let (_, outcome) = run_execute("  haiku-4-5  ");
        assert_eq!(outcome, Ok(swap_model("claude-haiku-4-5")));
    }

    #[test]
    fn execute_1m_on_incompatible_model_is_rejected_with_display_name() {
        for arg in ["haiku[1m]", "claude-haiku-4-5[1m]"] {
            let (_, outcome) = run_execute(arg);
            let msg = outcome.expect_err("must error");
            assert_eq!(
                msg, "Claude Haiku 4.5: 1M context not supported. Drop the `[1m]` tag.",
                "arg `{arg}`",
            );
        }
    }

    #[test]
    fn execute_unknown_arg_errors_with_curated_listing() {
        let (chat, outcome) = run_execute("gpt-4");
        let msg = outcome.expect_err("unknown arg must error");
        assert!(msg.starts_with("Unknown model: `gpt-4`."), "{msg}");
        assert!(msg.contains("Supported models:"), "{msg}");
        for id in LISTED_MODELS {
            assert!(
                msg.contains(&format!("- `{id}` — ")),
                "{id} should appear as a bullet: {msg}",
            );
        }
        assert!(
            msg.contains("Claude Opus 4.7 (1M context)"),
            "1M variant renders the (1M context) suffix: {msg}",
        );
        assert_eq!(chat.entry_count(), 0);
    }

    #[test]
    fn execute_retired_or_legacy_id_falls_through_to_ambiguity() {
        // Retired ids substring-match multiple current rows; ambiguity must surface.
        for arg in ["opus-4", "claude-opus-4", "claude-sonnet-4"] {
            let (_, outcome) = run_execute(arg);
            let msg = outcome.expect_err(&format!("`{arg}` must error"));
            assert!(msg.contains("is ambiguous"), "ambiguity for `{arg}`: {msg}");
        }
    }

    #[test]
    fn execute_ambiguous_substring_filters_curated_listing_by_arg() {
        let (_, outcome) = run_execute("4-6");
        let msg = outcome.expect_err("ambiguous arg must error");
        assert!(msg.starts_with("`4-6` is ambiguous."), "{msg}");
        // `LISTED_MODELS` filtered by `4-6` — only Sonnet entries are curated, so non-listed
        // `claude-opus-4-6` doesn't surface here even though the resolver counted it as a match.
        for needle in ["claude-sonnet-4-6", "claude-sonnet-4-6[1m]"] {
            assert!(msg.contains(needle), "{needle} listed: {msg}");
        }
        assert!(
            !msg.contains("claude-opus-4-6"),
            "non-curated row must not appear: {msg}",
        );
        assert!(msg.contains("opus"), "alias hint surfaces: {msg}");
    }

    #[test]
    fn execute_ambiguous_listing_falls_back_to_full_curated_set_when_filter_empty() {
        let (_, outcome) = run_execute("claude-opus");
        let msg = outcome.expect_err("ambiguous arg must error");
        // `claude-opus` matches the listed Opus 4.7 entries, so the listing surfaces those.
        for id in ["claude-opus-4-7", "claude-opus-4-7[1m]"] {
            assert!(msg.contains(id), "{id} should be listed: {msg}");
        }
        // Older non-listed Opus rows must not appear in the curated listing.
        for id in ["claude-opus-4-5", "claude-opus-4-1"] {
            assert!(
                !msg.contains(id),
                "non-curated `{id}` must not appear: {msg}",
            );
        }
    }

    // ── resolve_model_arg ──

    #[test]
    fn resolve_model_arg_alias_substitution_runs_before_substring_match() {
        assert_eq!(
            resolve_model_arg("opus")
                .as_ref()
                .map(ResolvedModelId::as_str),
            Ok("claude-opus-4-7")
        );
    }

    #[test]
    fn resolve_model_arg_round_trips_every_models_row() {
        for info in MODELS {
            assert_eq!(
                resolve_model_arg(info.id_substr)
                    .as_ref()
                    .map(ResolvedModelId::as_str),
                Ok(info.id_substr),
                "{}",
                info.id_substr,
            );
        }
    }

    #[test]
    fn resolve_model_arg_passes_through_dated_id_via_lookup() {
        for dated in [
            "claude-opus-4-7-20260101",
            "claude-opus-4-6-20250805",
            "claude-sonnet-4-5-20250929",
        ] {
            assert_eq!(
                resolve_model_arg(dated)
                    .as_ref()
                    .map(ResolvedModelId::as_str),
                Ok(dated),
                "{dated} must pass through",
            );
        }
    }

    #[test]
    fn resolve_model_arg_lowercases_arg_before_matching() {
        assert_eq!(
            resolve_model_arg("OPUS")
                .as_ref()
                .map(ResolvedModelId::as_str),
            Ok("claude-opus-4-7")
        );
        assert_eq!(
            resolve_model_arg("Claude-Opus-4-7")
                .as_ref()
                .map(ResolvedModelId::as_str),
            Ok("claude-opus-4-7"),
        );
        assert_eq!(
            resolve_model_arg("OPUS[1M]")
                .as_ref()
                .map(ResolvedModelId::as_str),
            Ok("claude-opus-4-7[1m]"),
        );
    }

    #[test]
    fn resolve_model_arg_rejects_malformed_ids_that_only_contain_known_rows() {
        for arg in [
            "claude-opus-4-7x",
            "foo-claude-opus-4-7",
            "claude-opus-4-7[1m]-bad",
            "claude-opus-4-7-2026010x",
            "claude-opus-4-7-202601011",
        ] {
            assert!(
                resolve_model_arg(arg).is_err(),
                "malformed id must not pass through: {arg}",
            );
        }
    }

    #[test]
    fn resolve_model_arg_bare_1m_tag_errors_without_listing_models() {
        let msg = resolve_model_arg("[1m]").expect_err("must error");
        assert!(msg.contains("tag, not a model"), "{msg}");
        assert!(
            !msg.contains("- `claude"),
            "must not surface the candidate listing: {msg}",
        );
    }

    // ── listed_models_matching ──

    #[test]
    fn listed_models_matching_filters_curated_roster_by_arg() {
        let out = listed_models_matching("haiku");
        assert_eq!(out, "- `claude-haiku-4-5` — Claude Haiku 4.5");
    }

    #[test]
    fn listed_models_matching_surfaces_1m_variants_alongside_base() {
        let out = listed_models_matching("claude-opus");
        assert!(
            out.contains("- `claude-opus-4-7` — Claude Opus 4.7"),
            "{out}"
        );
        assert!(
            out.contains("- `claude-opus-4-7[1m]` — Claude Opus 4.7 (1M context)"),
            "{out}",
        );
    }

    #[test]
    fn listed_models_matching_falls_back_to_full_roster_when_filter_empty() {
        let out = listed_models_matching("zzz-no-match");
        for id in LISTED_MODELS {
            assert!(out.contains(id), "{id} should appear in fallback: {out}");
        }
    }

    // ── format_supported_models ──

    #[test]
    fn format_supported_models_renders_bullets_with_1m_suffix_for_1m_ids() {
        let out = format_supported_models(&["claude-opus-4-7", "claude-opus-4-7[1m]", "gpt-4"]);
        assert_eq!(
            out,
            "- `claude-opus-4-7` — Claude Opus 4.7\n\
             - `claude-opus-4-7[1m]` — Claude Opus 4.7 (1M context)\n\
             - `gpt-4` — gpt-4",
        );
    }

    #[test]
    fn format_supported_models_empty_input_renders_empty_string() {
        assert_eq!(format_supported_models(&[]), "");
    }
}
