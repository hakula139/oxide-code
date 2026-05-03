//! `/effort` — list / swap the active effort tier mid-session.
//!
//! Bare lists the levels for the active model with the current marked
//! and unsupported levels annotated. `/effort <level>` swaps to that
//! tier; `/effort auto` (alias `unset`) clears the user pick so the
//! model's default kicks in. The agent loop calls
//! [`Client::set_effort`](crate::client::anthropic::Client::set_effort)
//! which clamps against the active model's caps. See `effort.md`.

use std::fmt::Write as _;

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_table;
use super::registry::{SlashCommand, SlashOutcome};
use crate::agent::event::UserAction;
use crate::config::Effort;
use crate::model::capabilities_for;
use crate::prompt::environment::marketing_or_id;

/// Levels presented in the list view, weakest first.
const LEVELS: &[(Effort, &str)] = &[
    (Effort::Low, "low"),
    (Effort::Medium, "medium"),
    (Effort::High, "high"),
    (Effort::Xhigh, "xhigh"),
    (Effort::Max, "max"),
];

/// Keywords that clear the user pick so the model default kicks in.
const AUTO_KEYWORDS: &[&str] = &["auto", "unset"];

pub(super) struct EffortCmd;

impl SlashCommand for EffortCmd {
    fn name(&self) -> &'static str {
        "effort"
    }

    fn description(&self) -> &'static str {
        "List effort levels or set the active one"
    }

    fn is_read_only(&self, args: &str) -> bool {
        args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<level>|auto]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            ctx.chat.push_system_message(render_effort_list(ctx.info));
            return Ok(SlashOutcome::Local);
        }
        let pick = parse_effort_arg(arg)?;
        // Preflight: setting an explicit level on a no-effort model is
        // a clear user mistake — surface upfront instead of letting it
        // silently resolve to None.
        let caps = capabilities_for(&ctx.info.config.model_id);
        if pick.is_some() && !caps.effort {
            return Err(format!(
                "{} has no effort tier. Pick an effort-capable model first with /model (e.g. /model opus, /model sonnet).",
                marketing_or_id(&ctx.info.config.model_id),
            ));
        }
        Ok(SlashOutcome::Action(UserAction::SwitchEffort(pick)))
    }
}

/// Map the level keyword to `Some(Effort)`; `auto` / `unset` to `None`.
/// Case-insensitive.
fn parse_effort_arg(arg: &str) -> Result<Option<Effort>, String> {
    let lower = arg.to_ascii_lowercase();
    if AUTO_KEYWORDS.contains(&lower.as_str()) {
        return Ok(None);
    }
    LEVELS
        .iter()
        .find(|(_, name)| *name == lower)
        .map(|(level, _)| Some(*level))
        .ok_or_else(|| {
            format!("Unknown effort: `{arg}`. Valid: low, medium, high, xhigh, max, auto.")
        })
}

/// `* level  note` table with the active marker, plus a header naming
/// the active model so the user knows which caps the levels reflect.
fn render_effort_list(info: &SessionInfo) -> String {
    let marketing = marketing_or_id(&info.config.model_id);
    let caps = capabilities_for(&info.config.model_id);
    let active = info.config.effort;

    let mut out = format!("Effort levels for {marketing}  (* = active)\n\n");

    if !caps.effort {
        _ = writeln!(out, "  (no effort tier — {marketing} ignores effort)");
        out.push_str("\nSwitch models first with /model.");
        return out;
    }

    let labels: Vec<String> = LEVELS
        .iter()
        .map(|(level, name)| {
            let marker = if Some(*level) == active { '*' } else { ' ' };
            format!("{marker} {name}")
        })
        .collect();
    let notes: Vec<String> = LEVELS
        .iter()
        .map(|(level, _)| level_note(*level, caps))
        .collect();
    let rows = labels
        .iter()
        .zip(&notes)
        .map(|(l, n)| (l.as_str(), n.as_str()));
    write_kv_table(&mut out, rows);

    out.push_str("\nSwitch with: /effort <level>\n");
    out.push_str(
        "Use /effort auto to clear (fall back to model default). \
         Unsupported levels clamp down to the model's ceiling.",
    );
    out
}

/// Empty for supported levels; `(clamps to high)` for unsupported.
/// Kept terse so the table column stays narrow.
fn level_note(level: Effort, caps: crate::model::Capabilities) -> String {
    if caps.accepts_effort(level) {
        return String::new();
    }
    let ceiling = caps
        .clamp_effort(level)
        .map_or_else(|| "unsupported".to_owned(), |c| format!("clamps to {c}"));
    format!("({ceiling})")
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
        assert_eq!(EffortCmd.usage(), Some("[<level>|auto]"));
    }

    #[test]
    fn is_read_only_splits_on_args() {
        assert!(EffortCmd.is_read_only(""));
        assert!(EffortCmd.is_read_only("   "));
        assert!(!EffortCmd.is_read_only("xhigh"));
        assert!(!EffortCmd.is_read_only("auto"));
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
    fn execute_no_args_pushes_list_with_marker_and_swap_hint() {
        let (chat, outcome) = run_execute("");
        assert_eq!(outcome, Ok(SlashOutcome::Local));
        let body = chat.last_system_text().expect("system block present");
        assert!(
            body.starts_with("Effort levels for"),
            "header leads the output: {body}",
        );
        assert!(body.contains("Switch with: /effort <level>"), "{body}");
        assert!(body.contains("/effort auto"), "auto hint: {body}");
        for (_, name) in LEVELS {
            assert!(body.contains(name), "level `{name}` listed: {body}");
        }
    }

    #[test]
    fn execute_no_args_marks_only_the_active_level() {
        // `test_session_info` ships effort=High; assert both the row
        // count AND that "high" is the marked one so a misrouted
        // marker (e.g. always-mark-low regression) fails here.
        let (chat, _) = run_execute("");
        let body = chat.last_system_text().unwrap();
        let marked: Vec<&str> = body.lines().filter(|l| l.contains(" * ")).collect();
        assert_eq!(marked.len(), 1, "exactly one marker row: {marked:?}");
        assert!(
            marked[0].contains("high"),
            "active row marks `high`: {marked:?}",
        );
    }

    #[test]
    fn execute_no_args_annotates_unsupported_levels_with_clamp_target() {
        // Sonnet 4.6 supports low/medium/high but not xhigh/max — both
        // should be marked `(clamps to high)` so the user knows what
        // will happen if they pick.
        let (chat, _) = run_execute_with_model("claude-sonnet-4-6", "");
        let body = chat.last_system_text().unwrap();
        for unsupported in ["xhigh", "max"] {
            let row = body
                .lines()
                .find(|l| l.contains(unsupported))
                .unwrap_or_else(|| panic!("row for {unsupported} missing: {body}"));
            assert!(
                row.contains("clamps to high"),
                "clamp annotation on {unsupported}: {row}",
            );
        }
    }

    #[test]
    fn execute_no_args_warns_when_active_model_has_no_effort_tier() {
        let (chat, _) = run_execute_with_model("claude-haiku-4-5", "");
        let body = chat.last_system_text().unwrap();
        assert!(
            body.contains("no effort tier") && body.contains("/model"),
            "no-tier warning + recovery hint: {body}",
        );
    }

    #[test]
    fn execute_with_level_dispatches_switch_effort() {
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
                Ok(SlashOutcome::Action(UserAction::SwitchEffort(Some(level)))),
                "`{arg}` should dispatch SwitchEffort({level:?})",
            );
        }
    }

    #[test]
    fn execute_auto_dispatches_switch_effort_none() {
        for arg in ["auto", "unset", "AUTO", "  auto  "] {
            let (_, outcome) = run_execute(arg);
            assert_eq!(
                outcome,
                Ok(SlashOutcome::Action(UserAction::SwitchEffort(None))),
                "`{arg}` should clear the pick",
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
    fn execute_auto_on_no_tier_model_dispatches_to_clear() {
        // `auto` on a no-effort model is harmless — nothing to clear,
        // but the loop emits an EffortSwitched(None, None) the user
        // can read as confirmation.
        let (_, outcome) = run_execute_with_model("claude-haiku-4-5", "auto");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchEffort(None))),
        );
    }

    #[test]
    fn execute_unknown_level_returns_error_listing_valid_options() {
        let (chat, outcome) = run_execute("turbo");
        let msg = outcome.expect_err("unknown level must error");
        assert!(msg.starts_with("Unknown effort: `turbo`."), "{msg}");
        for valid in ["low", "medium", "high", "xhigh", "max", "auto"] {
            assert!(msg.contains(valid), "lists `{valid}`: {msg}");
        }
        assert_eq!(chat.entry_count(), 0, "execute must not push on Err");
    }

    // ── parse_effort_arg ──

    #[test]
    fn parse_effort_arg_is_case_insensitive() {
        assert_eq!(parse_effort_arg("XHIGH"), Ok(Some(Effort::Xhigh)));
        assert_eq!(parse_effort_arg("MaX"), Ok(Some(Effort::Max)));
        assert_eq!(parse_effort_arg("AUTO"), Ok(None));
    }
}
