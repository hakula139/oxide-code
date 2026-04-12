mod environment;
mod instructions;

use std::path::{Path, PathBuf};

use indoc::indoc;
use tokio::process::Command;

use environment::Environment;

const IDENTITY: &str = indoc! {"
    You are an interactive AI assistant that helps with software engineering tasks.
    Use the tools available to you to assist the user.

    Output text to communicate with the user. Use GitHub-flavored Markdown for
    formatting."
};

const TASK_GUIDANCE: &str = indoc! {"
    # Doing tasks

    - Do not propose changes to code you haven't read. If a user asks about or
      wants to modify a file, read it first.
    - Do not create files unless absolutely necessary. Prefer editing existing
      files over creating new ones.
    - Do not add features, refactor code, or make improvements beyond what was
      asked. Match the scope of changes to what was actually requested.
    - Be careful not to introduce security vulnerabilities such as command
      injection, path traversal, and other OWASP top 10 issues. If you notice
      insecure code you wrote, fix it immediately.
    - If a task is ambiguous, ask for clarification instead of guessing.
    - If an approach fails, diagnose why before retrying or switching tactics —
      read the error, check assumptions, try a focused fix. Do not retry the
      identical action blindly."
};

const CAUTION: &str = indoc! {"
    # Executing actions with care

    Consider the reversibility and blast radius of actions. Local, reversible
    actions like editing files or running tests can proceed freely. For actions
    that are hard to reverse, affect shared systems, or could be destructive,
    ask the user before proceeding.

    Examples of risky actions that warrant confirmation:

    - Destructive: deleting files or branches, `rm -rf`, overwriting uncommitted
      changes.
    - Hard to reverse: force-pushing, `git reset --hard`, amending published
      commits.
    - Visible to others: pushing code, creating or commenting on PRs / issues.

    When encountering unexpected state (unfamiliar files, branches, lock files),
    investigate before deleting or overwriting — it may be the user's in-progress
    work. Prefer fixing root causes over bypassing safety checks (e.g., do not
    use `--no-verify`)."
};

const TOOL_GUIDANCE: &str = indoc! {"
    # Using your tools

    Use dedicated tools instead of running equivalent shell commands:

    - Read files: use `read`, not `cat` / `head` / `tail`
    - Edit files: use `edit`, not `sed` / `awk`
    - Write files: use `write`, not `echo` / `cat` with redirection
    - Search files: use `glob`, not `find` / `ls`
    - Search content: use `grep`, not shell `grep` / `rg`
    - Reserve `bash` for commands that genuinely require shell execution.

    When multiple tool calls are independent of each other, make them in
    parallel."
};

const STYLE: &str = indoc! {"
    # Tone and style

    - Be concise. Lead with the answer or action, not the reasoning.
    - When referencing code, include `file_path:line_number` for easy navigation.
    - Skip filler words and preamble. Go straight to the point.
    - Focus text output on decisions that need user input, progress at milestones,
      and errors.
    - Do not use emojis unless the user requests it."
};

/// Assembled prompt split into two API surfaces.
///
/// `system` contains static guidance for the `system` API parameter.
/// `user_context` contains dynamic content (CLAUDE.md, date) that is
/// prepended to the `messages` array as a `<system-reminder>`-wrapped
/// user message — matching Claude Code's context injection pattern.
pub(crate) struct PromptParts {
    pub system: String,
    pub user_context: Option<String>,
}

/// Build the prompt parts for the agent.
///
/// Resolves the working directory and git root automatically, then delegates
/// to [`assemble`].
pub(crate) async fn build_prompt(model: &str) -> PromptParts {
    let cwd = std::env::current_dir().ok();
    let git_root = match &cwd {
        Some(cwd) => find_git_root(cwd).await,
        None => None,
    };

    assemble(model, cwd.as_deref(), git_root.as_deref()).await
}

/// Assemble the prompt from explicit path parameters.
///
/// The identity prefix required for OAuth is handled by the API client as a
/// separate system block. This function builds the remaining prompt content:
/// identity body and static guidance sections go into `system`; CLAUDE.md
/// and date go into `user_context` as a `<system-reminder>` block.
async fn assemble(model: &str, cwd: Option<&Path>, git_root: Option<&Path>) -> PromptParts {
    let (env, claude_md) = tokio::join!(
        Environment::detect(model, cwd, git_root),
        instructions::load(cwd, git_root),
    );

    let system = [
        IDENTITY,
        TASK_GUIDANCE,
        CAUTION,
        TOOL_GUIDANCE,
        STYLE,
        &env.render(),
    ]
    .join("\n\n");

    let user_context = build_user_context(&claude_md, &env.date());

    PromptParts {
        system,
        user_context,
    }
}

/// Build the `<system-reminder>` user message content from dynamic context.
///
/// Mirrors Claude Code's `prependUserContext()` — CLAUDE.md and date are
/// injected as a synthetic user message, not in the `system` parameter.
fn build_user_context(claude_md: &str, date: &str) -> Option<String> {
    use std::fmt::Write;

    if claude_md.is_empty() {
        return None;
    }

    let mut out = String::from(indoc! {"
        <system-reminder>
        As you answer the user's questions, you can use the following context:"
    });
    _ = write!(out, "\n# claudeMd\n{claude_md}");
    _ = write!(out, "\n# currentDate\n{date}");
    out.push_str(indoc! {"

        IMPORTANT: this context may or may not be relevant to your tasks. You should \
        not respond to this context unless it is highly relevant to your task.
        </system-reminder>"
    });

    Some(out)
}

/// Find the git repository root from a working directory.
///
/// Returns `None` when not inside a git repository or when `git` is not
/// available.
async fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return None;
    }

    Some(PathBuf::from(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_prompt ──

    #[tokio::test]
    async fn build_prompt_system_starts_with_identity() {
        let parts = build_prompt("test-model").await;
        assert!(
            parts
                .system
                .starts_with("You are an interactive AI assistant"),
            "system should start with identity body (prefix is in the client)"
        );
    }

    #[tokio::test]
    async fn build_prompt_system_contains_all_static_sections() {
        let parts = build_prompt("test-model").await;
        assert!(parts.system.contains("# Doing tasks"));
        assert!(parts.system.contains("# Executing actions with care"));
        assert!(parts.system.contains("# Using your tools"));
        assert!(parts.system.contains("# Tone and style"));
        assert!(parts.system.contains("# Environment"));
    }

    #[tokio::test]
    async fn build_prompt_system_includes_model_name() {
        let parts = build_prompt("claude-opus-4-6").await;
        assert!(parts.system.contains("Model: claude-opus-4-6"));
    }

    /// This test runs inside the oxide-code repo which has CLAUDE.md, so the
    /// `user_context` branch should be exercised.
    #[tokio::test]
    async fn build_prompt_returns_user_context_with_claude_md() {
        let parts = build_prompt("test-model").await;
        let ctx = parts
            .user_context
            .as_deref()
            .expect("expected user context from project CLAUDE.md");
        assert!(ctx.contains("<system-reminder>"));
        assert!(ctx.contains("# claudeMd"));
        assert!(ctx.contains("# currentDate"));
        assert!(ctx.contains("</system-reminder>"));
    }

    #[tokio::test]
    async fn build_prompt_system_does_not_contain_user_instructions() {
        let parts = build_prompt("test-model").await;
        assert!(
            !parts.system.contains("# User instructions"),
            "CLAUDE.md should be in user_context, not system"
        );
    }

    #[tokio::test]
    async fn build_prompt_sections_joined_with_double_newline() {
        let parts = build_prompt("test-model").await;
        // Each section boundary is a double newline. Verify the identity
        // section is separated from the next by exactly "\n\n".
        let identity_end = parts
            .system
            .find("# Doing tasks")
            .expect("task guidance missing");
        let before = &parts.system[..identity_end];
        assert!(
            before.ends_with("\n\n"),
            "sections should be joined with double newline"
        );
    }

    // ── assemble ──

    #[tokio::test]
    async fn assemble_in_git_repo_has_system_and_user_context() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("CLAUDE.md"), "Test project rules.").unwrap();

        let parts = assemble("test-model", Some(tmp.path()), Some(tmp.path())).await;

        let expected_system_sections = [
            "You are an interactive AI assistant",
            "# Doing tasks",
            "# Executing actions with care",
            "# Using your tools",
            "# Tone and style",
            "# Environment",
        ];
        let mut prev_pos = 0;
        for header in &expected_system_sections {
            let pos = parts
                .system
                .find(header)
                .unwrap_or_else(|| panic!("missing section: {header}"));
            assert!(
                pos >= prev_pos,
                "{header} should come after previous section"
            );
            prev_pos = pos;
        }

        assert!(
            parts
                .system
                .contains(&format!("Working directory: {}", tmp.path().display()))
        );
        assert!(parts.system.contains("Is a git repository: true"));
        assert!(parts.system.contains("Model: test-model"));

        // CLAUDE.md goes into user_context as <system-reminder>.
        let ctx = parts
            .user_context
            .as_deref()
            .expect("expected user context");
        assert!(ctx.contains("Test project rules."));
        assert!(ctx.contains("<system-reminder>"));
    }

    /// Without project-level CLAUDE.md, `user_context` depends only on the
    /// global `~/.claude/CLAUDE.md`. The system prompt must never contain
    /// user instructions regardless.
    #[tokio::test]
    async fn assemble_without_project_claude_md_keeps_system_clean() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        init_git_repo(tmp.path());

        let parts = assemble("test-model", Some(tmp.path()), Some(tmp.path())).await;
        assert!(
            !parts.system.contains("# User instructions"),
            "system should never contain user instructions"
        );
    }

    #[tokio::test]
    async fn assemble_walks_root_to_cwd_for_instruction_discovery() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let root = tmp.path();
        init_git_repo(root);

        std::fs::write(root.join("CLAUDE.md"), "Root rules.").unwrap();
        let sub = root.join("crates").join("core");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "Subdir rules.").unwrap();

        let parts = assemble("test-model", Some(&sub), Some(root)).await;
        let ctx = parts
            .user_context
            .as_deref()
            .expect("expected user context");

        assert!(ctx.contains("Root rules."));
        assert!(ctx.contains("Subdir rules."));

        let root_pos = ctx.find("Root rules.").unwrap();
        let sub_pos = ctx.find("Subdir rules.").unwrap();
        assert!(
            root_pos < sub_pos,
            "root instructions should appear before subdirectory"
        );
    }

    // ── build_user_context ──

    #[test]
    fn build_user_context_with_claude_md() {
        let ctx = build_user_context("Project rules.", "Today's date is 2026-04-12.")
            .expect("expected Some");
        assert!(ctx.starts_with("<system-reminder>"));
        assert!(ctx.ends_with("</system-reminder>"));
        assert!(ctx.contains("# claudeMd\nProject rules."));
        assert!(ctx.contains("# currentDate\nToday's date is 2026-04-12."));
        assert!(ctx.contains("IMPORTANT: this context may or may not be relevant"));
    }

    #[test]
    fn build_user_context_empty_claude_md_returns_none() {
        assert!(build_user_context("", "Today's date is 2026-04-12.").is_none());
    }

    // ── find_git_root ──

    #[tokio::test]
    async fn find_git_root_inside_repo() {
        let cwd = std::env::current_dir().expect("cwd should be available");
        let root = find_git_root(&cwd).await;
        assert!(root.is_some(), "test must run inside a git repo");
        assert!(
            root.as_ref().unwrap().join(".git").exists(),
            "root should contain .git"
        );
    }

    #[tokio::test]
    async fn find_git_root_outside_repo() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let root = find_git_root(tmp.path()).await;
        assert!(root.is_none());
    }

    // ── constant content ──

    /// Verify the prompt constants use `indoc!` correctly — no leading
    /// whitespace from source indentation should appear in the output.
    #[test]
    fn prompt_constants_have_no_leading_whitespace() {
        for (name, content) in [
            ("IDENTITY", IDENTITY),
            ("TASK_GUIDANCE", TASK_GUIDANCE),
            ("CAUTION", CAUTION),
            ("TOOL_GUIDANCE", TOOL_GUIDANCE),
            ("STYLE", STYLE),
        ] {
            assert!(
                !content.starts_with(' ') && !content.starts_with('\t'),
                "{name} should not start with whitespace"
            );
        }
    }

    // ── helpers ──

    fn init_git_repo(path: &Path) {
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to spawn git");
        assert!(status.success(), "git init exited with non-zero status");
    }
}
