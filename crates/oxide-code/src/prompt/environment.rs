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
            Some(cwd) if git_root.is_some() => Some(detect_git_info(cwd).await),
            _ => None,
        };

        let platform = format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH);

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "(unknown)".to_owned());

        let date = current_date();

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

async fn detect_git_info(cwd: &Path) -> GitInfo {
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

    // Handle each result independently: default to empty branch and assume
    // dirty when a command fails, rather than discarding all git info.
    let branch = branch_result
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let is_clean = status_result
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim().is_empty());

    GitInfo { branch, is_clean }
}

// ── Date Detection ──

fn current_date() -> String {
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

    // ── Environment::render ──

    #[test]
    fn render_with_git_shows_all_fields_in_order() {
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
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[0], "# Environment");
        assert_eq!(lines[1], "- Working directory: /home/user/project");
        assert_eq!(lines[2], "  - Is a git repository: true");
        assert_eq!(lines[3], "  - Branch: main");
        assert_eq!(lines[4], "  - Status: clean");
        assert_eq!(lines[5], "- Platform: linux (x86_64)");
        assert_eq!(lines[6], "- Shell: /bin/bash");
        assert_eq!(lines[7], "- Date: 2026-04-05");
        assert_eq!(lines[8], "- Model: claude-opus-4-6");
        assert_eq!(lines.len(), 9);
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
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[2], "  - Is a git repository: false");
        // No branch or status lines — jump straight to platform.
        assert_eq!(lines[3], "- Platform: macos (aarch64)");
        assert_eq!(lines.len(), 7);
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

    // ── Environment::detect ──

    #[tokio::test]
    async fn detect_without_cwd_uses_unknown_and_skips_git() {
        let env = Environment::detect("test-model", None, None).await;
        assert_eq!(env.cwd, "(unknown)");
        assert!(env.git.is_none());
    }

    #[tokio::test]
    async fn detect_with_cwd_but_no_git_root_skips_git() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let env = Environment::detect("test-model", Some(tmp.path()), None).await;
        assert!(env.git.is_none());
        assert!(
            env.cwd
                .ends_with(tmp.path().file_name().unwrap().to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn detect_inside_repo_populates_git_info() {
        let cwd = std::env::current_dir().expect("cwd should be available");
        let env = Environment::detect("test-model", Some(&cwd), Some(&cwd)).await;
        assert!(env.git.is_some());
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
