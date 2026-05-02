//! `/model` — list models or swap the active one mid-session.
//!
//! Bare `/model` lists [`crate::model::MODELS`] with the active row
//! marked. `/model <id>` resolves the argument by substring match
//! against the same table and returns
//! [`UserAction::SwitchModel`]; the agent loop calls
//! [`Client::set_model`](crate::client::anthropic::Client::set_model)
//! and emits
//! [`AgentEvent::ModelSwitched`](crate::agent::event::AgentEvent::ModelSwitched),
//! which the App turns into the confirmation system block.
//!
//! Persistence: session-only — see `model.md`.

use std::fmt::Write as _;

use super::context::{SessionInfo, SlashContext};
use super::registry::{SlashCommand, SlashOutcome};
use crate::agent::event::UserAction;
use crate::model::{MODELS, ModelInfo, lookup};

pub(super) struct ModelCmd;

impl SlashCommand for ModelCmd {
    fn name(&self) -> &'static str {
        "model"
    }

    fn description(&self) -> &'static str {
        "List models or switch the active one (effort clamps to the new model)"
    }

    fn is_read_only(&self) -> bool {
        // The arg-swap form races the in-flight `Client`. Refuse
        // both forms uniformly — args-aware classification isn't
        // worth its plumbing for a one-turn wait on the list view.
        false
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<model-id>]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            ctx.chat.push_system_message(render_model_list(ctx.info));
            return Ok(SlashOutcome::Local);
        }
        let id = resolve_model_arg(arg)?;
        Ok(SlashOutcome::Action(UserAction::SwitchModel(id.to_owned())))
    }
}

/// Substring-matches `arg` against each [`MODELS`] row's canonical
/// id. Returns the canonical id on a unique match; `Err` on no
/// match or ambiguity. Family-base ids (`claude-opus-4`) are
/// substrings of every more-specific row, so they always come back
/// ambiguous — that's the intended UX.
fn resolve_model_arg(arg: &str) -> Result<&'static str, String> {
    let matches: Vec<&'static str> = MODELS
        .iter()
        .filter(|info| info.id_substr.contains(arg))
        .map(|info| info.id_substr)
        .collect();
    match matches.as_slice() {
        [] => Err(format!("unknown model: {arg}. Run /model for the list.")),
        [id] => Ok(*id),
        _ => Err(format!(
            "ambiguous: {arg} matches {n} models ({list}). Try a more specific id.",
            n = matches.len(),
            list = matches.join(", "),
        )),
    }
}

/// `id  marketing` table with a `*` on the active row plus the
/// switch hint. Active row is the [`lookup`] resolution of the
/// session's `model_id`; substring equality would also mark family
/// bases when the user is on a 4.x release.
fn render_model_list(info: &SessionInfo) -> String {
    let active_id = lookup(&info.config.model_id).map(|m| m.id_substr);
    let gutter = MODELS.iter().map(|m| m.id_substr.len()).max().unwrap_or(0);
    let mut out = String::from("Available models\n\n");
    for ModelInfo {
        id_substr,
        marketing,
        ..
    } in MODELS
    {
        let marker = if active_id == Some(id_substr) {
            '*'
        } else {
            ' '
        };
        let pad = gutter.saturating_sub(id_substr.len());
        _ = writeln!(
            out,
            "  {marker} {id_substr}{spaces}  {marketing}",
            spaces = " ".repeat(pad),
        );
    }
    out.push_str(
        "\nSwitch with `/model <id>`. A unique substring works (e.g. `/model haiku-4-5`).\n",
    );
    out.push_str(
        "Effort clamps to the new model's ceiling; the user pick is not preserved across swaps.",
    );
    out
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
        assert_eq!(ModelCmd.usage(), Some("[<model-id>]"));
    }

    #[test]
    fn is_read_only_is_false() {
        // The arg-swap form races the in-flight stream — overrides
        // the `true` trait default so the dispatcher refuses mid-turn.
        assert!(!ModelCmd.is_read_only());
    }

    // ── ModelCmd::execute ──

    fn run_execute(args: &str) -> (ChatView, Result<SlashOutcome, String>) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let outcome = ModelCmd.execute(args, &mut SlashContext::new(&mut chat, &info));
        (chat, outcome)
    }

    #[test]
    fn execute_no_args_pushes_list_and_returns_local() {
        let (chat, outcome) = run_execute("");
        assert_eq!(outcome, Ok(SlashOutcome::Local));
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
        let body = chat.last_system_text().expect("system block present");
        for info in MODELS {
            assert!(
                body.contains(info.id_substr),
                "list must mention every id: missing {}",
                info.id_substr,
            );
            assert!(
                body.contains(info.marketing),
                "list must mention every marketing name: missing {}",
                info.marketing,
            );
        }
        assert!(
            body.contains("Switch with `/model <id>`"),
            "list must end with the switch hint: {body}",
        );
    }

    #[test]
    fn execute_no_args_marks_only_the_current_model() {
        // Exactly one `*` even though `claude-opus-4-7` is a superstring
        // of `claude-opus-4` — substring equality on `id_substr` would
        // mark both, so the renderer must use `lookup` resolution.
        let mut chat = ChatView::new(&Theme::default(), false);
        let mut info = test_session_info();
        info.config.model_id = "claude-opus-4-7".to_owned();
        ModelCmd
            .execute("", &mut SlashContext::new(&mut chat, &info))
            .unwrap();
        let body = chat.last_system_text().unwrap();
        let marked: Vec<&str> = body.lines().filter(|l| l.starts_with("  * ")).collect();
        assert_eq!(marked.len(), 1, "exactly one current marker: {marked:?}");
        assert!(
            marked[0].contains("claude-opus-4-7"),
            "marker on the current id: {marked:?}",
        );
    }

    #[test]
    fn execute_with_canonical_id_returns_switch_action() {
        let (chat, outcome) = run_execute("claude-sonnet-4-6");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchModel(
                "claude-sonnet-4-6".to_owned(),
            ))),
        );
        // Confirmation lands later via `ModelSwitched`; execute paints
        // nothing on the success path.
        assert_eq!(chat.entry_count(), 0);
    }

    #[test]
    fn execute_with_unique_substring_resolves_to_canonical_id() {
        let (_, outcome) = run_execute("haiku-4-5");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Action(UserAction::SwitchModel(
                "claude-haiku-4-5".to_owned(),
            ))),
        );
    }

    #[test]
    fn execute_with_unknown_arg_returns_err() {
        let (chat, outcome) = run_execute("gpt-4");
        let msg = outcome.expect_err("unknown arg must error");
        assert!(msg.contains("unknown model"), "{msg}");
        assert!(msg.contains("/model"), "recovery hint: {msg}");
        assert_eq!(
            chat.entry_count(),
            0,
            "execute does not push on the error path",
        );
    }

    #[test]
    fn execute_with_ambiguous_arg_returns_err_listing_candidates() {
        // `sonnet` matches every Sonnet row.
        let (_, outcome) = run_execute("sonnet");
        let msg = outcome.expect_err("ambiguous arg must error");
        assert!(msg.contains("ambiguous"), "{msg}");
        for needle in ["claude-sonnet-4-6", "claude-sonnet-4-5", "claude-sonnet-4"] {
            assert!(
                msg.contains(needle),
                "candidate must be listed for disambiguation: {msg}",
            );
        }
    }

    #[test]
    fn execute_trims_whitespace_around_arg() {
        // Padded input must resolve the same as bare input — dropping
        // the trim would NotFound, since no id has leading whitespace.
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
    fn resolve_model_arg_unique_substring_returns_canonical_id() {
        assert_eq!(resolve_model_arg("opus-4-7"), Ok("claude-opus-4-7"));
        assert_eq!(resolve_model_arg("haiku-4-5"), Ok("claude-haiku-4-5"));
    }

    #[test]
    fn resolve_model_arg_leaf_id_is_a_unique_match() {
        // Versioned leaf ids (`claude-opus-4-7`, etc.) match only
        // themselves. Family-base ids (`claude-opus-4`) are
        // intentionally ambiguous — see `..._family_base_id_is_ambiguous`.
        for leaf in [
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-1",
        ] {
            assert_eq!(resolve_model_arg(leaf), Ok(leaf), "{leaf}");
        }
    }

    #[test]
    fn resolve_model_arg_family_base_id_is_ambiguous() {
        // Pin the intentional ambiguity: the family-base id is a
        // substring of every more-specific row in its family, so
        // typing it must surface the disambiguation error rather
        // than silently route to the experimental base.
        let err = resolve_model_arg("claude-opus-4").unwrap_err();
        assert!(err.starts_with("ambiguous: claude-opus-4"), "{err}");
        for needle in ["claude-opus-4-7", "claude-opus-4-1", "claude-opus-4"] {
            assert!(err.contains(needle), "{err}");
        }
    }

    #[test]
    fn resolve_model_arg_unknown_yields_not_found() {
        let err = resolve_model_arg("gpt-4").unwrap_err();
        assert!(err.contains("unknown model: gpt-4"), "{err}");
    }

    #[test]
    fn resolve_model_arg_ambiguous_lists_every_match() {
        // `opus-4` matches Opus 4.7 / 4.6 / 4.5 / 4.1 and the base
        // family row. The error must list all of them.
        let err = resolve_model_arg("opus-4").unwrap_err();
        assert!(err.starts_with("ambiguous: opus-4"), "{err}");
        for needle in [
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-opus-4-1",
            "claude-opus-4",
        ] {
            assert!(err.contains(needle), "{err}");
        }
    }
}
