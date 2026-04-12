use std::path::Path;

use tokio::process::Command;

/// Frontier model marketing name used for the "fast mode" bullet.
const FRONTIER_MODEL_NAME: &str = "Claude Opus 4.6";

/// Detected runtime environment for the system prompt.
///
/// Mirrors Claude Code's `computeSimpleEnvInfo()` — each field maps to one
/// or more bullets in the `# Environment` section.
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
    pub(super) async fn detect(model: &str, cwd: Option<&Path>, git_root: Option<&Path>) -> Self {
        let cwd_str = cwd.map_or_else(
            || "(unknown)".to_owned(),
            |p| p.to_string_lossy().into_owned(),
        );
        let is_git = git_root.is_some();
        let platform = detect_platform();
        let shell = detect_shell();
        let os_version = detect_os_version().await;
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
    /// Output format mirrors Claude Code's `computeSimpleEnvInfo()` with
    /// `prependBullets()` formatting:
    /// - Top-level items: ` - item` (1-space indent)
    /// - Sub-items: `  - item` (2-space indent)
    pub(super) fn render(&self) -> String {
        // Note: the trailing space after "environment:" matches Claude Code.
        let mut lines = vec![
            "# Environment".to_owned(),
            "You have been invoked in the following environment: ".to_owned(),
            format!(" - Primary working directory: {}", self.cwd),
            format!("  - Is a git repository: {}", self.is_git),
            format!(" - Platform: {}", self.platform),
            format!(" - Shell: {}", self.shell),
            format!(" - OS Version: {}", self.os_version),
        ];

        // Model description.
        if let Some(name) = marketing_name(&self.model) {
            lines.push(format!(
                " - You are powered by the model named {name}. \
                 The exact model ID is {}.",
                self.model
            ));
        } else {
            lines.push(format!(" - You are powered by the model {}.", self.model));
        }

        // Knowledge cutoff.
        if let Some(cutoff) = knowledge_cutoff(&self.model) {
            lines.push(format!(" - Assistant knowledge cutoff is {cutoff}."));
        }

        // Model family information.
        lines.push(
            " - The most recent Claude model family is Claude 4.5/4.6. \
             Model IDs \u{2014} Opus 4.6: 'claude-opus-4-6', \
             Sonnet 4.6: 'claude-sonnet-4-6', \
             Haiku 4.5: 'claude-haiku-4-5-20251001'. \
             When building AI applications, default to the latest and most \
             capable Claude models."
                .to_owned(),
        );

        // Availability.
        lines.push(
            " - Claude Code is available as a CLI in the terminal, \
             desktop app (Mac/Windows), web app (claude.ai/code), \
             and IDE extensions (VS Code, JetBrains)."
                .to_owned(),
        );

        // Fast mode.
        let frontier = marketing_name(&self.model).unwrap_or(FRONTIER_MODEL_NAME);
        lines.push(format!(
            " - Fast mode for Claude Code uses the same {frontier} model \
             with faster output. It does NOT switch to a different model. \
             It can be toggled with /fast."
        ));

        lines.join("\n")
    }
}

// ── Model Metadata ──

/// Map a model ID to its marketing name.
///
/// Arms are ordered most-specific-first because `contains()` would match
/// e.g. `"claude-opus-4"` against `"claude-opus-4-6"`. When adding new
/// models, keep more specific prefixes above less specific ones.
fn marketing_name(model: &str) -> Option<&'static str> {
    if model.contains("claude-opus-4-6") {
        Some("Claude Opus 4.6")
    } else if model.contains("claude-sonnet-4-6") {
        Some("Claude Sonnet 4.6")
    } else if model.contains("claude-opus-4-5") {
        Some("Claude Opus 4.5")
    } else if model.contains("claude-haiku-4") {
        Some("Claude Haiku 4.5")
    } else if model.contains("claude-opus-4") {
        Some("Claude Opus 4")
    } else if model.contains("claude-sonnet-4") {
        Some("Claude Sonnet 4")
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

/// Return the platform name matching Node's `process.platform` values.
///
/// Rust's `std::env::consts::OS` returns `"macos"` on macOS, but Claude
/// Code (via Node) uses `"darwin"`. This mapping ensures the environment
/// section matches the expected format.
fn detect_platform() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_owned(),
        "windows" => "win32".to_owned(),
        other => other.to_owned(),
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

/// Detect the OS version via `uname -sr`.
///
/// Returns a string like `"Darwin 25.3.0"` on macOS or `"Linux 6.1.0"` on
/// Linux. Falls back to the OS name when `uname` is unavailable.
async fn detect_os_version() -> String {
    let output = Command::new("uname")
        .args(["-sr"])
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if s.is_empty() {
                std::env::consts::OS.to_owned()
            } else {
                s
            }
        }
        _ => std::env::consts::OS.to_owned(),
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

    #[tokio::test]
    async fn detect_inside_repo_sets_is_git_true() {
        let cwd = std::env::current_dir().expect("cwd should be available");
        let env = Environment::detect("test-model", Some(&cwd), Some(&cwd)).await;
        assert!(env.is_git);
    }

    #[tokio::test]
    async fn detect_with_cwd_but_no_git_root_sets_is_git_false() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let env = Environment::detect("test-model", Some(tmp.path()), None).await;
        assert!(!env.is_git);
        assert!(
            env.cwd
                .ends_with(tmp.path().file_name().unwrap().to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn detect_without_cwd_uses_unknown_and_skips_git() {
        let env = Environment::detect("test-model", None, None).await;
        assert_eq!(env.cwd, "(unknown)");
        assert!(!env.is_git);
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

        assert!(rendered.starts_with("# Environment\n"));
        assert!(rendered.contains("You have been invoked in the following environment: \n"));
        assert!(rendered.contains(" - Primary working directory: /home/user/project\n"));
        assert!(rendered.contains("  - Is a git repository: true\n"));
        assert!(rendered.contains(" - Platform: linux\n"));
        assert!(rendered.contains(" - Shell: bash\n"));
        assert!(rendered.contains(" - OS Version: Linux 6.1.0\n"));
        assert!(rendered.contains(
            " - You are powered by the model named Claude Opus 4.6. \
             The exact model ID is claude-opus-4-6."
        ));
        assert!(rendered.contains(" - Assistant knowledge cutoff is May 2025."));
        assert!(rendered.contains(" - The most recent Claude model family"));
        assert!(rendered.contains(" - Claude Code is available"));
        assert!(
            rendered.contains(" - Fast mode for Claude Code uses the same Claude Opus 4.6 model")
        );
    }

    #[test]
    fn render_without_git_shows_false() {
        let env = Environment {
            cwd: "/tmp".to_owned(),
            is_git: false,
            platform: "darwin".to_owned(),
            shell: "zsh".to_owned(),
            os_version: "Darwin 25.3.0".to_owned(),
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
        assert!(rendered.contains(" - You are powered by the model custom-model-v1."));
        assert!(!rendered.contains("knowledge cutoff"));
        // Fast mode falls back to FRONTIER_MODEL_NAME.
        assert!(rendered.contains("same Claude Opus 4.6 model"));
    }

    #[test]
    fn render_git_sub_item_has_two_space_indent() {
        let env = Environment {
            cwd: "/home/user/project".to_owned(),
            is_git: true,
            platform: "linux".to_owned(),
            shell: "bash".to_owned(),
            os_version: "Linux 6.1.0".to_owned(),
            date: "2026-04-05".to_owned(),
            model: "test-model".to_owned(),
        };
        let rendered = env.render();

        // Git status is a sub-item: 2-space indent (prependBullets convention).
        let git_line = rendered
            .lines()
            .find(|l| l.contains("Is a git repository"))
            .expect("git line should exist");
        assert!(
            git_line.starts_with("  - "),
            "git status should be a sub-item with 2-space indent, got: {git_line:?}"
        );

        // Other items are top-level: 1-space indent.
        let platform_line = rendered
            .lines()
            .find(|l| l.contains("Platform:"))
            .expect("platform line should exist");
        assert!(
            platform_line.starts_with(" - "),
            "platform should be top-level with 1-space indent, got: {platform_line:?}"
        );
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
        assert_eq!(marketing_name("claude-haiku-4"), Some("Claude Haiku 4.5"));
        assert_eq!(marketing_name("claude-opus-4"), Some("Claude Opus 4"));
        assert_eq!(marketing_name("claude-sonnet-4"), Some("Claude Sonnet 4"));
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
        assert_eq!(knowledge_cutoff("claude-haiku-4"), Some("February 2025"));
        assert_eq!(knowledge_cutoff("claude-opus-4"), Some("January 2025"));
        assert_eq!(knowledge_cutoff("claude-sonnet-4"), Some("January 2025"));
    }

    #[test]
    fn knowledge_cutoff_unknown_model() {
        assert_eq!(knowledge_cutoff("custom-model"), None);
    }

    // ── detect_platform ──

    #[test]
    fn detect_platform_returns_node_style_names() {
        let platform = detect_platform();
        // On macOS this should be "darwin", not "macos".
        if cfg!(target_os = "macos") {
            assert_eq!(platform, "darwin");
        } else if cfg!(target_os = "linux") {
            assert_eq!(platform, "linux");
        }
    }

    // ── detect_shell ──

    #[test]
    fn detect_shell_simplifies_path() {
        // The function reads $SHELL; we can at least verify it returns
        // a non-empty string.
        let shell = detect_shell();
        assert!(!shell.is_empty());
        // On typical CI / dev machines, it should be "zsh" or "bash".
        assert!(
            shell == "zsh" || shell == "bash" || !shell.contains('/'),
            "shell should be simplified, got: {shell}"
        );
    }

    // ── detect_os_version ──

    #[tokio::test]
    async fn detect_os_version_returns_nonempty() {
        let version = detect_os_version().await;
        assert!(!version.is_empty());
    }

    // ── current_date ──

    #[test]
    fn current_date_matches_iso_format() {
        let date = current_date();
        assert_eq!(date.len(), 10, "expected YYYY-MM-DD, got: {date}");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
        // Verify it represents a plausible year.
        let year: u32 = date[..4].parse().expect("year should be numeric");
        assert!((2025..=2100).contains(&year), "unexpected year: {year}");
    }
}
