mod environment;
mod instructions;

use std::path::{Path, PathBuf};

use tokio::process::Command;

use environment::Environment;

/// OAuth-required identity prefix. The Anthropic API returns 429 for non-Haiku
/// models with OAuth tokens unless the system prompt starts with this string.
const IDENTITY_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

const IDENTITY: &str = "\
You are an interactive AI assistant that helps with software engineering tasks. \
Use the tools available to you to assist the user.

Output text to communicate with the user. Use GitHub-flavored Markdown for formatting.";

const TASK_GUIDANCE: &str = "\
# Doing tasks

- Read and understand existing code before suggesting modifications.
- Prefer editing existing files over creating new ones.
- Do not add features, refactor code, or make improvements beyond what was asked.
- Be careful not to introduce security vulnerabilities.
- If a task is ambiguous, ask for clarification instead of guessing.
- If an approach fails, diagnose why before retrying or switching tactics.";

const TOOL_GUIDANCE: &str = "\
# Using your tools

Use dedicated tools instead of running equivalent shell commands:
- Read files: use `read`, not `cat` / `head` / `tail`
- Edit files: use `edit`, not `sed` / `awk`
- Write files: use `write`, not `echo` / `cat` with redirection
- Search files: use `glob`, not `find` / `ls`
- Search content: use `grep`, not shell `grep` / `rg`
- Reserve `bash` for commands that genuinely require shell execution.

When multiple tool calls are independent of each other, make them in parallel.";

const STYLE: &str = "\
# Tone and style

- Be concise. Lead with the answer or action, not the reasoning.
- When referencing code, include `file_path:line_number` for easy navigation.
- Skip filler words and preamble. Go straight to the point.";

/// Build the complete system prompt for the agent.
///
/// The prompt always begins with [`IDENTITY_PREFIX`] (required for OAuth)
/// followed by static guidance sections, a detected environment section, and
/// any discovered CLAUDE.md user instructions.
pub(crate) async fn build_system_prompt(model: &str) -> String {
    let cwd = std::env::current_dir().ok();
    let git_root = match &cwd {
        Some(cwd) => find_git_root(cwd).await,
        None => None,
    };

    let (env, claude_md) = tokio::join!(
        Environment::detect(model, cwd.as_deref(), git_root.as_deref()),
        instructions::load(cwd.as_deref(), git_root.as_deref()),
    );

    let mut sections = vec![
        format!("{IDENTITY_PREFIX}\n{IDENTITY}"),
        TASK_GUIDANCE.to_owned(),
        TOOL_GUIDANCE.to_owned(),
        STYLE.to_owned(),
        env.render(),
    ];

    if !claude_md.is_empty() {
        sections.push(claude_md);
    }

    sections.join("\n\n")
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

    // ── build_system_prompt ──

    #[tokio::test]
    async fn build_system_prompt_starts_with_identity_prefix() {
        let prompt = build_system_prompt("test-model").await;
        assert!(prompt.starts_with(&format!("{IDENTITY_PREFIX}\n")));
    }

    #[tokio::test]
    async fn build_system_prompt_contains_all_static_sections() {
        let prompt = build_system_prompt("test-model").await;
        assert!(prompt.contains("# Doing tasks"));
        assert!(prompt.contains("# Using your tools"));
        assert!(prompt.contains("# Tone and style"));
        assert!(prompt.contains("# Environment"));
    }

    #[tokio::test]
    async fn build_system_prompt_includes_model_name() {
        let prompt = build_system_prompt("claude-opus-4-6").await;
        assert!(prompt.contains("Model: claude-opus-4-6"));
    }
}
