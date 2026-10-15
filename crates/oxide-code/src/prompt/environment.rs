//! Runtime environment detection for system prompt context.

use std::path::Path;

use indoc::formatdoc;
use platform_info::{PlatformInfo, PlatformInfoAPI, UNameAPI};
use serde::{Deserialize, Serialize};

use crate::model::lookup;
use crate::util::env;
use crate::util::time::local_offset;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Default))]
pub(super) struct Environment {
    cwd: String,
    is_git: bool,
    platform: String,
    shell: String,
    os_version: String,
    model: String,
}

impl Environment {
    /// Best-effort detection; failures produce fallback values.
    pub(super) fn detect(model: &str, cwd: &Path, git_root: Option<&Path>) -> Self {
        let is_git = git_root.is_some();
        let platform = normalize_node_platform(std::env::consts::OS).to_owned();
        let shell = detect_shell();
        let os_version = detect_os_version();

        Self {
            cwd: cwd.to_string_lossy().into_owned(),
            is_git,
            platform,
            shell,
            os_version,
            model: model.to_owned(),
        }
    }

    pub(super) fn set_model(&mut self, model: &str) {
        model.clone_into(&mut self.model);
    }

    pub(super) fn describes(&self, cwd: &Path) -> bool {
        self.cwd == cwd.to_string_lossy()
    }

    pub(super) fn render(&self) -> String {
        let info = lookup(&self.model);
        let model_bullet = match info {
            Some(info) => format!(
                "- You are powered by the model named {}. \
                The exact model ID is {}.",
                info.display_name, self.model,
            ),
            None => format!("- You are powered by the model {}.", self.model),
        };
        let cutoff_bullet = info
            .and_then(|info| info.cutoff)
            .map_or(String::new(), |c| {
                format!("- Assistant knowledge cutoff is {c}.")
            });

        formatdoc! {"
            # Environment

            You have been invoked in the following environment:

            - Primary working directory: {cwd}
              - Is a git repository: {is_git}
            - Platform: {platform}
            - Shell: {shell}
            - OS Version: {os_version}
            {model_bullet}
            {cutoff_bullet}",
            cwd = self.cwd,
            is_git = self.is_git,
            platform = self.platform,
            shell = self.shell,
            os_version = self.os_version,
        }
    }
}

// ── Platform Detection ──

/// Rust's `std::env::consts::OS` → Node's `process.platform` values.
fn normalize_node_platform(os: &str) -> &'static str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        _ => "unknown",
    }
}

// ── Shell Detection ──

fn detect_shell() -> String {
    let shell = env::string("SHELL").unwrap_or_else(|| "unknown".to_owned());
    std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&shell)
        .to_owned()
}

// ── OS Version Detection ──

fn detect_os_version() -> String {
    let Ok(info) = PlatformInfo::new() else {
        return std::env::consts::OS.to_owned();
    };
    let sysname = info.sysname().to_string_lossy();
    let release = info.release().to_string_lossy();
    if sysname.is_empty() {
        std::env::consts::OS.to_owned()
    } else {
        format!("{sysname} {release}")
    }
}

// ── Date Detection ──

pub(super) fn current_date() -> String {
    format_local_date(time::OffsetDateTime::now_utc(), local_offset())
}

fn format_local_date(now: time::OffsetDateTime, offset: time::UtcOffset) -> String {
    let date = now.to_offset(offset).date();
    format!(
        "{}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_model(model: &str) -> Environment {
        Environment {
            cwd: "/tmp".to_owned(),
            is_git: false,
            platform: "darwin".to_owned(),
            shell: "zsh".to_owned(),
            os_version: "Darwin 25.3.0".to_owned(),
            model: model.to_owned(),
        }
    }

    // ── Environment::detect ──

    #[test]
    fn detect_inside_repo_sets_is_git_true() {
        let cwd = std::env::current_dir().expect("cwd should be available");
        let env = Environment::detect("test-model", &cwd, Some(&cwd));
        assert!(env.is_git);
    }

    #[test]
    fn detect_with_cwd_but_no_git_root_sets_is_git_false() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let env = Environment::detect("test-model", tmp.path(), None);
        assert!(!env.is_git);
        assert!(
            env.cwd
                .ends_with(tmp.path().file_name().unwrap().to_str().unwrap())
        );
    }

    // ── Environment::render ──

    #[test]
    fn render_with_known_model_shows_all_fields() {
        let env = Environment {
            cwd: "/home/user/project".to_owned(),
            is_git: true,
            platform: "linux".to_owned(),
            shell: "bash".to_owned(),
            os_version: "Linux 6.1.0".to_owned(),
            model: "claude-opus-5-5".to_owned(),
        };
        let rendered = env.render();

        assert!(rendered.starts_with("# Environment"));
        assert!(rendered.contains("You have been invoked in the following environment:"));
        assert!(rendered.contains("- Primary working directory: /home/user/project"));
        assert!(rendered.contains("  - Is a git repository: true"));
        assert!(rendered.contains("- Platform: linux"));
        assert!(rendered.contains("- Shell: bash"));
        assert!(rendered.contains("- OS Version: Linux 6.1.0"));
        assert!(rendered.contains(
            "- You are powered by the model named Claude Opus 5.5. \
             The exact model ID is claude-opus-5-5."
        ));
        assert!(rendered.contains("- Assistant knowledge cutoff is June 2026."));
    }

    #[test]
    fn render_without_git_shows_false() {
        let rendered = env_with_model("test-model").render();
        assert!(rendered.contains("  - Is a git repository: false"));
    }

    #[test]
    fn render_known_model_without_cutoff_omits_cutoff_bullet() {
        let rendered = env_with_model("claude-sonnet-5-5").render();
        assert!(rendered.contains("- You are powered by the model named Claude Sonnet 5.5."));
        assert!(!rendered.contains("knowledge cutoff"));
    }

    #[test]
    fn render_unknown_model_uses_fallback_description() {
        let rendered = env_with_model("custom-model-v1").render();
        assert!(rendered.contains("- You are powered by the model custom-model-v1."));
        assert!(!rendered.contains("knowledge cutoff"));
    }

    // ── normalize_node_platform ──

    #[test]
    fn normalize_node_platform_known_values() {
        assert_eq!(normalize_node_platform("macos"), "darwin");
        assert_eq!(normalize_node_platform("linux"), "linux");
        assert_eq!(normalize_node_platform("windows"), "win32");
    }

    #[test]
    fn normalize_node_platform_unknown_value() {
        assert_eq!(normalize_node_platform("haiku"), "unknown");
    }

    // ── detect_shell ──

    #[test]
    fn detect_shell_produces_basename() {
        let shell = detect_shell();
        assert_ne!(shell, "");
        assert!(!shell.contains('/'), "should be a basename, got: {shell:?}");
    }

    // ── detect_os_version ──

    #[test]
    fn detect_os_version_contains_platform_and_version() {
        let version = detect_os_version();
        assert!(
            version.contains(' '),
            "expected 'Platform Version', got: {version:?}",
        );
    }

    // ── current_date ──

    #[test]
    fn current_date_matches_iso_format() {
        let date = current_date();
        assert_eq!(date.len(), 10, "expected YYYY-MM-DD, got: {date}");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");

        let year: u32 = date[..4].parse().expect("year should be numeric");
        let month: u32 = date[5..7].parse().expect("month should be numeric");
        let day: u32 = date[8..10].parse().expect("day should be numeric");
        assert!((2025..=2100).contains(&year), "unexpected year: {year}");
        assert!((1..=12).contains(&month), "unexpected month: {month}");
        assert!((1..=31).contains(&day), "unexpected day: {day}");
    }

    // ── format_local_date ──

    #[test]
    fn format_local_date_applies_offset_across_midnight() {
        let now = time::macros::datetime!(2026-04-12 20:30:00 UTC);
        assert_eq!(
            format_local_date(now, time::macros::offset!(+8)),
            "2026-04-13"
        );
        assert_eq!(
            format_local_date(now, time::macros::offset!(-5)),
            "2026-04-12"
        );
        assert_eq!(format_local_date(now, time::UtcOffset::UTC), "2026-04-12");
    }
}
