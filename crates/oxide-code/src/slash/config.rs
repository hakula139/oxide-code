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
use super::format::write_kv_section;
use super::registry::SlashCommand;
use crate::config::file;
use crate::util::path::tildify;

pub(crate) struct ConfigCmd;

impl SlashCommand for ConfigCmd {
    fn name(&self) -> &'static str {
        "config"
    }

    fn description(&self) -> &'static str {
        "show resolved config and source files"
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
        .map_or_else(|| "(model default)".to_owned(), |e| e.to_string());
    let max_tokens = cfg.max_tokens.to_string();
    let cache_ttl = cfg.prompt_cache_ttl.to_string();
    let thinking = if cfg.show_thinking { "yes" } else { "no" };
    let resolved: [(&str, &str); 8] = [
        ("Model", &info.model),
        ("Model ID", &cfg.model_id),
        ("Base URL", &cfg.base_url),
        ("Auth", cfg.auth_label),
        ("Effort", &effort),
        ("Max Tokens", &max_tokens),
        ("Prompt Cache TTL", &cache_ttl),
        ("Show Thinking", thinking),
    ];
    let user = display_path(user_path);
    let project = display_path(project_path);
    let files: [(&str, &str); 2] = [("User", &user), ("Project", &project)];

    let mut out = String::new();
    write_kv_section(&mut out, "Resolved Config", resolved);
    write_kv_section(&mut out, "Source Files", files);
    out
}

/// Render the searched path with `(not found)` when the file isn't on
/// disk, or `(not configured)` when the path itself was never resolved
/// (e.g., neither `$XDG_CONFIG_HOME` nor `$HOME` is set). The path is
/// always shown when known, so the user can see *where* we looked even
/// if nothing was there.
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
        assert!(body.starts_with("Resolved Config"), "{body}");
        assert!(body.contains("\nSource Files\n"), "{body}");
    }

    #[test]
    fn render_config_includes_every_resolved_field_value() {
        // Every `ConfigSnapshot` field reaches the user — a regression
        // that drops a row would fail here before it can ship. Includes
        // the `Some(effort)` rendering and `show_thinking` value, both
        // of which the dedicated tests below pin in their None / true
        // / false branches but neither covers the happy-path Some +
        // false combination the live fixture uses.
        let info = test_session_info();
        let cfg = &info.config;
        let body = render_config(&info, None, None);
        let effort = cfg
            .effort
            .map(|e| e.to_string())
            .expect("fixture sets effort = Some");
        for needle in [
            info.model.as_str(),
            cfg.model_id.as_str(),
            cfg.base_url.as_str(),
            cfg.auth_label,
            effort.as_str(),
            "no", // fixture: show_thinking = false
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
    fn render_config_renders_effort_fallback_marker_when_none() {
        // None should not display as "None" or empty — it should say
        // explicitly that the value defers to the model's own default.
        // Pin the marker text so a refactor that loses the fallback
        // fails here.
        let mut info = test_session_info();
        info.config.effort = None;
        let body = render_config(&info, None, None);
        assert!(body.contains("(model default)"), "{body}");
    }

    #[test]
    fn render_config_thinking_renders_yes_or_no_per_flag() {
        // The `show_thinking` row mirrors the toml key directly:
        // label `show thinking`, value `yes` / `no`. Pin both branches
        // so a regression that prints `true` / `false` (or drops the
        // row entirely) fails here.
        let mut info = test_session_info();
        info.config.show_thinking = false;
        let body = render_config(&info, None, None);
        assert!(body.contains("Show Thinking"), "label missing: {body}");
        assert!(body.contains("  no"), "false should render `no`: {body}");
        info.config.show_thinking = true;
        let body = render_config(&info, None, None);
        assert!(body.contains("  yes"), "true should render `yes`: {body}");
    }

    #[test]
    fn render_config_paths_present_renders_tildified_value() {
        let info = test_session_info();
        let path = PathBuf::from("/nonexistent/dir/config.toml");
        let body = render_config(&info, Some(&path), None);
        // Missing-on-disk paths render the path with `(not found)`,
        // not the bare path or a placeholder — the user can see
        // *where* /config looked even when the file is absent.
        assert!(body.contains("(not found)"), "{body}");
        assert!(
            body.contains("/nonexistent/dir/config.toml"),
            "missing path body: {body}",
        );
    }

    #[test]
    fn render_config_paths_none_marks_each_section_explicitly() {
        // When neither path resolved (e.g., no XDG home, no `ox.toml`
        // in CWD ancestry) both rows render `(not configured)`, not
        // blank values. Two rows ⇒ two markers.
        let info = test_session_info();
        let body = render_config(&info, None, None);
        assert_eq!(body.matches("(not configured)").count(), 2, "{body}");
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
    fn display_path_existing_file_returns_tildified_value_only() {
        // Use the workspace Cargo.toml — guaranteed to exist when the
        // test runs. Existing files render the bare tildified path
        // with no marker.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(here.exists(), "test fixture missing");
        let got = display_path(Some(&here));
        assert!(!got.contains("(not found)"), "{got}");
        assert!(!got.contains("(not configured)"), "{got}");
    }
}
