//! `/status` — open a read-only [`KvOverview`] of the live session: model, effort, cwd,
//! session id, auth source, version, runtime knobs.

use super::context::{LiveSessionInfo, SlashContext};
use super::registry::{SlashCommand, SlashOutcome};
use crate::config::display_effort;
use crate::tui::modal::kv_overview::{KvOverview, KvSection};

pub(super) struct StatusCmd;

impl SlashCommand for StatusCmd {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "Show session info: model, effort, version, working directory, auth source, and session ID"
    }

    fn echoes_input(&self, _args: &str) -> bool {
        false
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        ctx.open_modal(Box::new(build_modal(ctx.info)));
        Ok(SlashOutcome::Done)
    }
}

fn build_modal(info: &LiveSessionInfo) -> KvOverview {
    let model = format!("{} ({})", info.display_name(), info.config.model_id);
    let rows = vec![
        ("Model".to_owned(), model),
        ("Effort".to_owned(), display_effort(info.config.effort)),
        ("Working Directory".to_owned(), info.cwd.clone()),
        ("Session".to_owned(), info.session_id.clone()),
        ("Auth".to_owned(), info.config.auth_label.to_owned()),
        ("Version".to_owned(), info.version.to_owned()),
        (
            "Context Cache".to_owned(),
            info.config.prompt_cache_ttl.to_string(),
        ),
        (
            "Show Thinking".to_owned(),
            on_off(info.config.show_thinking).to_owned(),
        ),
        (
            "Show Welcome".to_owned(),
            on_off(info.config.show_welcome).to_owned(),
        ),
    ];
    KvOverview::new("Status", vec![KvSection::new(rows)])
}

fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::modal::Modal;
    use crate::tui::theme::Theme;

    // ── StatusCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(StatusCmd.name(), "status");
        assert!(StatusCmd.aliases().is_empty());
        assert!(!StatusCmd.description().is_empty());
    }

    // ── StatusCmd::echoes_input ──

    #[test]
    fn echoes_input_is_false_so_the_typed_line_does_not_orphan_after_dismiss() {
        // Modal IS the response — echoing would leave `> /status` alone in chat once dismissed.
        assert!(!StatusCmd.echoes_input(""));
    }

    // ── StatusCmd::execute ──

    #[test]
    fn execute_opens_a_modal_via_ctx_and_pushes_no_chat_block() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        let outcome = StatusCmd.execute("", &mut ctx);
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        assert!(
            ctx.take_modal().is_some(),
            "/status must populate the modal slot",
        );
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }

    // ── build_modal ──

    #[test]
    fn build_modal_renders_one_row_per_session_descriptor() {
        let info = test_session_info();
        let m = build_modal(&info);
        // Title + blank + 9 rows + blank + footer = 13.
        assert_eq!(m.height(80), 13);
    }

    // ── on_off ──

    #[test]
    fn on_off_renders_the_two_flag_states() {
        assert_eq!(on_off(true), "on");
        assert_eq!(on_off(false), "off");
    }
}
