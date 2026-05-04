//! `/model` — list selectable models or swap the active one.
//!
//! Resolution tiers: alias → exact / dated-id → unique suffix → unique substring.
//! `[1m]` is a first-class variant; rejected on models without `context_1m`.

use std::fmt::Write as _;

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_table;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::model::{MODELS, ResolvedModelId, lookup, marketing_or_id};

// ── Constants ──

/// `[1m]` opt-in tag — appended to a canonical id to request the 1M
/// context window on models whose capability row has `context_1m`.
const TAG_1M: &str = "[1m]";

/// Curated roster shown by bare `/model`. Manual swap resolves against
/// the full [`MODELS`] table — this constant only governs what the list
/// view displays.
const LISTED_MODELS: &[&str] = &[
    "claude-opus-4-7",
    "claude-opus-4-7[1m]",
    "claude-sonnet-4-6",
    "claude-sonnet-4-6[1m]",
    "claude-haiku-4-5",
];

/// Short aliases resolved before suffix / substring matching.
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
        "List models or switch the active one"
    }

    fn classify(&self, args: &str) -> SlashKind {
        // Bare lists; the swap form races the in-flight `Client`.
        if args.trim().is_empty() {
            SlashKind::ReadOnly
        } else {
            SlashKind::Mutating
        }
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id>]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            ctx.chat.push_system_message(render_model_list(ctx.info));
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

/// Strips `[1m]`, resolves base, re-attaches if supported. Case-insensitive.
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
            info.marketing,
        ));
    }
    Ok(ResolvedModelId::new(format!("{base_id}{TAG_1M}")))
}

/// Four-tier resolution against [`MODELS`]: alias → exact / dated-id
/// pass-through → unique suffix → unique substring.
fn resolve_base(arg: &str) -> Result<String, String> {
    if let Some(&(_, target)) = ALIASES.iter().find(|(name, _)| *name == arg) {
        return Ok(target.to_owned());
    }
    if is_known_model_id(arg) || is_dated_model_id(arg) {
        return Ok(arg.to_owned());
    }
    if let [id] = candidates(|id| id.ends_with(arg)).as_slice() {
        return Ok((*id).to_owned());
    }
    let matches = candidates(|id| id.contains(arg));
    match matches.as_slice() {
        [id] => Ok((*id).to_owned()),
        [_, ..] => Err(format!(
            "`{arg}` matches {n} models: {list}. Type a more specific id or use a short alias (`opus`, `sonnet`, `haiku`).",
            n = matches.len(),
            list = matches.join(", "),
        )),
        [] => Err(format!(
            "Unknown model: `{arg}`. Run `/model` for selectable shortcuts; \
             any id from the model table works (e.g. `claude-opus-4-6`).",
        )),
    }
}

fn is_known_model_id(arg: &str) -> bool {
    MODELS.iter().any(|m| m.id_substr == arg)
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

fn candidates(pred: impl Fn(&str) -> bool) -> Vec<&'static str> {
    MODELS
        .iter()
        .map(|m| m.id_substr)
        .filter(|id| pred(id))
        .collect()
}

// ── List View ──

/// Renders the selectable model table with active marker.
fn render_model_list(info: &SessionInfo) -> String {
    let active = info.config.model_id.as_str();
    let labels: Vec<String> = LISTED_MODELS
        .iter()
        .map(|id| label_for(id, *id == active))
        .collect();
    let descriptions: Vec<String> = LISTED_MODELS.iter().map(|id| description_for(id)).collect();
    let rows = labels
        .iter()
        .zip(&descriptions)
        .map(|(label, desc)| (label.as_str(), desc.as_str()));

    let mut out = String::from("Available models  (* = active)\n\n");
    write_kv_table(&mut out, rows);

    out.push_str("\nSwitch: /model <id>  (aliases: opus, sonnet, haiku)");

    if !LISTED_MODELS.contains(&active) {
        _ = write!(
            out,
            "\n\nCurrent model: {active} (not in the selectable list).",
        );
    }
    out
}

fn label_for(id: &'static str, active: bool) -> String {
    let marker = if active { '*' } else { ' ' };
    format!("{marker} {id}")
}

/// Marketing name, appending `(1M context)` for `[1m]` variants.
fn description_for(id: &'static str) -> String {
    let name = marketing_or_id(id);
    if id.ends_with("[1m]") {
        format!("{name} (1M context)")
    } else {
        name.into_owned()
    }
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
    fn classify_splits_on_args() {
        // Whitespace-only args route the same as bare.
        assert_eq!(ModelCmd.classify(""), SlashKind::ReadOnly);
        assert_eq!(ModelCmd.classify("   "), SlashKind::ReadOnly);
        assert_eq!(ModelCmd.classify("opus"), SlashKind::Mutating);
        assert_eq!(ModelCmd.classify("claude-opus-4-7"), SlashKind::Mutating);
    }

    // ── ModelCmd::execute ──

    fn run_execute(args: &str) -> (ChatView, Result<SlashOutcome, String>) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let outcome = ModelCmd.execute(args, &mut SlashContext::new(&mut chat, &info));
        (chat, outcome)
    }

    #[test]
    fn execute_no_args_pushes_list_with_legend_and_switch_hint() {
        let (chat, outcome) = run_execute("");
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
        let body = chat.last_system_text().expect("system block present");
        assert!(
            body.starts_with("Available models  (* = active)"),
            "header + legend must lead the output: {body}",
        );
        assert!(body.contains("Switch: /model <id>"), "switch hint: {body}");
        assert!(body.contains("aliases: opus, sonnet, haiku"), "{body}");
    }

    #[test]
    fn execute_no_args_lists_every_selectable_in_declared_order() {
        // Pin the row order — a mutation reversing or sorting the
        // LISTED_MODELS iteration would survive a per-row contains check.
        let (chat, _) = run_execute("");
        let body = chat.last_system_text().unwrap();
        let mut last_idx = 0usize;
        for id in LISTED_MODELS {
            let idx = body
                .find(id)
                .unwrap_or_else(|| panic!("missing {id}: {body}"));
            assert!(
                idx >= last_idx,
                "row order broken: {id} at {idx} before previous row at {last_idx}",
            );
            last_idx = idx;
        }
    }

    #[test]
    fn execute_no_args_marks_only_the_active_row() {
        // Active row is the exact-match against LISTED_MODELS.
        // `claude-opus-4-7` (bare) marks only itself, never
        // `claude-opus-4-7[1m]` — `[1m]` distinctness matters.
        let mut chat = ChatView::new(&Theme::default(), false);
        let mut info = test_session_info();
        info.config.model_id = "claude-opus-4-7".to_owned();
        ModelCmd
            .execute("", &mut SlashContext::new(&mut chat, &info))
            .unwrap();
        let body = chat.last_system_text().unwrap();
        let marked: Vec<&str> = body.lines().filter(|l| l.contains(" * ")).collect();
        assert_eq!(marked.len(), 1, "exactly one marker row: {marked:?}");
        assert!(marked[0].contains("claude-opus-4-7"), "{marked:?}");
        assert!(
            !marked[0].contains("[1m]"),
            "bare id must not match the [1m] row: {marked:?}",
        );
    }

    #[test]
    fn execute_no_args_warns_when_current_model_is_not_selectable() {
        // A user with `model = claude-opus-4-1` set via config gets
        // an unmarked list plus a footer naming their current model
        // so they understand why nothing is starred.
        let mut chat = ChatView::new(&Theme::default(), false);
        let mut info = test_session_info();
        info.config.model_id = "claude-opus-4-1".to_owned();
        ModelCmd
            .execute("", &mut SlashContext::new(&mut chat, &info))
            .unwrap();
        let body = chat.last_system_text().unwrap();
        assert!(
            body.contains("Current model: claude-opus-4-1 (not in the selectable list)"),
            "warning footer expected: {body}",
        );
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
    fn execute_1m_on_incompatible_model_is_rejected_with_marketing_name() {
        // Haiku 4.5 has `context_1m: false`; silent acceptance would
        // degrade to 200K. `haiku[1m]` (alias) and the spelled-out
        // `claude-haiku-4-5[1m]` both route through the same check.
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
    fn execute_canonical_id_round_trips_for_bare_and_1m_variants() {
        // Pass-through tier returns exact table rows unchanged, including
        // non-LISTED_MODELS older rows and 1M variants.
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
        // Suffix tier turns short forms into canonical ids without an
        // explicit alias. The `[1m]` cases pin the strip-resolve-
        // reattach pipeline; bare forms must NOT acquire `[1m]`.
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
    fn execute_unknown_arg_errors_with_recovery_hint() {
        let (chat, outcome) = run_execute("gpt-4");
        let msg = outcome.expect_err("unknown arg must error");
        assert!(
            msg.starts_with("Unknown model: `gpt-4`."),
            "leading capital + backticked input: {msg}",
        );
        assert!(msg.contains("Run `/model`"), "recovery hint: {msg}");
        assert!(
            msg.contains("claude-opus-4-6"),
            "manual-entry example surfaces: {msg}",
        );
        assert_eq!(chat.entry_count(), 0, "execute must not push on Err");
    }

    #[test]
    fn execute_unique_suffix_resolves_above_substring_ambiguity() {
        // `opus-4` is a substring of 5 ids but a suffix of only one —
        // the suffix tier must short-circuit before the substring
        // ambiguity check runs.
        let (_, outcome) = run_execute("opus-4");
        assert_eq!(outcome, Ok(swap_model("claude-opus-4")));
    }

    #[test]
    fn execute_ambiguous_substring_lists_count_and_each_candidate() {
        // `4-6` is neither a unique suffix nor a unique substring —
        // both `claude-opus-4-6` and `claude-sonnet-4-6` end with it.
        // The error must list both candidates and the alias hint.
        let (_, outcome) = run_execute("4-6");
        let msg = outcome.expect_err("ambiguous arg must error");
        assert!(
            msg.starts_with("`4-6` matches"),
            "leading backtick + count substring: {msg}",
        );
        for needle in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            assert!(msg.contains(needle), "candidate `{needle}` listed: {msg}");
        }
        assert!(msg.contains("opus"), "alias hint surfaces: {msg}");
    }

    #[test]
    fn execute_unique_substring_resolves_after_suffix_tier_misses() {
        let (_, outcome) = run_execute("haiku-4-");
        assert_eq!(outcome, Ok(swap_model("claude-haiku-4-5")));
    }

    #[test]
    fn execute_trims_whitespace_around_arg() {
        // Padded input resolves the same as bare input.
        let (_, outcome) = run_execute("  haiku-4-5  ");
        assert_eq!(outcome, Ok(swap_model("claude-haiku-4-5")));
    }

    // ── resolve_model_arg ──

    #[test]
    fn resolve_model_arg_alias_substitution_runs_before_substring_match() {
        // `opus` matches every Opus row as a substring (would be
        // ambiguous), but the alias map intercepts and routes it to
        // the canonical opus-4-7 row. Pin the precedence directly.
        assert_eq!(
            resolve_model_arg("opus")
                .as_ref()
                .map(ResolvedModelId::as_str),
            Ok("claude-opus-4-7")
        );
    }

    #[test]
    fn resolve_model_arg_round_trips_every_models_row() {
        // Drift in MODELS would silently shrink the manual-entry
        // surface — every row must be exactly typeable.
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
        // Anthropic's fully-qualified dated ids round-trip unchanged —
        // `lookup` finds the family row for capability detection but
        // the user's exact string is sent on the wire.
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
    fn resolve_model_arg_lowercases_arg_before_matching() {
        // Mirrors `/effort`'s case-insensitivity so `/model OPUS`
        // doesn't silently fail with "Unknown model".
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
    fn resolve_model_arg_bare_1m_tag_errors_without_listing_models() {
        // `/model [1m]` strips to an empty base — a substring filter
        // would match every row. Reject up front so the user gets a
        // clear "tag, not a model" message instead of a 10-row dump.
        let msg = resolve_model_arg("[1m]").expect_err("must error");
        assert!(msg.contains("tag, not a model"), "{msg}");
        assert!(!msg.contains("matches"), "must not list candidates: {msg}");
    }

    // ── render_model_list ──

    fn render(model_id: &str) -> String {
        let mut info = test_session_info();
        info.config.model_id = model_id.to_owned();
        render_model_list(&info)
    }

    #[test]
    fn render_model_list_marker_column_aligns_within_table() {
        // Pin the column alignment — `write_kv_table` pads to the
        // longest id, and the marker prepended by the active-row
        // logic must not break that gutter. Filter to table rows
        // (have BOTH the canonical id AND the marketing name).
        let body = render("claude-opus-4-7");
        let value_cols: Vec<usize> = body
            .lines()
            .filter(|l| l.contains("claude-") && l.contains("Claude"))
            .map(|l| l.find("Claude").expect("description present"))
            .collect();
        assert_eq!(value_cols.len(), LISTED_MODELS.len(), "row count: {body}");
        assert!(
            value_cols.windows(2).all(|w| w[0] == w[1]),
            "columns not aligned: {value_cols:?} — body: {body}",
        );
    }

    #[test]
    fn render_model_list_appends_1m_context_suffix_to_1m_rows() {
        let body = render("claude-opus-4-7");
        // Every [1m] entry must carry the `(1M context)` suffix
        // so users can tell variants apart in the list.
        for id in LISTED_MODELS.iter().filter(|id| id.ends_with("[1m]")) {
            let row = body
                .lines()
                .find(|l| l.contains(id))
                .unwrap_or_else(|| panic!("row for {id} missing: {body}"));
            assert!(
                row.contains("(1M context)"),
                "1M suffix missing on {id}: {row}",
            );
        }
        // Non-1M rows must NOT carry the suffix.
        let bare_row = body
            .lines()
            .find(|l| l.contains("claude-opus-4-7  "))
            .expect("bare opus-4-7 row");
        assert!(
            !bare_row.contains("(1M context)"),
            "leaked suffix: {bare_row}"
        );
    }
}
