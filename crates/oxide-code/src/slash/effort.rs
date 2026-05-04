//! `/effort` — list / swap the active effort tier mid-session.
//!
//! Bare lists the levels supported by the active model with the current
//! marked. `/effort <level>` swaps to that tier. The agent loop calls
//! [`Client::set_effort`](crate::client::anthropic::Client::set_effort)
//! which clamps against the active model's caps.

use std::fmt::Write as _;

use super::context::{SessionInfo, SlashContext};
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
        "List effort levels or set the active one"
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
            ctx.chat.push_system_message(render_effort_list(ctx.info));
            return Ok(SlashOutcome::Done);
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
        Ok(SlashOutcome::Forward(UserAction::SwitchEffort(pick)))
    }
}

fn parse_effort_arg(arg: &str) -> Result<Effort, String> {
    let lower = arg.to_ascii_lowercase();
    lower
        .parse()
        .map_err(|_| format!("Unknown effort: `{arg}`. Valid: {}.", Effort::VALID_VALUES))
}

/// `* level` list with the active marker, plus a header naming the
/// active model so the user knows which caps the levels reflect.
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

    for level in Effort::ALL
        .iter()
        .copied()
        .filter(|level| caps.accepts_effort(*level))
    {
        let marker = if Some(level) == active { '*' } else { ' ' };
        _ = writeln!(out, "  {marker} {level}");
    }

    out.push_str("\nSwitch with: /effort <level>\n");
    out.push_str("Only levels supported by the active model are shown.");
    out
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
    fn execute_no_args_pushes_list_with_marker_and_swap_hint() {
        let (chat, outcome) = run_execute("");
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        let body = chat.last_system_text().expect("system block present");
        assert!(
            body.starts_with("Effort levels for"),
            "header leads the output: {body}",
        );
        assert!(body.contains("Switch with: /effort <level>"), "{body}");
        assert!(
            body.contains("Only levels supported"),
            "supported-level hint: {body}",
        );
        for level in [Effort::Low, Effort::Medium, Effort::High] {
            assert!(
                body.contains(&level.to_string()),
                "level `{level}` listed: {body}",
            );
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
    fn execute_no_args_hides_unsupported_levels() {
        let (chat, _) = run_execute_with_model("claude-sonnet-4-6", "");
        let body = chat.last_system_text().unwrap();
        for unsupported in ["xhigh", "max"] {
            assert!(
                !body.contains(unsupported),
                "unsupported level `{unsupported}` should not be listed: {body}",
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
                Ok(SlashOutcome::Forward(UserAction::SwitchEffort(level))),
                "`{arg}` should dispatch SwitchEffort({level:?})",
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
