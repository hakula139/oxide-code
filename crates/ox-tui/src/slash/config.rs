//! `/config` opens a read-only [`KvOverview`] of the resolved config, where each value came from,
//! and the config files that loaded.

use std::path::Path;

use ox_core::config::load::user_config_path;
use ox_core::config::provenance::{Layer, Provenance, Source, TierPattern};
use ox_core::util::path::tildify;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};
use crate::labels;
use crate::modal::kv_overview::{KvOverview, KvSection};
use crate::session_info::LiveSessionInfo;

// ── ConfigCmd ──

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

// ── Helpers ──

fn build_modal(info: &LiveSessionInfo, user_path: Option<&Path>) -> KvOverview {
    let cfg = &info.config;
    let sources = &cfg.provenance;
    let row =
        |label: &str, value: String, source: &Source| (label.to_owned(), value, source.to_string());
    let resolved = vec![
        row("Model", cfg.model.display_name().to_owned(), &sources.model),
        row("Model ID", cfg.model.id().to_owned(), &sources.model),
        row("Effort", labels::effort(cfg.effort), &sources.effort),
        row("Auth", labels::auth(cfg.auth), &sources.auth),
        row("Base URL", cfg.base_url.clone(), &sources.base_url),
        row(
            "Extra CA Certs",
            cfg.extra_ca_certs
                .as_deref()
                .map_or_else(|| labels::NONE.to_owned(), tildify),
            &sources.extra_ca_certs,
        ),
        row(
            "Custom Headers",
            join_or(&cfg.gateway.custom_header_names, labels::NONE),
            &sources.custom_headers,
        ),
        row(
            "Attribution Header",
            labels::on_off(cfg.gateway.attribution).to_owned(),
            &sources.attribution,
        ),
        row(
            "Structured Outputs",
            labels::on_off(cfg.gateway.structured_outputs).to_owned(),
            &sources.structured_outputs,
        ),
        row(
            "Max Tokens",
            cfg.model.resolve_max_tokens(cfg.max_tokens).to_string(),
            &sources.max_tokens,
        ),
        row(
            "Max Tool Rounds",
            labels::max_tool_rounds(cfg.max_tool_rounds),
            &sources.max_tool_rounds,
        ),
        row(
            "Prompt Cache TTL",
            cfg.prompt_cache_ttl.to_string(),
            &sources.prompt_cache_ttl,
        ),
        row(
            "Auto-Compaction",
            cfg.compaction.auto.to_string(),
            &sources.compaction,
        ),
        row(
            "Permission Mode",
            cfg.permission_mode.to_string(),
            &sources.permission_mode,
        ),
        row(
            "Show Thinking",
            labels::on_off(cfg.show_thinking).to_owned(),
            &sources.show_thinking,
        ),
        row(
            "Show Welcome",
            labels::on_off(cfg.show_welcome).to_owned(),
            &sources.show_welcome,
        ),
        row(
            "Status Line",
            join_or(cfg.status_line.iter().map(<&str>::from), labels::NONE),
            &sources.status_line,
        ),
        row("Theme", cfg.theme_name.clone(), &sources.theme),
        (
            "Theme Overrides".to_owned(),
            join_or(&info.theme_overrides, labels::NONE),
            join_or(&sources.theme_overrides, &Source::Default.to_string()),
        ),
    ];
    let mut sections = vec![KvSection::annotated(resolved).with_heading("Resolved")];
    let rules = permission_rows(sources);
    if !rules.is_empty() {
        sections.push(KvSection::annotated(rules).with_heading("Permissions"));
    }
    sections
        .push(KvSection::new(file_rows(&sources.layers, user_path)).with_heading("Source Files"));
    KvOverview::new("Configuration", sections)
}

/// The extra roots, rule patterns, and turned-off default groups the config files set.
fn permission_rows(sources: &Provenance) -> Vec<(String, String, String)> {
    let roots = sources.extra_roots.iter().map(|root| {
        (
            "extra_roots".to_owned(),
            root.value.clone(),
            root.source.to_string(),
        )
    });
    let rules = sources.rules.iter().map(|rule| {
        let TierPattern {
            tier,
            class,
            pattern,
        } = &rule.value;
        (
            format!("{tier}.{class}"),
            pattern.clone(),
            rule.source.to_string(),
        )
    });
    let defaults = sources.disabled_defaults.iter().map(|group| {
        (
            format!("defaults.{}", group.value),
            labels::on_off(false).to_owned(),
            group.source.to_string(),
        )
    });
    roots.chain(rules).chain(defaults).collect()
}

/// `items` joined with commas, or `empty` when there are none.
fn join_or(items: impl IntoIterator<Item = impl ToString>, empty: &str) -> String {
    let items: Vec<String> = items.into_iter().map(|item| item.to_string()).collect();
    if items.is_empty() {
        empty.to_owned()
    } else {
        items.join(", ")
    }
}

/// The loaded files, lowest precedence first, led by where the user file belongs if it is absent.
fn file_rows(layers: &[Layer], user_path: Option<&Path>) -> Vec<(String, String)> {
    let user_loaded = layers.iter().any(|layer| matches!(layer, Layer::User(_)));
    let missing_user = (!user_loaded).then(|| {
        let path = user_path.map_or_else(
            || "not configured".to_owned(),
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
        Layer::Project(_) => "Project",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ox_core::config::StatusLineSegment;
    use ox_core::config::provenance::Sourced;
    use ox_core::model::ModelSpec;
    use ox_core::permission::defaults::Shipped;
    use ox_core::permission::{PermissionClass, Tier};
    use ox_core::util::path::testing::HOME_VAR;

    use super::*;
    use crate::modal::testing::{render_modal, rendered_text};
    use crate::session_info::testing::test_session_info;
    use crate::slash::registry::testing::run_execute;

    mod execute {
        use super::*;

        #[test]
        fn opens_the_overview_modal() {
            let outcome = run_execute(&ConfigCmd, "");
            outcome.unwrap().into_modal();
        }
    }

    mod build_modal {
        use super::*;

        fn row<'a>(rendered: &'a str, key: &str) -> &'a str {
            rendered
                .lines()
                .find(|line| line.contains(key))
                .map_or_else(|| panic!("no {key} row: {rendered}"), str::trim_end)
        }

        #[test]
        fn lists_every_resolved_key_with_its_source() {
            let mut modal = build_modal(&test_session_info(), None);

            insta::assert_debug_snapshot!(render_modal(&mut modal, 80));
        }

        #[test]
        fn lists_the_permission_entries_and_theme_overrides_with_their_layers() {
            let mut info = test_session_info();
            let user = Layer::User(PathBuf::from("/cfg/ox/config.toml"));
            let drop_in = Layer::DropIn(PathBuf::from("/cfg/ox/config.d/10-nix.toml"));
            let project = Layer::Project(PathBuf::from("/repo/.ox/config.toml"));
            info.config.show_welcome = false;
            info.config.status_line = vec![StatusLineSegment::Model, StatusLineSegment::RunState];
            info.theme_overrides = vec!["accent".to_owned(), "error".to_owned()];
            let sources = &mut info.config.provenance;
            sources.layers = vec![user.clone(), drop_in.clone(), project.clone()];
            sources.show_welcome = Source::File(user.clone());
            sources.status_line = Source::Env("OX_STATUS_LINE");
            sources.theme = Source::Session;
            sources.theme_overrides = vec![Source::File(user.clone()), Source::File(drop_in)];
            sources.extra_roots = vec![Sourced {
                value: "~/shared".to_owned(),
                source: Source::File(user.clone()),
            }];
            sources.rules = vec![Sourced {
                value: TierPattern {
                    tier: Tier::Deny,
                    class: PermissionClass::Read,
                    pattern: "./secrets/**".to_owned(),
                },
                source: Source::File(project),
            }];
            sources.disabled_defaults = vec![Sourced {
                value: Shipped::Git,
                source: Source::File(user),
            }];
            let mut modal = build_modal(&info, None);

            insta::assert_debug_snapshot!(render_modal(&mut modal, 80));
        }

        #[test]
        fn renders_resolved_auto_compaction() {
            let info = test_session_info();
            let mut m = build_modal(&info, None);
            let rendered = rendered_text(&mut m, 80);

            assert!(rendered.contains("Auto-Compaction"), "{rendered}");
            assert!(rendered.contains("at 155000 tokens"), "{rendered}");
        }

        #[test]
        fn tildifies_extra_ca_certs_path() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
                let mut info = test_session_info();
                info.config.extra_ca_certs =
                    Some(PathBuf::from("/tmp/oxide-fake-home/certs/corp.pem"));
                let mut m = build_modal(&info, None);
                let rendered = rendered_text(&mut m, 80);

                assert!(rendered.contains("Extra CA Certs"), "{rendered}");
                assert!(rendered.contains("~/certs/corp.pem"), "{rendered}");
            });
        }

        #[test]
        fn shows_none_for_unset_extra_ca_certs() {
            let info = test_session_info();
            assert!(info.config.extra_ca_certs.is_none(), "precondition: unset");
            let mut m = build_modal(&info, None);
            let rendered = rendered_text(&mut m, 80);

            assert!(
                row(&rendered, "Extra CA Certs").contains(" none "),
                "{rendered}"
            );
        }

        #[test]
        fn shows_gateway_settings() {
            let mut info = test_session_info();
            let rendered = rendered_text(&mut build_modal(&info, None), 80);
            assert!(
                row(&rendered, "Custom Headers").contains(" none "),
                "{rendered}"
            );

            info.config.gateway.custom_header_names =
                vec!["x-team".to_owned(), "x-gateway-key".to_owned()];
            let rendered = rendered_text(&mut build_modal(&info, None), 80);
            assert!(
                row(&rendered, "Custom Headers").contains("x-team, x-gateway-key"),
                "{rendered}"
            );

            info.config.gateway.attribution = false;
            let rendered = rendered_text(&mut build_modal(&info, None), 80);
            assert!(
                row(&rendered, "Attribution Header").contains(" off "),
                "{rendered}"
            );
            assert!(
                row(&rendered, "Structured Outputs").contains(" on "),
                "{rendered}"
            );
        }

        #[test]
        fn resolves_max_tokens_for_the_current_model() {
            let mut info = test_session_info();
            let assert_max_tokens = |info: &LiveSessionInfo, expected: &str| {
                let rendered = rendered_text(&mut build_modal(info, None), 80);
                let row = row(&rendered, "Max Tokens");
                assert!(row.contains(&format!(" {expected} ")), "{row}");
            };
            assert_max_tokens(&info, "64000");

            info.config.model = ModelSpec::resolve("custom-model");
            assert_max_tokens(&info, "16000");

            info.config.max_tokens = Some(1_000);
            assert_max_tokens(&info, "1000");
        }

        #[test]
        fn shows_where_each_value_came_from() {
            let mut info = test_session_info();
            let drop_in = Layer::DropIn(PathBuf::from("/cfg/ox/config.d/10-nix.toml"));
            info.config.provenance.layers = vec![drop_in.clone()];
            info.config.provenance.model = Source::Env("OX_MODEL");
            info.config.provenance.effort = Source::Session;
            info.config.provenance.max_tokens = Source::File(drop_in);
            let rendered = rendered_text(&mut build_modal(&info, None), 100);

            assert!(
                row(&rendered, "Model ID").ends_with("claude-opus-5-5              $OX_MODEL"),
                "{rendered}"
            );
            assert!(row(&rendered, "Effort").ends_with("session"), "{rendered}");
            assert!(
                row(&rendered, "Max Tokens").ends_with("config.d/10-nix.toml"),
                "{rendered}"
            );
            assert!(row(&rendered, "Base URL").contains("default"), "{rendered}");
        }
    }

    mod file_rows {
        use super::*;

        #[test]
        fn list_loaded_layers_lowest_first() {
            temp_env::with_var(HOME_VAR, Some("/home/user"), || {
                let layers = [
                    Layer::User(PathBuf::from("/home/user/.config/ox/config.toml")),
                    Layer::DropIn(PathBuf::from("/home/user/.config/ox/config.d/10-nix.toml")),
                    Layer::Project(PathBuf::from("/repo/.ox/config.toml")),
                ];
                let user = PathBuf::from("/home/user/.config/ox/config.toml");
                let pair = |label: &str, path: &str| (label.to_owned(), path.to_owned());
                assert_eq!(
                    file_rows(&layers, Some(&user)),
                    [
                        pair("User", "~/.config/ox/config.toml"),
                        pair("Drop-in", "~/.config/ox/config.d/10-nix.toml"),
                        pair("Project", "/repo/.ox/config.toml"),
                    ],
                );
            });
        }

        #[test]
        fn point_to_a_missing_user_file() {
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
                    [("User".to_owned(), "not configured".to_owned())],
                );
            });
        }
    }
}
