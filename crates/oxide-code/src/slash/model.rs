//! `/model` — list selectable models or swap the active one mid-session.
//!
//! Bare `/model` lists the curated [`SELECTABLE`] set with the active
//! row marked. `/model <arg>` resolves through four tiers against the
//! broader [`crate::model::MODELS`] table: alias map, lookup
//! pass-through (the arg already matches via [`crate::model::lookup`],
//! e.g. dated `claude-opus-4-6-20250805`), unique suffix (so `opus-4`
//! lands on `claude-opus-4` rather than 5-way ambiguous), then unique
//! substring. Manual entry of older or dated ids works even though the
//! curated list only surfaces 4.7. On a unique match, the dispatcher hands
//! [`UserAction::SwitchModel`] to the agent loop, which calls
//! [`Client::set_model`](crate::client::anthropic::Client::set_model)
//! and emits [`AgentEvent::ModelSwitched`](crate::agent::event::AgentEvent::ModelSwitched).
//!
//! `[1m]` is a first-class variant — `/model opus-4-7` means non-1M
//! Opus 4.7; `/model opus-4-7[1m]` means the 1M variant. Typing `[1m]`
//! on a model whose capability table row has `context_1m: false`
//! (e.g. Haiku 4.5) is rejected upfront so the user gets a clear
//! signal instead of a silent fallback to 200K context. See `model.md`
//! § Design Decisions.

use std::fmt::Write as _;

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_table;
use super::registry::{SlashCommand, SlashOutcome};
use crate::agent::event::UserAction;
use crate::model::{MODELS, lookup, marketing_or_id};

/// `[1m]` opt-in tag — appended to a canonical id to request the 1M
/// context window on models whose capability row has `context_1m`.
const TAG_1M: &str = "[1m]";

/// Curated UI surface for the list view. Manual swap accepts any id
/// from [`MODELS`] plus its `[1m]` variant where `context_1m` is true.
const SELECTABLE: &[&str] = &[
    "claude-opus-4-7",
    "claude-opus-4-7[1m]",
    "claude-sonnet-4-6",
    "claude-sonnet-4-6[1m]",
    "claude-haiku-4-5",
];

/// Short aliases for the bare (non-`[1m]`) form. The resolver strips
/// `[1m]` before alias lookup, so `opus[1m]` works without a separate
/// entry — `haiku[1m]` then errors uniformly via the capability check.
const ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-7"),
    ("sonnet", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5"),
];

pub(super) struct ModelCmd;

impl SlashCommand for ModelCmd {
    fn name(&self) -> &'static str {
        "model"
    }

    fn description(&self) -> &'static str {
        "List models or switch the active one"
    }

    fn is_read_only(&self, args: &str) -> bool {
        // Bare `/model` (list view) is safe mid-turn; the swap form
        // races the in-flight `Client` and must wait for idle.
        args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id>]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            ctx.chat.push_system_message(render_model_list(ctx.info));
            return Ok(SlashOutcome::Local);
        }
        let id = resolve_model_arg(arg)?;
        Ok(SlashOutcome::Action(UserAction::SwitchModel(id)))
    }
}

/// Strip `[1m]`, resolve the base id, then re-attach `[1m]` if the
/// model supports 1M context (errors otherwise). Splitting the tag
/// from identity means `opus[1m]` works through the bare alias and
/// `haiku[1m]` errors uniformly — no per-variant table entries.
/// Lowercased at entry so `/model OPUS` and `/effort XHIGH` match
/// the same convention.
fn resolve_model_arg(arg: &str) -> Result<String, String> {
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
        return Ok(base_id);
    }
    let info = lookup(&base_id).expect("base_id resolves via lookup");
    if !info.capabilities.context_1m {
        return Err(format!(
            "{}: 1M context not supported. Drop the `{TAG_1M}` tag.",
            info.marketing,
        ));
    }
    Ok(format!("{base_id}{TAG_1M}"))
}

/// Four-tier resolution against [`MODELS`]: alias → pass-through (arg
/// is already a recognized id, including dated forms like
/// `claude-opus-4-6-20250805`) → unique suffix (so `opus-4` lands on
/// `claude-opus-4`, not 5-way ambiguous) → unique substring.
fn resolve_base(arg: &str) -> Result<String, String> {
    if let Some(&(_, target)) = ALIASES.iter().find(|(name, _)| *name == arg) {
        return Ok(target.to_owned());
    }
    if lookup(arg).is_some() {
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

fn candidates(pred: impl Fn(&str) -> bool) -> Vec<&'static str> {
    MODELS
        .iter()
        .map(|m| m.id_substr)
        .filter(|id| pred(id))
        .collect()
}

/// `* id  marketing` table with a legend header. Active row marker is
/// an exact match between [`SessionInfo`]'s `config.model_id` and a
/// [`SELECTABLE`] entry — `[1m]` distinctness matters because the
/// 1M-tagged variant is a separate selectable row.
fn render_model_list(info: &SessionInfo) -> String {
    let active = info.config.model_id.as_str();
    let labels: Vec<String> = SELECTABLE
        .iter()
        .map(|id| label_for(id, *id == active))
        .collect();
    let descriptions: Vec<String> = SELECTABLE.iter().map(|id| description_for(id)).collect();
    let rows = labels
        .iter()
        .zip(&descriptions)
        .map(|(label, desc)| (label.as_str(), desc.as_str()));

    let mut out = String::from("Available models  (* = active)\n\n");
    write_kv_table(&mut out, rows);

    out.push_str("\nSwitch: /model <id>  (aliases: opus, sonnet, haiku)");

    if !SELECTABLE.contains(&active) {
        _ = write!(
            out,
            "\n\nCurrent model: {active} (not in the selectable list).",
        );
    }
    out
}

/// `* id` on the active row, `  id` otherwise. Width-aligned by
/// [`write_kv_table`]'s gutter so the marker column stays straight.
fn label_for(id: &'static str, active: bool) -> String {
    let marker = if active { '*' } else { ' ' };
    format!("{marker} {id}")
}

/// Marketing name + ` (1M context)` when the id carries the `[1m]`
/// opt-in tag. [`marketing_or_id`] falls back to the raw id for
/// unknown rows; `[1m]` is stripped by the substring lookup, so
/// the marketing name comes from the bare row.
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

    // ── ModelCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ModelCmd.name(), "model");
        assert!(ModelCmd.aliases().is_empty());
        assert!(!ModelCmd.description().is_empty());
        assert_eq!(ModelCmd.usage(), Some("[<id>]"));
    }

    #[test]
    fn is_read_only_splits_on_args() {
        // Bare list form stays read-only; arg-bearing form refuses
        // mid-turn. Whitespace-only args route the same as bare.
        assert!(ModelCmd.is_read_only(""));
        assert!(ModelCmd.is_read_only("   "));
        assert!(!ModelCmd.is_read_only("opus"));
        assert!(!ModelCmd.is_read_only("claude-opus-4-7"));
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
        assert_eq!(outcome, Ok(SlashOutcome::Local));
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
        // SELECTABLE iteration would survive a per-row contains check.
        let (chat, _) = run_execute("");
        let body = chat.last_system_text().unwrap();
        let mut last_idx = 0usize;
        for id in SELECTABLE {
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
        // Active row is the exact-match against SELECTABLE.
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
                Ok(SlashOutcome::Action(UserAction::SwitchModel(
                    expected.to_owned(),
                ))),
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
        // Pass-through tier returns the arg unchanged when `lookup`
        // recognizes it. Covers every canonical id including
        // non-SELECTABLE older rows and their 1M variants.
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
                Ok(SlashOutcome::Action(UserAction::SwitchModel(id.to_owned()))),
                "canonical `{id}` must round-trip",
            );
        }
    }

    #[test]
    fn execute_short_id_resolves_via_suffix_tier() {
        // Suffix tier turns short forms into canonical ids without
        // an explicit alias entry. Covers non-SELECTABLE rows like
        // `opus-4-1` that are reachable only by manual entry.
        for (arg, expected) in [
            ("haiku-4-5", "claude-haiku-4-5"),
            ("opus-4-1", "claude-opus-4-1"),
            ("sonnet-4-5", "claude-sonnet-4-5"),
        ] {
            let (_, outcome) = run_execute(arg);
            assert_eq!(
                outcome,
                Ok(SlashOutcome::Action(UserAction::SwitchModel(
                    expected.to_owned()
                ))),
                "`{arg}` should resolve to `{expected}`",
            );
        }
    }

    #[test]
    fn execute_bare_arg_resolves_to_bare_row_not_1m_variant() {
        // `sonnet-4-6` (no [1m]) is the non-1M variant. The strip-
        // resolve-reattach pipeline never re-attaches [1m] when the
        // arg lacks it, so the bare row wins without ambiguity.
        let (_, outcome) = run_execute("sonnet-4-6");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchModel(
                "claude-sonnet-4-6".to_owned(),
            ))),
        );
    }

    #[test]
    fn execute_1m_arg_re_attaches_tag_after_resolving_base() {
        // Strip `[1m]`, resolve `opus-4-6` via suffix, re-attach `[1m]`
        // because Opus 4.6 caps allow it. Pin the round-trip so a
        // regression in the strip-reattach pipeline shows up here.
        let (_, outcome) = run_execute("opus-4-6[1m]");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchModel(
                "claude-opus-4-6[1m]".to_owned(),
            ))),
        );
    }

    #[test]
    fn execute_unknown_arg_returns_error_with_recovery_hint() {
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
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchModel(
                "claude-opus-4".to_owned(),
            ))),
        );
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
    fn execute_trims_whitespace_around_arg() {
        // Padded input resolves the same as bare input.
        let (_, outcome) = run_execute("  haiku-4-5  ");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchModel(
                "claude-haiku-4-5".to_owned(),
            ))),
        );
    }

    // ── resolve_model_arg ──

    #[test]
    fn resolve_model_arg_alias_substitution_runs_before_substring_match() {
        // `opus` matches every Opus row as a substring (would be
        // ambiguous), but the alias map intercepts and routes it to
        // the canonical opus-4-7 row. Pin the precedence directly.
        assert_eq!(resolve_model_arg("opus").as_deref(), Ok("claude-opus-4-7"));
    }

    #[test]
    fn resolve_model_arg_round_trips_every_models_row() {
        // Drift in MODELS would silently shrink the manual-entry
        // surface — every row must be exactly typeable.
        for info in MODELS {
            assert_eq!(
                resolve_model_arg(info.id_substr).as_deref(),
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
                resolve_model_arg(dated).as_deref(),
                Ok(dated),
                "{dated} must pass through",
            );
        }
    }

    #[test]
    fn resolve_model_arg_lowercases_arg_before_matching() {
        // Mirrors `/effort`'s case-insensitivity so `/model OPUS`
        // doesn't silently fail with "Unknown model".
        assert_eq!(resolve_model_arg("OPUS").as_deref(), Ok("claude-opus-4-7"));
        assert_eq!(
            resolve_model_arg("Claude-Opus-4-7").as_deref(),
            Ok("claude-opus-4-7"),
        );
        assert_eq!(
            resolve_model_arg("OPUS[1M]").as_deref(),
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
        assert_eq!(value_cols.len(), SELECTABLE.len(), "row count: {body}");
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
        for id in SELECTABLE.iter().filter(|id| id.ends_with("[1m]")) {
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
