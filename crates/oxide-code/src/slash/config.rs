//! `/config` — read-only view of the resolved effective config.
//!
//! Prints the resolved fields from [`ConfigSnapshot`] plus the
//! filesystem paths the layered TOML files were assembled from
//! (`$XDG_CONFIG_HOME/ox/config.toml`, `./ox.toml`). Layout mirrors
//! `/help` and `/status`: heading, blank line, key-value rows aligned
//! to a shared gutter.
//!
//! Path discovery happens at execute time so the user sees fresh
//! state even if they edited a file mid-session. `[absent]` next to
//! a path means the file isn't there — useful for "is my project
//! `ox.toml` actually being picked up?" debugging.
//!
//! Persistence: this command does not write anywhere. Mutations to
//! runtime state live behind `/model`, `/theme`, etc. and stay
//! session-local (see `docs/research/design/slash-commands.md`).

use std::path::Path;

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_table;
use super::registry::SlashCommand;
use crate::config::file;
use crate::util::path::tildify;

pub(crate) struct ConfigCmd;

impl SlashCommand for ConfigCmd {
    fn name(&self) -> &'static str {
        "config"
    }

    fn description(&self) -> &'static str {
        "show resolved config (read-only)"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<(), String> {
        let user = file::user_config_path();
        let project = file::find_project_config();
        ctx.chat
            .push_system_message(render_config(ctx.info, user.as_deref(), project.as_deref()));
        Ok(())
    }
}

/// Render the snapshot + layered file paths as two key-value tables
/// separated by a blank line. The first table is the resolved values
/// (`/status` shows a subset of these too); the second is where they
/// came from on disk.
fn render_config(
    info: &SessionInfo,
    user_path: Option<&Path>,
    project_path: Option<&Path>,
) -> String {
    let cfg = &info.config;
    let effort = cfg
        .effort
        .map_or_else(|| "(default)".to_owned(), |e| e.to_string());
    let max_tokens = cfg.max_tokens.to_string();
    let cache_ttl = cfg.prompt_cache_ttl.to_string();
    let thinking = if cfg.show_thinking { "shown" } else { "hidden" };
    let resolved: [(&str, &str); 8] = [
        ("model", &info.model),
        ("model id", &cfg.model_id),
        ("base url", &cfg.base_url),
        ("auth", cfg.auth_label),
        ("effort", &effort),
        ("max tokens", &max_tokens),
        ("prompt cache ttl", &cache_ttl),
        ("thinking", thinking),
    ];
    let user = display_path(user_path);
    let project = display_path(project_path);
    let files: [(&str, &str); 2] = [("user", &user), ("project", &project)];

    let mut out = String::from("Config (resolved)\n\n");
    write_kv_table(&mut out, resolved);
    out.push_str("\nSources\n\n");
    write_kv_table(&mut out, files);
    out
}

/// `$HOME`-relative path string, or the explicit `(none)` /
/// `(absent)` markers so the user can tell missing-from-disk apart
/// from never-discovered.
fn display_path(path: Option<&Path>) -> String {
    let Some(path) = path else {
        return "(none)".to_owned();
    };
    let pretty = tildify(path);
    if path.exists() {
        pretty
    } else {
        format!("{pretty} (absent)")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::slash::test_session_info;

    // ── Config trait ──

    #[test]
    fn config_metadata_exposes_canonical_name_and_description() {
        assert_eq!(ConfigCmd.name(), "config");
        assert!(!ConfigCmd.description().is_empty());
    }

    #[test]
    fn config_execute_pushes_a_non_error_block() {
        // End-to-end through the trait method: success → one
        // non-error block in chat. Path discovery may or may not
        // resolve a real config in the test environment; either way
        // the renderer produces output, never an error.
        use crate::slash::SlashContext;
        use crate::tui::components::chat::ChatView;
        use crate::tui::theme::Theme;

        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let mut ctx = SlashContext::new(&mut chat, &info);
        ConfigCmd.execute("", &mut ctx).unwrap();
        assert_eq!(chat.entry_count(), 1);
        assert!(!chat.last_is_error());
    }

    // ── render_config ──

    #[test]
    fn render_config_starts_with_resolved_heading_then_sources_section() {
        let info = test_session_info();
        let body = render_config(&info, None, None);
        assert!(body.starts_with("Config (resolved)"), "{body}");
        assert!(body.contains("\nSources\n"), "{body}");
    }

    #[test]
    fn render_config_includes_every_resolved_field_value() {
        let info = test_session_info();
        let cfg = &info.config;
        let body = render_config(&info, None, None);
        for needle in [
            info.model.as_str(),
            cfg.model_id.as_str(),
            cfg.base_url.as_str(),
            cfg.auth_label,
        ] {
            assert!(body.contains(needle), "missing `{needle}`: {body}");
        }
        assert!(
            body.contains(&cfg.max_tokens.to_string()),
            "missing max_tokens: {body}",
        );
        assert!(
            body.contains(&cfg.prompt_cache_ttl.to_string()),
            "missing prompt_cache_ttl: {body}",
        );
    }

    #[test]
    fn render_config_renders_effort_or_fallback_marker_when_none() {
        // None should not display as "None" or empty — it should say
        // explicitly that the value defaults. Pin the marker text so
        // a refactor that loses the fallback fails here.
        let mut info = test_session_info();
        info.config.effort = None;
        let body = render_config(&info, None, None);
        assert!(body.contains("(default)"), "{body}");
    }

    #[test]
    fn render_config_thinking_renders_shown_or_hidden_per_flag() {
        // `show_thinking: false` ⇒ "hidden", `true` ⇒ "shown". Pin
        // both branches so a regression that prints `true` / `false`
        // fails here. Avoids "show thinking  true" reading as a
        // verb-phrase next to a bool.
        let mut info = test_session_info();
        info.config.show_thinking = false;
        assert!(
            render_config(&info, None, None).contains("hidden"),
            "false flag should render `hidden`",
        );
        info.config.show_thinking = true;
        assert!(
            render_config(&info, None, None).contains("shown"),
            "true flag should render `shown`",
        );
    }

    #[test]
    fn render_config_paths_present_renders_tildified_value() {
        let info = test_session_info();
        let path = PathBuf::from("/nonexistent/dir/config.toml");
        let body = render_config(&info, Some(&path), None);
        // Nonexistent path gets `(absent)` marker so the user sees the
        // file isn't actually there.
        assert!(body.contains("(absent)"), "{body}");
    }

    #[test]
    fn render_config_paths_none_marks_each_section_explicitly() {
        // Both file paths absent: `/config` distinguishes
        // never-discovered (`(none)`) from on-disk-but-missing
        // (`(absent)`). When both `user_path` and `project_path` are
        // `None` (no XDG home, no ox.toml in CWD ancestry) the output
        // should still render `(none)` placeholders, not blank rows.
        let info = test_session_info();
        let body = render_config(&info, None, None);
        // Two `(none)` rows expected — one per file.
        assert_eq!(body.matches("(none)").count(), 2, "{body}");
    }

    // ── display_path ──

    #[test]
    fn display_path_none_yields_explicit_marker() {
        assert_eq!(display_path(None), "(none)");
    }

    #[test]
    fn display_path_missing_file_marks_absent() {
        let p = PathBuf::from("/definitely/does/not/exist.toml");
        let got = display_path(Some(&p));
        assert!(got.ends_with("(absent)"), "{got}");
    }

    #[test]
    fn display_path_existing_file_returns_tildified_value() {
        // Use the workspace Cargo.toml — guaranteed to exist when the
        // test runs.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(here.exists(), "test fixture missing");
        let got = display_path(Some(&here));
        assert!(!got.contains("(absent)"), "{got}");
        // Tildify either rewrites the home prefix or leaves the path
        // verbatim; either way, it never inserts the absent marker.
    }
}
