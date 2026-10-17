//! Runtime environment detection for system prompt context.

use std::fmt::Write as _;
use std::path::Path;

use indoc::formatdoc;
use platform_info::{PlatformInfo, PlatformInfoAPI, UNameAPI};
use serde::{Deserialize, Serialize};

use crate::model::ModelSpec;
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
}

impl Environment {
    /// Best-effort detection, with a fallback value for each probe that fails.
    pub(super) fn detect(cwd: &Path, git_root: Option<&Path>) -> Self {
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
        }
    }

    pub(super) fn describes(&self, cwd: &Path) -> bool {
        self.cwd == cwd.to_string_lossy()
    }

    pub(super) fn render(&self) -> String {
        formatdoc! {"
            # Environment

            You have been invoked in the following environment:

            - Primary working directory: {cwd}
              - Is a git repository: {is_git}
            - Platform: {platform}
            - Shell: {shell}
            - OS Version: {os_version}",
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
    Path::new(&shell)
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
    now.to_offset(offset)
        .date()
        .format(time::macros::format_description!("[year]-[month]-[day]"))
        .expect("static `[year]-[month]-[day]` description never fails on a valid date")
}

// ── Model Identity ──

/// Names `model`, its exact id, and its knowledge cutoff when the catalogue knows them.
pub(super) fn model_identity(model: &ModelSpec) -> String {
    let id = model.id();
    let Some(info) = model.info() else {
        return format!("You are powered by the model {id}.");
    };
    let mut identity = format!(
        "You are powered by the model named {}. The exact model ID is {id}.",
        info.display_name,
    );
    if let Some(cutoff) = info.cutoff {
        _ = write!(identity, " Assistant knowledge cutoff is {cutoff}.");
    }
    identity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::resolve(id.to_owned())
    }

    // ── Environment::detect ──

    #[test]
    fn detect_inside_repo_sets_is_git_true() {
        let cwd = std::env::current_dir().expect("cwd should be available");
        let env = Environment::detect(&cwd, Some(&cwd));
        assert!(env.is_git);
    }

    #[test]
    fn detect_with_cwd_but_no_git_root_sets_is_git_false() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let env = Environment::detect(tmp.path(), None);
        assert!(!env.is_git);
        assert!(
            env.cwd
                .ends_with(tmp.path().file_name().unwrap().to_str().unwrap())
        );
    }

    // ── Environment::render ──

    #[test]
    fn render_lists_every_field() {
        let env = Environment {
            cwd: "/home/user/project".to_owned(),
            is_git: true,
            platform: "linux".to_owned(),
            shell: "bash".to_owned(),
            os_version: "Linux 6.1.0".to_owned(),
        };

        assert_eq!(
            env.render(),
            indoc::indoc! {"
                # Environment

                You have been invoked in the following environment:

                - Primary working directory: /home/user/project
                  - Is a git repository: true
                - Platform: linux
                - Shell: bash
                - OS Version: Linux 6.1.0"
            },
        );
    }

    // ── normalize_node_platform ──

    #[test]
    fn normalize_node_platform_maps_rust_names_to_node_names() {
        assert_eq!(normalize_node_platform("macos"), "darwin");
        assert_eq!(normalize_node_platform("linux"), "linux");
        assert_eq!(normalize_node_platform("windows"), "win32");
    }

    #[test]
    fn normalize_node_platform_unrecognized_name_is_unknown() {
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
    fn detect_os_version_joins_sysname_and_release() {
        let version = detect_os_version();
        let info = PlatformInfo::new().unwrap();
        let sysname = info.sysname().to_string_lossy();
        assert_eq!(
            version,
            format!("{sysname} {}", info.release().to_string_lossy())
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

    // ── model_identity ──

    #[test]
    fn model_identity_names_a_catalogue_model_with_its_cutoff() {
        assert_eq!(
            model_identity(&spec("claude-opus-5-5")),
            "You are powered by the model named Claude Opus 5.5. The exact model ID is \
             claude-opus-5-5. Assistant knowledge cutoff is June 2026.",
        );
    }

    #[test]
    fn model_identity_omits_an_unknown_cutoff() {
        assert_eq!(
            model_identity(&spec("claude-sonnet-5-5")),
            "You are powered by the model named Claude Sonnet 5.5. The exact model ID is \
             claude-sonnet-5-5.",
        );
    }

    #[test]
    fn model_identity_names_an_unknown_model_by_its_id() {
        assert_eq!(
            model_identity(&spec("custom-model-v1")),
            "You are powered by the model custom-model-v1.",
        );
    }
}
