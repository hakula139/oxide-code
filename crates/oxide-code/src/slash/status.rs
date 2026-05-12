//! `/status` — open a read-only [`KvOverview`] of the live session: model, effort, cwd,
//! session id, auth source, version, runtime knobs.

use super::context::{LiveSessionInfo, SlashContext};
use super::registry::{SlashCommand, SlashOutcome};
use crate::config::{display_auto_compaction, display_bool, display_effort};
use crate::tui::modal::kv_overview::{KvOverview, KvSection};

pub(super) struct StatusCmd;

impl SlashCommand for StatusCmd {
    fn name(&self) -> &'static str {
        "status"
    }

    fn description(&self) -> &'static str {
        "Show current session info"
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
            "Auto Compaction".to_owned(),
            display_auto_compaction(info.config.compaction.auto),
        ),
        (
            "Show Thinking".to_owned(),
            display_bool(info.config.show_thinking).to_owned(),
        ),
        (
            "Show Welcome".to_owned(),
            display_bool(info.config.show_welcome).to_owned(),
        ),
    ];
    KvOverview::new("Status", vec![KvSection::new(rows)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::modal::Modal;
    use crate::tui::theme::Theme;

    fn render_modal(modal: &KvOverview, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let height = modal.height(width);
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // ── StatusCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(StatusCmd.name(), "status");
        assert!(StatusCmd.aliases().is_empty());
        assert!(!StatusCmd.description().is_empty());
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
        // Title + blank + 10 rows + blank + footer = 14.
        assert_eq!(m.height(80), 14);
    }

    #[test]
    fn build_modal_renders_resolved_auto_compaction() {
        let info = test_session_info();
        let m = build_modal(&info);
        let rendered = render_modal(&m, 80);

        assert!(rendered.contains("Auto Compaction"), "{rendered}");
        assert!(rendered.contains("on at 155000 tokens"), "{rendered}");
    }
}
