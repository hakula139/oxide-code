use std::path::Path;

use indoc::formatdoc;
use platform_info::{PlatformInfo, PlatformInfoAPI, UNameAPI};

/// Detected runtime environment for the system prompt.
///
/// Each field maps to one or more bullets in the `# Environment` section.
pub(super) struct Environment {
    cwd: String,
    is_git: bool,
    platform: String,
    shell: String,
    os_version: String,
    date: String,
    model: String,
}

impl Environment {
    /// Detect the current runtime environment.
    ///
    /// All detection is best-effort: failures produce fallback values rather
    /// than errors, so the system prompt is always constructible.
    pub(super) fn detect(model: &str, cwd: Option<&Path>, git_root: Option<&Path>) -> Self {
        let cwd_str = cwd.map_or_else(
            || "(unknown)".to_owned(),
            |p| p.to_string_lossy().into_owned(),
        );
        let is_git = git_root.is_some();
        let platform = normalize_node_platform(std::env::consts::OS).to_owned();
        let shell = detect_shell();
        let os_version = detect_os_version();
        let date = current_date();

        Self {
            cwd: cwd_str,
            is_git,
            platform,
            shell,
            os_version,
            date,
            model: model.to_owned(),
        }
    }

    /// The formatted date string (e.g., `"Today's date is 2026-04-12."`).
    pub(super) fn date(&self) -> String {
        format!("Today's date is {}.", self.date)
    }

    /// Render the environment section for the system prompt.
    ///
    /// Standard markdown formatting:
    /// - Top-level items: `- item`
    /// - Sub-items: `  - item` (2-space indent)
    pub(super) fn render(&self) -> String {
        let model_bullet = match marketing_name(&self.model) {
            Some(name) => format!(
                "- You are powered by the model named {name}. \
                The exact model ID is {}.",
                self.model
            ),
            None => format!("- You are powered by the model {}.", self.model),
        };
        let cutoff_bullet = knowledge_cutoff(&self.model).map_or(String::new(), |c| {
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

// ── Model Metadata ──

/// Map a model ID to its marketing name.
///
/// Arms are ordered most-specific-first because `contains()` would match
/// e.g. `"claude-opus-4"` against `"claude-opus-4-6"`. When adding new
/// models, keep more specific prefixes above less specific ones.
pub(crate) fn marketing_name(model: &str) -> Option<&'static str> {
    if model.contains("claude-opus-4-6") {
        Some("Claude Opus 4.6")
    } else if model.contains("claude-sonnet-4-6") {
        Some("Claude Sonnet 4.6")
    } else if model.contains("claude-opus-4-5") {
        Some("Claude Opus 4.5")
    } else if model.contains("claude-sonnet-4-5") {
        Some("Claude Sonnet 4.5")
    } else if model.contains("claude-haiku-4-5") {
        Some("Claude Haiku 4.5")
    } else if model.contains("claude-opus-4-1") {
        Some("Claude Opus 4.1")
    } else if model.contains("claude-opus-4") {
        Some("Claude Opus 4")
    } else if model.contains("claude-sonnet-4") {
        Some("Claude Sonnet 4")
    } else if model.contains("claude-haiku-4") {
        Some("Claude Haiku 4")
    } else {
        None
    }
}

/// Map a model ID to its knowledge cutoff date.
fn knowledge_cutoff(model: &str) -> Option<&'static str> {
    if model.contains("claude-sonnet-4-6") {
        Some("August 2025")
    } else if model.contains("claude-opus-4-6") || model.contains("claude-opus-4-5") {
        Some("May 2025")
    } else if model.contains("claude-haiku-4") {
        Some("February 2025")
    } else if model.contains("claude-opus-4") || model.contains("claude-sonnet-4") {
        Some("January 2025")
    } else {
        None
    }
}

// ── Platform Detection ──

/// Map Rust's OS name to Node's `process.platform` values.
///
/// Rust's `std::env::consts::OS` returns `"macos"` on macOS, but Claude
/// Code (via Node) uses `"darwin"`. This mapping ensures the environment
/// section matches the expected format.
fn normalize_node_platform(os: &str) -> &'static str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        _ => "unknown",
    }
}

// ── Shell Detection ──

/// Extract the shell name from `$SHELL`, matching Claude Code's format.
///
/// Returns just the basename (`"zsh"`, `"bash"`) rather than the full path.
fn detect_shell() -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_owned());
    std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&shell)
        .to_owned()
}

// ── OS Version Detection ──

/// Detect the OS version via `platform_info::PlatformInfo`.
///
/// Returns a string like `"Darwin 25.3.0"` on macOS or `"Linux 6.1.0"` on
/// Linux. Falls back to the OS name when detection fails.
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

fn current_date() -> String {
    // now_local() fails on multi-threaded Linux (time crate safety constraint),
    // so this effectively falls back to UTC there.
    let date = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .date();
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

    // ── Environment::detect ──

    #[test]
    fn detect_inside_repo_sets_is_git_true() {
        let cwd = std::env::current_dir().expect("cwd should be available");
        let env = Environment::detect("test-model", Some(&cwd), Some(&cwd));
        assert!(env.is_git);
    }

    #[test]
    fn detect_with_cwd_but_no_git_root_sets_is_git_false() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let env = Environment::detect("test-model", Some(tmp.path()), None);
        assert!(!env.is_git);
        assert!(
            env.cwd
                .ends_with(tmp.path().file_name().unwrap().to_str().unwrap())
        );
    }

    #[test]
    fn detect_without_cwd_uses_unknown_and_skips_git() {
        let env = Environment::detect("test-model", None, None);
        assert_eq!(env.cwd, "(unknown)");
        assert!(!env.is_git);
    }

    // ── Environment::date ──

    #[test]
    fn date_returns_formatted_string() {
        let env = Environment {
            cwd: "/tmp".to_owned(),
            is_git: false,
            platform: "darwin".to_owned(),
            shell: "zsh".to_owned(),
            os_version: "Darwin 25.3.0".to_owned(),
            date: "2026-04-12".to_owned(),
            model: "test-model".to_owned(),
        };
        assert_eq!(env.date(), "Today's date is 2026-04-12.");
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
            date: "2026-04-05".to_owned(),
            model: "claude-opus-4-6".to_owned(),
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
            "- You are powered by the model named Claude Opus 4.6. \
             The exact model ID is claude-opus-4-6."
        ));
        assert!(rendered.contains("- Assistant knowledge cutoff is May 2025."));
    }

    #[test]
    fn render_without_git_shows_false() {
        let env = Environment {
            cwd: "/tmp".to_owned(),
            is_git: false,
            platform: "darwin".to_owned(),
            shell: "zsh".to_owned(),
            os_version: "Darwin 24.6.0".to_owned(),
            date: "2026-04-05".to_owned(),
            model: "test-model".to_owned(),
        };
        let rendered = env.render();
        assert!(rendered.contains("  - Is a git repository: false"));
    }

    #[test]
    fn render_unknown_model_uses_fallback_description() {
        let env = Environment {
            cwd: "/tmp".to_owned(),
            is_git: false,
            platform: "darwin".to_owned(),
            shell: "zsh".to_owned(),
            os_version: "Darwin 25.3.0".to_owned(),
            date: "2026-04-05".to_owned(),
            model: "custom-model-v1".to_owned(),
        };
        let rendered = env.render();
        assert!(rendered.contains("- You are powered by the model custom-model-v1."));
        assert!(!rendered.contains("knowledge cutoff"));
    }

    // ── marketing_name ──

    #[test]
    fn marketing_name_known_models() {
        assert_eq!(marketing_name("claude-opus-4-6"), Some("Claude Opus 4.6"));
        assert_eq!(
            marketing_name("claude-sonnet-4-6"),
            Some("Claude Sonnet 4.6")
        );
        assert_eq!(marketing_name("claude-opus-4-5"), Some("Claude Opus 4.5"));
        assert_eq!(
            marketing_name("claude-sonnet-4-5"),
            Some("Claude Sonnet 4.5")
        );
        assert_eq!(marketing_name("claude-haiku-4-5"), Some("Claude Haiku 4.5"));
        assert_eq!(marketing_name("claude-opus-4-1"), Some("Claude Opus 4.1"));
        assert_eq!(marketing_name("claude-opus-4"), Some("Claude Opus 4"));
        assert_eq!(marketing_name("claude-sonnet-4"), Some("Claude Sonnet 4"));
        assert_eq!(marketing_name("claude-haiku-4"), Some("Claude Haiku 4"));
    }

    #[test]
    fn marketing_name_unknown_model() {
        assert_eq!(marketing_name("gpt-4o"), None);
        assert_eq!(marketing_name("custom-model"), None);
    }

    #[test]
    fn marketing_name_with_suffix() {
        // Model IDs can include suffixes (e.g., date tags).
        assert_eq!(
            marketing_name("claude-opus-4-6-20260401"),
            Some("Claude Opus 4.6")
        );
    }

    // ── knowledge_cutoff ──

    #[test]
    fn knowledge_cutoff_known_models() {
        assert_eq!(knowledge_cutoff("claude-sonnet-4-6"), Some("August 2025"));
        assert_eq!(knowledge_cutoff("claude-opus-4-6"), Some("May 2025"));
        assert_eq!(knowledge_cutoff("claude-opus-4-5"), Some("May 2025"));
        assert_eq!(knowledge_cutoff("claude-haiku-4-5"), Some("February 2025"));
        assert_eq!(knowledge_cutoff("claude-haiku-4"), Some("February 2025"));
        assert_eq!(knowledge_cutoff("claude-opus-4-1"), Some("January 2025"));
        assert_eq!(knowledge_cutoff("claude-opus-4"), Some("January 2025"));
        assert_eq!(knowledge_cutoff("claude-sonnet-4-5"), Some("January 2025"));
        assert_eq!(knowledge_cutoff("claude-sonnet-4"), Some("January 2025"));
    }

    #[test]
    fn knowledge_cutoff_unknown_model() {
        assert_eq!(knowledge_cutoff("custom-model"), None);
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
    fn detect_shell_returns_basename() {
        let shell = detect_shell();
        assert!(!shell.is_empty());
        assert!(!shell.contains('/'), "should be a basename, got: {shell:?}");
    }

    // ── detect_os_version ──

    #[test]
    fn detect_os_version_returns_nonempty() {
        let version = detect_os_version();
        assert!(!version.is_empty());
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
}
