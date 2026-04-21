//! System prompt assembly.
//!
//! Builds [`PromptParts`]: static sections (identity, guidance, tool
//! use) that live in the API `system` parameter and cache globally,
//! plus a `<system-reminder>`-wrapped user context (CLAUDE.md, date)
//! prepended to the messages array so per-session content doesn't
//! invalidate the static cache.

pub(crate) mod environment;
mod instructions;
mod sections;

use std::path::{Path, PathBuf};

use indoc::formatdoc;
use tokio::process::Command;

use environment::Environment;
use sections::{
    CAUTION, INTRO, OUTPUT_EFFICIENCY, STYLE, SYSTEM_SECTION, TASK_GUIDANCE, TOOL_GUIDANCE,
};

/// Marker between static (globally cacheable) and dynamic (per-session)
/// system prompt sections. Used by the API client to apply `cache_control`
/// scopes to the static portion.
pub(crate) const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";

/// Assembled prompt split into two API surfaces.
///
/// `system_sections` contains the static system prompt sections — one
/// per API text block, so `cache_control` can apply to the static
/// portion without re-caching on every turn. `user_context` contains
/// dynamic content (CLAUDE.md, date) that is prepended to the
/// `messages` array as a `<system-reminder>`-wrapped user message, so
/// per-session content doesn't invalidate the static cache.
pub(crate) struct PromptParts {
    pub(crate) system_sections: Vec<String>,
    pub(crate) user_context: Option<String>,
}

impl PromptParts {
    /// Join all system sections into a single string for testing / display.
    #[cfg(test)]
    fn system_joined(&self) -> String {
        self.system_sections.join("\n\n")
    }
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
    let env = Environment::detect(model, cwd, git_root);
    let claude_md = instructions::load(cwd, git_root).await;

    let env_section = env.render();
    let system_sections: Vec<String> = [
        // Static content (globally cacheable)
        INTRO,
        SYSTEM_SECTION,
        TASK_GUIDANCE,
        CAUTION,
        TOOL_GUIDANCE,
        STYLE,
        OUTPUT_EFFICIENCY,
        // Cache boundary
        SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
        // Dynamic content (per-session)
        &env_section,
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let user_context = build_user_context(&claude_md, &env.date());

    PromptParts {
        system_sections,
        user_context,
    }
}

/// Build the `<system-reminder>` user message content from dynamic context.
///
/// CLAUDE.md and date ride in a synthetic user message rather than the
/// `system` parameter so per-session content doesn't invalidate the
/// static-section prompt cache.
fn build_user_context(claude_md: &str, date: &str) -> Option<String> {
    if claude_md.is_empty() {
        return None;
    }

    Some(formatdoc! {"
        <system-reminder>
        As you answer the user's questions, you can use the following context:

        # CLAUDE.md

        {claude_md}

        # Current date

        {date}

        IMPORTANT: this context may or may not be relevant to your tasks. You should
        not respond to this context unless it is highly relevant to your task.
        </system-reminder>"
    })
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
    async fn build_prompt_system_starts_with_intro() {
        let parts = build_prompt("test-model").await;
        assert!(
            parts
                .system_joined()
                .starts_with("You are an interactive agent"),
            "system should start with intro section"
        );
    }

    #[tokio::test]
    async fn build_prompt_system_contains_all_static_sections() {
        let parts = build_prompt("test-model").await;
        let joined = parts.system_joined();
        assert!(joined.contains("# System"));
        assert!(joined.contains("# Doing tasks"));
        assert!(joined.contains("# Executing actions with care"));
        assert!(joined.contains("# Using your tools"));
        assert!(joined.contains("# Tone and style"));
        assert!(joined.contains("# Output efficiency"));
        assert!(joined.contains("# Environment"));
    }

    #[tokio::test]
    async fn build_prompt_system_includes_model_name() {
        let parts = build_prompt("test-model").await;
        assert!(parts.system_joined().contains("test-model"));
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
        assert!(ctx.contains("# CLAUDE.md"));
        assert!(ctx.contains("# Current date"));
        assert!(ctx.contains("</system-reminder>"));
    }

    #[tokio::test]
    async fn build_prompt_system_does_not_contain_user_instructions() {
        let parts = build_prompt("test-model").await;
        assert!(
            !parts.system_joined().contains("# User instructions"),
            "CLAUDE.md should be in user_context, not system"
        );
    }

    #[tokio::test]
    async fn build_prompt_sections_joined_with_double_newline() {
        let parts = build_prompt("test-model").await;
        // Each section boundary is a double newline. Verify the intro
        // section is separated from the next by exactly "\n\n".
        let joined = parts.system_joined();
        let system_start = joined.find("# System").expect("system section missing");
        let before = &joined[..system_start];
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
            "You are an interactive agent",
            "# System",
            "# Doing tasks",
            "# Executing actions with care",
            "# Using your tools",
            "# Tone and style",
            "# Output efficiency",
            "# Environment",
        ];
        let joined = parts.system_joined();
        let mut prev_pos = 0;
        for header in &expected_system_sections {
            let pos = joined
                .find(header)
                .unwrap_or_else(|| panic!("missing section: {header}"));
            assert!(
                pos >= prev_pos,
                "{header} should come after previous section"
            );
            prev_pos = pos;
        }

        assert!(joined.contains(&format!(
            "Primary working directory: {}",
            tmp.path().display()
        )));
        assert!(joined.contains("Is a git repository: true"));
        assert!(joined.contains("test-model"));

        // Boundary marker must be a distinct element in system_sections
        // so the API client can split static from dynamic content.
        assert!(
            parts
                .system_sections
                .iter()
                .any(|s| s == SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
            "system_sections should contain the boundary marker"
        );

        // CLAUDE.md goes into user_context as <system-reminder>
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
            !parts.system_joined().contains("# User instructions"),
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
        assert!(ctx.contains("# CLAUDE.md\n\nProject rules."));
        assert!(ctx.contains("# Current date\n\nToday's date is 2026-04-12."));
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
            ("INTRO", INTRO),
            ("SYSTEM_SECTION", SYSTEM_SECTION),
            ("TASK_GUIDANCE", TASK_GUIDANCE),
            ("CAUTION", CAUTION),
            ("TOOL_GUIDANCE", TOOL_GUIDANCE),
            ("STYLE", STYLE),
            ("OUTPUT_EFFICIENCY", OUTPUT_EFFICIENCY),
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
