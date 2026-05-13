//! `/config` — open a read-only [`KvOverview`] of the resolved config plus the layered TOML
//! source paths it was assembled from. Path discovery is per-invocation so mid-session file
//! edits surface immediately.

use std::path::Path;

use super::context::{LiveSessionInfo, SlashContext};
use super::registry::{SlashCommand, SlashOutcome};
use crate::config::{
    display_auto_compaction, display_bool, display_effort, display_max_tool_rounds, file,
};
use crate::tui::modal::kv_overview::{KvOverview, KvSection};
use crate::util::path::tildify;

pub(super) struct ConfigCmd;

impl SlashCommand for ConfigCmd {
    fn name(&self) -> &'static str {
        "config"
    }

    fn description(&self) -> &'static str {
        "Show resolved config"
    }

    fn echoes_input(&self, _args: &str) -> bool {
        false
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let user = file::user_config_path();
        let project = file::find_project_config();
        ctx.open_modal(Box::new(build_modal(
            ctx.info,
            user.as_deref(),
            project.as_deref(),
        )));
        Ok(SlashOutcome::Done)
    }
}

fn build_modal(
    info: &LiveSessionInfo,
    user_path: Option<&Path>,
    project_path: Option<&Path>,
) -> KvOverview {
    let cfg = &info.config;
    let resolved = vec![
        ("Model".to_owned(), info.display_name().into_owned()),
        ("Model ID".to_owned(), cfg.model_id.clone()),
        ("Effort".to_owned(), display_effort(cfg.effort)),
        ("Auth".to_owned(), cfg.auth_label.to_owned()),
        ("Base URL".to_owned(), cfg.base_url.clone()),
        (
            "Extra CA Certs".to_owned(),
            cfg.extra_ca_certs
                .as_deref()
                .map_or_else(|| "(none)".to_owned(), tildify),
        ),
        ("Max Tokens".to_owned(), cfg.max_tokens.to_string()),
        (
            "Max Tool Rounds".to_owned(),
            display_max_tool_rounds(cfg.max_tool_rounds),
        ),
        (
            "Prompt Cache TTL".to_owned(),
            cfg.prompt_cache_ttl.to_string(),
        ),
        (
            "Auto Compaction".to_owned(),
            display_auto_compaction(cfg.compaction.auto),
        ),
        (
            "Show Thinking".to_owned(),
            display_bool(cfg.show_thinking).to_owned(),
        ),
    ];
    let files = vec![
        ("User".to_owned(), display_path(user_path)),
        ("Project".to_owned(), display_path(project_path)),
    ];
    KvOverview::new(
        "Configuration",
        vec![
            KvSection::new(resolved).with_heading("Resolved"),
            KvSection::new(files).with_heading("Source Files"),
        ],
    )
}

/// `(not configured)` when unresolved; `~/...` plus ` (not found)` when missing.
fn display_path(path: Option<&Path>) -> String {
    let Some(path) = path else {
        return "(not configured)".to_owned();
    };
    let pretty = tildify(path);
    if path.exists() {
        pretty
    } else {
        format!("{pretty} (not found)")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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

    // ── ConfigCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ConfigCmd.name(), "config");
        assert!(ConfigCmd.aliases().is_empty());
        assert!(!ConfigCmd.description().is_empty());
    }

    // ── ConfigCmd::execute ──

    #[test]
    fn execute_opens_a_modal_via_ctx_and_pushes_no_chat_block() {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        ConfigCmd.execute("", &mut ctx).unwrap();
        assert!(
            ctx.take_modal().is_some(),
            "/config must populate the modal slot",
        );
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }

    // ── build_modal ──

    #[test]
    fn build_modal_height_accounts_for_both_sections() {
        // title + blank + (heading + blank + 11 rows) + blank + (heading + blank + 2 rows)
        //   + blank + footer = 2 + 13 + 1 + 4 + 2 = 22.
        let info = test_session_info();
        let m = build_modal(&info, None, None);
        assert_eq!(m.height(80), 22);
    }

    #[test]
    fn build_modal_renders_resolved_auto_compaction() {
        let info = test_session_info();
        let m = build_modal(&info, None, None);
        let rendered = render_modal(&m, 80);

        assert!(rendered.contains("Auto Compaction"), "{rendered}");
        assert!(rendered.contains("at 155000 tokens"), "{rendered}");
    }

    #[test]
    fn build_modal_tildifies_extra_ca_certs_path() {
        // Pin HOME so the test is deterministic regardless of the runner env.
        temp_env::with_var("HOME", Some("/tmp/oxide-fake-home"), || {
            let mut info = test_session_info();
            info.config.extra_ca_certs = Some(PathBuf::from("/tmp/oxide-fake-home/certs/corp.pem"));
            let m = build_modal(&info, None, None);
            let rendered = render_modal(&m, 80);

            assert!(rendered.contains("Extra CA Certs"), "{rendered}");
            assert!(rendered.contains("~/certs/corp.pem"), "{rendered}");
        });
    }

    #[test]
    fn build_modal_renders_extra_ca_certs_none_as_placeholder() {
        let info = test_session_info();
        assert!(info.config.extra_ca_certs.is_none(), "precondition: unset");
        let m = build_modal(&info, None, None);
        let rendered = render_modal(&m, 80);

        assert!(rendered.contains("Extra CA Certs"), "{rendered}");
        assert!(rendered.contains("(none)"), "{rendered}");
    }

    // ── display_path ──

    #[test]
    fn display_path_none_renders_not_configured() {
        assert_eq!(display_path(None), "(not configured)");
    }

    #[test]
    fn display_path_missing_file_marks_not_found_after_path() {
        let p = PathBuf::from("/definitely/does/not/exist.toml");
        let got = display_path(Some(&p));
        assert!(got.ends_with("(not found)"), "{got}");
        assert!(got.starts_with("/definitely/"), "path missing: {got}");
    }

    #[test]
    fn display_path_existing_file_is_tildified_value_only() {
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(here.exists(), "test fixture missing");
        let got = display_path(Some(&here));
        assert!(!got.contains("(not found)"), "{got}");
        assert!(!got.contains("(not configured)"), "{got}");
    }
}
