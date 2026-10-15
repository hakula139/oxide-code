//! `/config` — open a read-only [`KvOverview`] of the resolved config, where each value came from,
//! and the config files that loaded.

use std::path::Path;

use super::context::{LiveSessionInfo, SlashContext};
use super::registry::{SlashCommand, SlashOutcome};
use crate::config::load::user_config_path;
use crate::config::provenance::{Layer, Source};
use crate::config::{
    display_auto_compaction, display_bool, display_effort, display_max_tool_rounds,
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

    fn execute(&self, _args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let user = user_config_path();
        Ok(SlashOutcome::Modal(Box::new(build_modal(
            ctx.info,
            user.as_deref(),
        ))))
    }
}

fn build_modal(info: &LiveSessionInfo, user_path: Option<&Path>) -> KvOverview {
    let cfg = &info.config;
    let sources = &cfg.provenance;
    let row =
        |label: &str, value: String, source: &Source| (label.to_owned(), value, source.to_string());
    let resolved = vec![
        row("Model", cfg.model.display_name().to_owned(), &sources.model),
        row("Model ID", cfg.model.id().to_owned(), &sources.model),
        row("Effort", display_effort(cfg.effort), &sources.effort),
        row("Auth", cfg.auth.to_string(), &sources.auth),
        row("Base URL", cfg.base_url.clone(), &sources.base_url),
        row(
            "Extra CA Certs",
            cfg.extra_ca_certs
                .as_deref()
                .map_or_else(|| "(none)".to_owned(), tildify),
            &sources.extra_ca_certs,
        ),
        row(
            "Custom Headers",
            display_names(&cfg.gateway.custom_header_names),
            &sources.custom_headers,
        ),
        row(
            "Attribution Header",
            display_bool(cfg.gateway.attribution).to_owned(),
            &sources.attribution,
        ),
        row(
            "Structured Outputs",
            display_bool(cfg.gateway.structured_outputs).to_owned(),
            &sources.structured_outputs,
        ),
        row(
            "Max Tokens",
            cfg.model.resolve_max_tokens(cfg.max_tokens).to_string(),
            &sources.max_tokens,
        ),
        row(
            "Max Tool Rounds",
            display_max_tool_rounds(cfg.max_tool_rounds),
            &sources.max_tool_rounds,
        ),
        row(
            "Prompt Cache TTL",
            cfg.prompt_cache_ttl.to_string(),
            &sources.prompt_cache_ttl,
        ),
        row(
            "Auto Compaction",
            display_auto_compaction(cfg.compaction.auto),
            &sources.compaction,
        ),
        row(
            "Permission Mode",
            cfg.permission_mode.to_string(),
            &sources.permission_mode,
        ),
        row(
            "Show Thinking",
            display_bool(cfg.show_thinking).to_owned(),
            &sources.show_thinking,
        ),
    ];
    KvOverview::new(
        "Configuration",
        vec![
            KvSection::annotated(resolved).with_heading("Resolved"),
            KvSection::new(file_rows(&sources.layers, user_path)).with_heading("Source Files"),
        ],
    )
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "(none)".to_owned()
    } else {
        names.join(", ")
    }
}

/// The loaded files, lowest precedence first, led by where the user file belongs when it did not
/// load.
fn file_rows(layers: &[Layer], user_path: Option<&Path>) -> Vec<(String, String)> {
    let user_loaded = layers.iter().any(|layer| matches!(layer, Layer::User(_)));
    let missing_user = (!user_loaded).then(|| {
        let path = user_path.map_or_else(
            || "(not configured)".to_owned(),
            |path| format!("{} (not found)", tildify(path)),
        );
        ("User".to_owned(), path)
    });
    missing_user
        .into_iter()
        .chain(
            layers
                .iter()
                .map(|layer| (layer_kind(layer).to_owned(), tildify(layer.path()))),
        )
        .collect()
}

fn layer_kind(layer: &Layer) -> &'static str {
    match layer {
        Layer::User(_) => "User",
        Layer::DropIn(_) => "Drop-in",
        Layer::LegacyProject(_) => "Legacy Project",
        Layer::Project(_) => "Project",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::modal::Modal;
    use crate::tui::theme::Theme;
    use crate::util::path::testing::HOME_VAR;

    fn render_modal(modal: &KvOverview, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let height = modal.height(width, u16::MAX);
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
        assert_eq!(ConfigCmd.aliases(), [] as [&str; 0]);
        assert_ne!(ConfigCmd.description(), "");
    }

    // ── ConfigCmd::execute ──

    #[test]
    fn execute_opens_the_overview_modal() {
        let info = test_session_info();
        let outcome = ConfigCmd.execute("", &SlashContext::new(&info, None));
        outcome.unwrap().into_modal();
    }

    // ── build_modal ──

    #[test]
    fn build_modal_height_accounts_for_both_sections() {
        // title + blank + (heading + blank + 15 rows) + blank + (heading + blank + 1 row)
        //   + blank + footer = 2 + 17 + 1 + 3 + 2 = 25.
        let info = test_session_info();
        let m = build_modal(&info, None);
        assert_eq!(m.height(80, u16::MAX), 25);
    }

    #[test]
    fn build_modal_renders_resolved_auto_compaction() {
        let info = test_session_info();
        let m = build_modal(&info, None);
        let rendered = render_modal(&m, 80);

        assert!(rendered.contains("Auto Compaction"), "{rendered}");
        assert!(rendered.contains("at 155000 tokens"), "{rendered}");
    }

    #[test]
    fn build_modal_tildifies_extra_ca_certs_path() {
        // Pin HOME so the test is deterministic regardless of the runner env.
        temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
            let mut info = test_session_info();
            info.config.extra_ca_certs = Some(PathBuf::from("/tmp/oxide-fake-home/certs/corp.pem"));
            let m = build_modal(&info, None);
            let rendered = render_modal(&m, 80);

            assert!(rendered.contains("Extra CA Certs"), "{rendered}");
            assert!(rendered.contains("~/certs/corp.pem"), "{rendered}");
        });
    }

    #[test]
    fn build_modal_renders_extra_ca_certs_none_as_placeholder() {
        let info = test_session_info();
        assert!(info.config.extra_ca_certs.is_none(), "precondition: unset");
        let m = build_modal(&info, None);
        let rendered = render_modal(&m, 80);

        assert!(
            row(&rendered, "Extra CA Certs").contains("(none)"),
            "{rendered}"
        );
    }

    #[test]
    fn build_modal_shows_gateway_settings() {
        let mut info = test_session_info();
        let rendered = render_modal(&build_modal(&info, None), 80);
        assert!(
            row(&rendered, "Custom Headers").contains("(none)"),
            "{rendered}"
        );

        info.config.gateway.custom_header_names =
            vec!["x-team".to_owned(), "x-gateway-key".to_owned()];
        let rendered = render_modal(&build_modal(&info, None), 80);
        assert!(
            row(&rendered, "Custom Headers").contains("x-team, x-gateway-key"),
            "{rendered}"
        );

        info.config.gateway.attribution = false;
        let rendered = render_modal(&build_modal(&info, None), 80);
        assert!(
            row(&rendered, "Attribution Header").contains(display_bool(false)),
            "{rendered}"
        );
        assert!(
            row(&rendered, "Structured Outputs").contains(display_bool(true)),
            "{rendered}"
        );
    }

    #[test]
    fn build_modal_resolves_max_tokens_for_the_current_model() {
        let mut info = test_session_info();
        let assert_max_tokens = |info: &LiveSessionInfo, expected: &str| {
            let rendered = render_modal(&build_modal(info, None), 80);
            let row = row(&rendered, "Max Tokens");
            assert!(row.contains(&format!(" {expected} ")), "{row}");
        };
        assert_max_tokens(&info, "64000");

        info.config.model = crate::model::ModelSpec::resolve("custom-model".to_owned());
        assert_max_tokens(&info, "16000");

        info.config.max_tokens = Some(1_000);
        assert_max_tokens(&info, "1000");
    }

    #[test]
    fn build_modal_shows_where_each_value_came_from() {
        let mut info = test_session_info();
        let drop_in = Layer::DropIn(PathBuf::from("/cfg/ox/config.d/10-nix.toml"));
        info.config.provenance.layers = vec![drop_in.clone()];
        info.config.provenance.model = Source::Env("ANTHROPIC_MODEL");
        info.config.provenance.effort = Source::Session;
        info.config.provenance.max_tokens = Source::File(drop_in);
        let rendered = render_modal(&build_modal(&info, None), 100);

        assert!(
            row(&rendered, "Model ID").ends_with("claude-opus-5-5              $ANTHROPIC_MODEL"),
            "{rendered}"
        );
        assert!(row(&rendered, "Effort").ends_with("session"), "{rendered}");
        assert!(
            row(&rendered, "Max Tokens").ends_with("config.d/10-nix.toml"),
            "{rendered}"
        );
        assert!(row(&rendered, "Base URL").contains("default"), "{rendered}");
    }

    fn row<'a>(rendered: &'a str, key: &str) -> &'a str {
        rendered
            .lines()
            .find(|line| line.contains(key))
            .map_or_else(|| panic!("no {key} row: {rendered}"), str::trim_end)
    }

    // ── file_rows ──

    #[test]
    fn file_rows_list_loaded_layers_lowest_first() {
        temp_env::with_var(HOME_VAR, Some("/home/user"), || {
            let layers = [
                Layer::User(PathBuf::from("/home/user/.config/ox/config.toml")),
                Layer::DropIn(PathBuf::from("/home/user/.config/ox/config.d/10-nix.toml")),
                Layer::LegacyProject(PathBuf::from("/repo/ox.toml")),
                Layer::Project(PathBuf::from("/repo/.ox/config.toml")),
            ];
            let user = PathBuf::from("/home/user/.config/ox/config.toml");
            let pair = |label: &str, path: &str| (label.to_owned(), path.to_owned());
            assert_eq!(
                file_rows(&layers, Some(&user)),
                [
                    pair("User", "~/.config/ox/config.toml"),
                    pair("Drop-in", "~/.config/ox/config.d/10-nix.toml"),
                    pair("Legacy Project", "/repo/ox.toml"),
                    pair("Project", "/repo/.ox/config.toml"),
                ],
            );
        });
    }

    #[test]
    fn file_rows_point_to_a_missing_user_file() {
        temp_env::with_var(HOME_VAR, Some("/home/user"), || {
            let project = [Layer::Project(PathBuf::from("/repo/.ox/config.toml"))];
            let user = PathBuf::from("/home/user/.config/ox/config.toml");
            assert_eq!(
                file_rows(&project, Some(&user)),
                [
                    (
                        "User".to_owned(),
                        "~/.config/ox/config.toml (not found)".to_owned()
                    ),
                    ("Project".to_owned(), "/repo/.ox/config.toml".to_owned()),
                ],
            );
            assert_eq!(
                file_rows(&[], None),
                [("User".to_owned(), "(not configured)".to_owned())],
            );
        });
    }
}
