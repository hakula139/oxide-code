//! `/config` — read-only view of the resolved effective config plus
//! the layered TOML paths it was assembled from
//! (`$XDG_CONFIG_HOME/ox/config.toml`, `./ox.toml`).
//!
//! Path discovery is per-invocation so the user sees fresh state if
//! they edited a file mid-session. `(not found)` flags a path that
//! resolved but isn't on disk; `(not configured)` flags an unresolved
//! path. The command never writes.

use std::path::Path;

use super::context::{SessionInfo, SlashContext};
use super::format::write_kv_section;
use super::registry::{SlashCommand, SlashOutcome};
use crate::config::file;
use crate::util::path::tildify;

pub(crate) struct ConfigCmd;

impl SlashCommand for ConfigCmd {
    fn name(&self) -> &'static str {
        "config"
    }

    fn description(&self) -> &'static str {
        "Show the resolved configuration and the layered files (~/.config/ox/config.toml, ./ox.toml) it was assembled from"
    }

    fn execute(&self, _args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let user = file::user_config_path();
        let project = file::find_project_config();
        ctx.chat
            .push_system_message(render_config(ctx.info, user.as_deref(), project.as_deref()));
        Ok(SlashOutcome::Local)
    }
}

/// Two key-value tables: resolved values (overlapping with `/status`)
/// and the on-disk paths they came from.
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
    let model = info.marketing_name();
    let resolved: [(&str, &str); 8] = [
        ("Model", &model),
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

/// `(not configured)` when the path didn't resolve, otherwise the
/// tildified path with `(not found)` appended if it doesn't exist on
/// disk. The path itself is always shown so the user sees *where* we
/// looked.
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
        // Trait-method end-to-end. Path discovery may or may not
        // resolve in the test env; either way `execute` is `Ok`.
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
        // Pin every `ConfigSnapshot` field reaches the user; this also
        // covers the happy-path `Some(effort)` + `show_thinking=false`
        // combination the dedicated branch tests below skip.
        let info = test_session_info();
        let cfg = &info.config;
        let model = info.marketing_name();
        let body = render_config(&info, None, None);
        let effort = cfg
            .effort
            .map(|e| e.to_string())
            .expect("fixture sets effort = Some");
        for needle in [
            model.as_ref(),
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
        // `None` must render explicit "(model default)", not "None"
        // or empty, so the user sees the fallback is intentional.
        let mut info = test_session_info();
        info.config.effort = None;
        let body = render_config(&info, None, None);
        assert!(body.contains("(model default)"), "{body}");
    }

    #[test]
    fn render_config_thinking_renders_yes_or_no_per_flag() {
        // Pin `yes`/`no` instead of `true`/`false`, both branches.
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
        // Missing files render the path with `(not found)` so the
        // user sees *where* we looked.
        assert!(body.contains("(not found)"), "{body}");
        assert!(
            body.contains("/nonexistent/dir/config.toml"),
            "missing path body: {body}",
        );
    }

    #[test]
    fn render_config_paths_none_marks_each_section_explicitly() {
        // Both rows render `(not configured)`, never blank.
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
        // Workspace `Cargo.toml` is guaranteed to exist at test time.
        let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(here.exists(), "test fixture missing");
        let got = display_path(Some(&here));
        assert!(!got.contains("(not found)"), "{got}");
        assert!(!got.contains("(not configured)"), "{got}");
    }
}
