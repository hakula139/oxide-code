use std::path::Path;

use tokio::process::Command;

/// Detected runtime environment for the system prompt.
pub(super) struct Environment {
    cwd: String,
    platform: String,
    shell: String,
    git: Option<GitInfo>,
    date: String,
    model: String,
}

struct GitInfo {
    branch: String,
    is_clean: bool,
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

        let git = match cwd {
            Some(cwd) if git_root.is_some() => detect_git_info(cwd).await,
            _ => None,
        };

        let platform = format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH);

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "(unknown)".to_owned());

        let date = current_date().await;

        Self {
            cwd: cwd_str,
            platform,
            shell,
            git,
            date,
            model: model.to_owned(),
        }
    }

    /// Render the environment section for the system prompt.
    pub(super) fn render(&self) -> String {
        let mut lines = vec![
            "# Environment".to_owned(),
            format!("- Working directory: {}", self.cwd),
        ];

        match &self.git {
            Some(git) => {
                lines.push("  - Is a git repository: true".to_owned());
                if !git.branch.is_empty() {
                    lines.push(format!("  - Branch: {}", git.branch));
                }
                let status = if git.is_clean { "clean" } else { "dirty" };
                lines.push(format!("  - Status: {status}"));
            }
            None => {
                lines.push("  - Is a git repository: false".to_owned());
            }
        }

        lines.push(format!("- Platform: {}", self.platform));
        lines.push(format!("- Shell: {}", self.shell));
        lines.push(format!("- Date: {}", self.date));
        lines.push(format!("- Model: {}", self.model));

        lines.join("\n")
    }
}

// ── Git Detection ──

async fn detect_git_info(cwd: &Path) -> Option<GitInfo> {
    let (branch_result, status_result) = tokio::join!(
        Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(cwd)
            .stderr(std::process::Stdio::null())
            .output(),
        Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .stderr(std::process::Stdio::null())
            .output(),
    );

    let branch = String::from_utf8_lossy(&branch_result.ok()?.stdout)
        .trim()
        .to_owned();
    let is_clean = String::from_utf8_lossy(&status_result.ok()?.stdout)
        .trim()
        .is_empty();

    Some(GitInfo { branch, is_clean })
}

// ── Date Detection ──

async fn current_date() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown)".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Environment::render ──

    #[test]
    fn render_with_git_shows_branch_and_status() {
        let env = Environment {
            cwd: "/home/user/project".to_owned(),
            platform: "linux (x86_64)".to_owned(),
            shell: "/bin/bash".to_owned(),
            git: Some(GitInfo {
                branch: "main".to_owned(),
                is_clean: true,
            }),
            date: "2026-04-05".to_owned(),
            model: "claude-opus-4-6".to_owned(),
        };
        let rendered = env.render();
        assert!(rendered.contains("Working directory: /home/user/project"));
        assert!(rendered.contains("Is a git repository: true"));
        assert!(rendered.contains("Branch: main"));
        assert!(rendered.contains("Status: clean"));
        assert!(rendered.contains("Platform: linux (x86_64)"));
        assert!(rendered.contains("Shell: /bin/bash"));
        assert!(rendered.contains("Date: 2026-04-05"));
        assert!(rendered.contains("Model: claude-opus-4-6"));
    }

    #[test]
    fn render_without_git_shows_not_a_repo() {
        let env = Environment {
            cwd: "/tmp".to_owned(),
            platform: "macos (aarch64)".to_owned(),
            shell: "/bin/zsh".to_owned(),
            git: None,
            date: "2026-04-05".to_owned(),
            model: "test-model".to_owned(),
        };
        let rendered = env.render();
        assert!(rendered.contains("Is a git repository: false"));
        assert!(!rendered.contains("Branch:"));
        assert!(!rendered.contains("Status:"));
    }

    #[test]
    fn render_dirty_repo_shows_dirty() {
        let env = Environment {
            cwd: "/home/user/project".to_owned(),
            platform: "linux (x86_64)".to_owned(),
            shell: "/bin/bash".to_owned(),
            git: Some(GitInfo {
                branch: "feat/wip".to_owned(),
                is_clean: false,
            }),
            date: "2026-04-05".to_owned(),
            model: "test-model".to_owned(),
        };
        let rendered = env.render();
        assert!(rendered.contains("Status: dirty"));
    }

    #[test]
    fn render_detached_head_omits_branch() {
        let env = Environment {
            cwd: "/home/user/project".to_owned(),
            platform: "linux (x86_64)".to_owned(),
            shell: "/bin/bash".to_owned(),
            git: Some(GitInfo {
                branch: String::new(),
                is_clean: true,
            }),
            date: "2026-04-05".to_owned(),
            model: "test-model".to_owned(),
        };
        let rendered = env.render();
        assert!(rendered.contains("Is a git repository: true"));
        assert!(!rendered.contains("Branch:"));
    }

    // ── current_date ──

    #[tokio::test]
    async fn current_date_matches_iso_format() {
        let date = current_date().await;
        assert_eq!(date.len(), 10, "expected YYYY-MM-DD, got: {date}");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
    }
}
