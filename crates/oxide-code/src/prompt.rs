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

- Do not propose changes to code you haven't read. If a user asks about or wants to \
modify a file, read it first.
- Do not create files unless absolutely necessary. Prefer editing existing files over \
creating new ones.
- Do not add features, refactor code, or make improvements beyond what was asked. Match \
the scope of changes to what was actually requested.
- Be careful not to introduce security vulnerabilities such as command injection, path \
traversal, and other OWASP top 10 issues. If you notice insecure code you wrote, fix it \
immediately.
- If a task is ambiguous, ask for clarification instead of guessing.
- If an approach fails, diagnose why before retrying or switching tactics — read the error, \
check assumptions, try a focused fix. Do not retry the identical action blindly.";

const CAUTION: &str = "\
# Executing actions with care

Consider the reversibility and blast radius of actions. Local, reversible actions like \
editing files or running tests can proceed freely. For actions that are hard to reverse, \
affect shared systems, or could be destructive, ask the user before proceeding.

Examples of risky actions that warrant confirmation:

- Destructive: deleting files or branches, `rm -rf`, overwriting uncommitted changes.
- Hard to reverse: force-pushing, `git reset --hard`, amending published commits.
- Visible to others: pushing code, creating or commenting on PRs / issues.

When encountering unexpected state (unfamiliar files, branches, lock files), investigate \
before deleting or overwriting — it may be the user's in-progress work. Prefer fixing root \
causes over bypassing safety checks (e.g., do not use `--no-verify`).";

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
- Skip filler words and preamble. Go straight to the point.
- Focus text output on decisions that need user input, progress at milestones, and errors.
- Do not use emojis unless the user requests it.";

/// Build the complete system prompt for the agent.
///
/// Resolves the working directory and git root automatically, then delegates
/// to [`assemble`].
pub(crate) async fn build_system_prompt(model: &str) -> String {
    let cwd = std::env::current_dir().ok();
    let git_root = match &cwd {
        Some(cwd) => find_git_root(cwd).await,
        None => None,
    };

    assemble(model, cwd.as_deref(), git_root.as_deref()).await
}

/// Assemble the system prompt from explicit path parameters.
///
/// The prompt always begins with [`IDENTITY_PREFIX`] (required for OAuth)
/// followed by static guidance sections, a detected environment section, and
/// any discovered CLAUDE.md user instructions.
async fn assemble(model: &str, cwd: Option<&Path>, git_root: Option<&Path>) -> String {
    let (env, claude_md) = tokio::join!(
        Environment::detect(model, cwd, git_root),
        instructions::load(cwd, git_root),
    );

    let mut sections = vec![
        format!("{IDENTITY_PREFIX}\n{IDENTITY}"),
        TASK_GUIDANCE.to_owned(),
        CAUTION.to_owned(),
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
        assert!(prompt.contains("# Executing actions with care"));
        assert!(prompt.contains("# Using your tools"));
        assert!(prompt.contains("# Tone and style"));
        assert!(prompt.contains("# Environment"));
    }

    #[tokio::test]
    async fn build_system_prompt_includes_model_name() {
        let prompt = build_system_prompt("claude-opus-4-6").await;
        assert!(prompt.contains("Model: claude-opus-4-6"));
    }

    /// This test runs inside the oxide-code repo which has CLAUDE.md, so the
    /// non-empty instructions branch should be exercised.
    #[tokio::test]
    async fn build_system_prompt_includes_user_instructions() {
        let prompt = build_system_prompt("test-model").await;
        assert!(
            prompt.contains("# User instructions"),
            "expected user instructions from project CLAUDE.md"
        );
    }

    #[tokio::test]
    async fn build_system_prompt_sections_joined_with_double_newline() {
        let prompt = build_system_prompt("test-model").await;
        // Each section boundary is a double newline. Verify the identity
        // section is separated from the next by exactly "\n\n".
        let identity_end = prompt.find("# Doing tasks").expect("task guidance missing");
        let before = &prompt[..identity_end];
        assert!(
            before.ends_with("\n\n"),
            "sections should be joined with double newline"
        );
    }

    // ── assemble ──

    #[tokio::test]
    async fn assemble_in_git_repo_includes_all_sections_in_order() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("CLAUDE.md"), "Test project rules.").unwrap();

        let prompt = assemble("test-model", Some(tmp.path()), Some(tmp.path())).await;

        let expected_headers = [
            IDENTITY_PREFIX,
            "# Doing tasks",
            "# Executing actions with care",
            "# Using your tools",
            "# Tone and style",
            "# Environment",
            "# User instructions",
        ];
        let mut prev_pos = 0;
        for header in &expected_headers {
            let pos = prompt
                .find(header)
                .unwrap_or_else(|| panic!("missing section: {header}"));
            assert!(
                pos >= prev_pos,
                "{header} should come after previous section"
            );
            prev_pos = pos;
        }

        assert!(prompt.contains(&format!("Working directory: {}", tmp.path().display())));
        assert!(prompt.contains("Is a git repository: true"));
        assert!(prompt.contains("Model: test-model"));
        assert!(prompt.contains("Test project rules."));
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

        let prompt = assemble("test-model", Some(&sub), Some(root)).await;

        assert!(prompt.contains("Root rules."));
        assert!(prompt.contains("Subdir rules."));

        let root_pos = prompt.find("Root rules.").unwrap();
        let sub_pos = prompt.find("Subdir rules.").unwrap();
        assert!(
            root_pos < sub_pos,
            "root instructions should appear before subdirectory"
        );
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

    // ── helpers ──

    fn init_git_repo(path: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git init failed");
    }
}
