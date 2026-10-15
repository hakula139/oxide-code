//! System prompt assembly: cacheable static sections plus a per-session user-context reminder.

pub(crate) mod environment;
mod instructions;
mod sections;

use std::path::{Path, PathBuf};

use indoc::formatdoc;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use environment::Environment;
use sections::{
    CAUTION, INTRO, OUTPUT_EFFICIENCY, STYLE, SYSTEM_SECTION, TASK_GUIDANCE, TOOL_GUIDANCE,
};

/// Marker between cacheable static and per-session dynamic sections.
pub(crate) const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";

/// Output of [`build_prompt`] or [`restore_prompt`], held for a whole session so every request
/// shares a byte-identical prefix and the conversation history stays append-only.
///
/// `system_sections` is ordered: cacheable static sections, then [`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`]
/// as its own element, then per-session dynamic sections. The client splits on the marker to apply
/// `cache_control` to the static prefix only. `user_context` is the CLAUDE.md / AGENTS.md and
/// current-date reminder. It rides as the first user message because some gateways cap the size of
/// `system` blocks.
#[cfg_attr(test, derive(Default))]
pub(crate) struct PromptParts {
    environment: Environment,
    pub(crate) system_sections: Vec<String>,
    pub(crate) user_context: String,
    /// What the model has seen of the user context, in `user_context` or a later reminder.
    pub(crate) seen: UserContext,
}

impl PromptParts {
    /// Re-renders the system prompt for `model`. `user_context` stays untouched so the
    /// conversation history sent after the swap is unchanged.
    pub(crate) fn set_model(&mut self, model: &str) {
        self.environment.set_model(model);
        self.system_sections = system_sections(&self.environment);
    }

    /// Returns a reminder of how `fresh` differs from what the model has seen, then counts
    /// `fresh` as seen.
    pub(crate) fn catch_up(&mut self, fresh: UserContext) -> Option<String> {
        if fresh == self.seen {
            return None;
        }
        let reminder = render_reminder(
            (fresh.instructions != self.seen.instructions).then_some(fresh.instructions.as_str()),
            (fresh.date != self.seen.date).then_some(fresh.date.as_str()),
        );
        self.seen = fresh;
        Some(reminder)
    }

    /// Re-renders `user_context` from `fresh`, for a transcript no earlier request started with.
    pub(crate) fn refresh(&mut self, fresh: UserContext) {
        self.user_context = render_user_context(&fresh);
        self.seen = fresh;
    }

    /// What the session records so [`restore_prompt`] can rebuild this prompt on resume.
    pub(crate) fn session_context(&self) -> SessionContext {
        SessionContext {
            environment: self.environment.clone(),
            user_context: self.user_context.clone(),
            seen: self.seen.clone(),
        }
    }

    #[cfg(test)]
    fn system_joined(&self) -> String {
        self.system_sections.join("\n\n")
    }
}

/// The parts of the user context that change over a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserContext {
    /// Rendered instruction files, empty when there are none.
    pub(crate) instructions: String,
    /// Local date as `YYYY-MM-DD`.
    pub(crate) date: String,
}

impl UserContext {
    /// Reads the instruction files that apply in `cwd`, plus today's date.
    pub(crate) async fn load(cwd: &Path) -> Self {
        let git_root = find_git_root(cwd).await;
        Self::load_with_root(cwd, git_root.as_deref()).await
    }

    async fn load_with_root(cwd: &Path, git_root: Option<&Path>) -> Self {
        Self {
            instructions: instructions::load(cwd, git_root).await,
            date: environment::current_date(),
        }
    }
}

/// The prompt state a session records, which [`restore_prompt`] rebuilds on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionContext {
    environment: Environment,
    user_context: String,
    seen: UserContext,
}

/// Detects the environment and loads instruction files for a new session.
pub(crate) async fn build_prompt(model: &str, cwd: &Path) -> PromptParts {
    let git_root = find_git_root(cwd).await;
    assemble(model, cwd, git_root.as_deref()).await
}

/// Rebuilds the prompt `stored` describes, so a resumed session sends the prefix it sent before.
/// A session recorded in another directory gets a freshly detected environment.
pub(crate) async fn restore_prompt(stored: SessionContext, model: &str, cwd: &Path) -> PromptParts {
    let SessionContext {
        mut environment,
        user_context,
        seen,
    } = stored;
    if environment.describes(cwd) {
        environment.set_model(model);
    } else {
        let git_root = find_git_root(cwd).await;
        environment = Environment::detect(model, cwd, git_root.as_deref());
    }

    PromptParts {
        system_sections: system_sections(&environment),
        environment,
        user_context,
        seen,
    }
}

async fn assemble(model: &str, cwd: &Path, git_root: Option<&Path>) -> PromptParts {
    let environment = Environment::detect(model, cwd, git_root);
    let context = UserContext::load_with_root(cwd, git_root).await;

    PromptParts {
        system_sections: system_sections(&environment),
        environment,
        user_context: render_user_context(&context),
        seen: context,
    }
}

fn system_sections(environment: &Environment) -> Vec<String> {
    let env_section = environment.render();
    [
        INTRO,
        SYSTEM_SECTION,
        TASK_GUIDANCE,
        CAUTION,
        TOOL_GUIDANCE,
        STYLE,
        OUTPUT_EFFICIENCY,
        SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
        &env_section,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn render_user_context(context: &UserContext) -> String {
    let claude_md_section = if context.instructions.is_empty() {
        String::new()
    } else {
        formatdoc! {"
            # CLAUDE.md

            {instructions}

        ", instructions = context.instructions}
    };

    formatdoc! {"
        <system-reminder>
        As you answer the user's questions, you can use the following context:

        {claude_md_section}# Current date

        Today's date is {date}.

        IMPORTANT: this context may or may not be relevant to your tasks. You should
        not respond to this context unless it is highly relevant to your task.
        </system-reminder>",
        date = context.date,
    }
}

/// Reminder carrying the changed `instructions` and / or `date`, appended to a user message.
fn render_reminder(instructions: Option<&str>, date: Option<&str>) -> String {
    let mut sections = Vec::new();
    if let Some(instructions) = instructions {
        let body = if instructions.is_empty() {
            "The instruction files were removed, so the earlier instructions no longer apply."
        } else {
            instructions
        };
        sections.push(formatdoc! {"
            # CLAUDE.md

            {body}"
        });
    }
    if let Some(date) = date {
        sections.push(formatdoc! {"
            # Current date

            Today's date is {date}."
        });
    }

    formatdoc! {"
        <system-reminder>
        The context from the start of this conversation has changed. Use these values from now on:

        {sections}
        </system-reminder>",
        sections = sections.join("\n\n"),
    }
}

/// Whether `text` is a context reminder, which transcript displays leave out.
pub(crate) fn is_system_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>")
}

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

    // ── PromptParts::set_model ──

    #[tokio::test]
    async fn set_model_rerenders_system_and_keeps_user_context() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let mut parts = assemble("claude-opus-4-6", tmp.path(), None).await;
        let user_context = parts.user_context.clone();

        parts.set_model("claude-sonnet-4-6");

        let joined = parts.system_joined();
        assert!(joined.contains("The exact model ID is claude-sonnet-4-6."));
        assert!(!joined.contains("claude-opus-4-6"));
        assert_eq!(parts.user_context, user_context);
    }

    // ── PromptParts::catch_up ──

    #[test]
    fn catch_up_reminds_only_of_what_changed() {
        let mut parts = PromptParts {
            seen: context("Old rules.", "2026-04-12"),
            ..PromptParts::default()
        };

        assert_eq!(parts.catch_up(context("Old rules.", "2026-04-12")), None);

        let reminder = parts.catch_up(context("Old rules.", "2026-04-13")).unwrap();
        assert!(reminder.starts_with("<system-reminder>"));
        assert!(reminder.contains("# Current date\n\nToday's date is 2026-04-13."));
        assert!(!reminder.contains("# CLAUDE.md"), "{reminder}");

        let reminder = parts.catch_up(context("New rules.", "2026-04-13")).unwrap();
        assert!(reminder.contains("# CLAUDE.md\n\nNew rules."));
        assert!(!reminder.contains("# Current date"), "{reminder}");

        let reminder = parts.catch_up(context("", "2026-04-13")).unwrap();
        assert!(reminder.contains("The instruction files were removed"));
        assert_eq!(parts.seen, context("", "2026-04-13"));
        assert!(
            parts.user_context.is_empty(),
            "catching up must leave the first message alone"
        );
    }

    // ── PromptParts::refresh ──

    #[test]
    fn refresh_rerenders_user_context_and_counts_it_as_seen() {
        let mut parts = PromptParts {
            user_context: "stale".to_owned(),
            ..PromptParts::default()
        };

        parts.refresh(context("Rules.", "2026-04-13"));

        assert_eq!(
            parts.user_context,
            render_user_context(&context("Rules.", "2026-04-13"))
        );
        assert_eq!(parts.catch_up(context("Rules.", "2026-04-13")), None);
    }

    // ── build_prompt ──

    #[tokio::test]
    async fn build_prompt_system_starts_with_intro() {
        let parts = build_prompt("test-model", &cwd()).await;
        assert!(
            parts
                .system_joined()
                .starts_with("You are an interactive agent"),
            "system should start with intro section"
        );
    }

    #[tokio::test]
    async fn build_prompt_system_contains_all_static_sections() {
        let parts = build_prompt("test-model", &cwd()).await;
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
        let parts = build_prompt("test-model", &cwd()).await;
        assert!(parts.system_joined().contains("test-model"));
    }

    #[tokio::test]
    async fn build_prompt_system_does_not_contain_user_instructions() {
        let parts = build_prompt("test-model", &cwd()).await;
        assert!(
            !parts.system_joined().contains("# User instructions"),
            "CLAUDE.md should be in user_context, not system"
        );
    }

    #[tokio::test]
    async fn build_prompt_sections_joined_with_double_newline() {
        let parts = build_prompt("test-model", &cwd()).await;
        let joined = parts.system_joined();
        let system_start = joined.find("# System").expect("system section missing");
        let before = &joined[..system_start];
        assert!(
            before.ends_with("\n\n"),
            "sections should be joined with double newline"
        );
    }

    // ── restore_prompt ──

    #[tokio::test]
    async fn restore_prompt_replays_the_recording_in_its_directory() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");

        let parts = restore_prompt(
            recorded_context(tmp.path()),
            "claude-sonnet-4-6",
            tmp.path(),
        )
        .await;

        let joined = parts.system_joined();
        assert!(joined.contains("- Platform: plan9"), "{joined}");
        assert!(joined.contains("The exact model ID is claude-sonnet-4-6."));
        assert_eq!(parts.user_context, "recorded context");
        assert_eq!(parts.seen, context("Recorded rules.", "2026-04-12"));
    }

    #[tokio::test]
    async fn restore_prompt_detects_the_environment_again_in_another_directory() {
        let recorded_in = tempfile::tempdir().expect("failed to create tempdir");
        let resumed_in = tempfile::tempdir().expect("failed to create tempdir");

        let parts = restore_prompt(
            recorded_context(recorded_in.path()),
            "claude-opus-4-6",
            resumed_in.path(),
        )
        .await;

        let joined = parts.system_joined();
        assert!(!joined.contains("plan9"), "{joined}");
        assert!(joined.contains(&format!(
            "Primary working directory: {}",
            resumed_in.path().display()
        )));
        assert_eq!(parts.user_context, "recorded context");
    }

    // ── assemble ──

    #[tokio::test]
    async fn assemble_in_git_repo_has_system_and_user_context() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("CLAUDE.md"), "Test project rules.").unwrap();

        let parts = assemble("test-model", tmp.path(), Some(tmp.path())).await;

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

        // Boundary marker must be a distinct element so the API client can split it out.
        assert!(
            parts
                .system_sections
                .iter()
                .any(|s| s == SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
            "system_sections should contain the boundary marker"
        );

        let ctx = &parts.user_context;
        assert!(ctx.contains("Test project rules."));
        assert!(ctx.contains("<system-reminder>"));
    }

    #[tokio::test]
    async fn assemble_without_project_claude_md_keeps_system_clean() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        init_git_repo(tmp.path());

        let parts = assemble("test-model", tmp.path(), Some(tmp.path())).await;
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

        let parts = assemble("test-model", &sub, Some(root)).await;
        let ctx = &parts.user_context;

        assert!(ctx.contains("Root rules."));
        assert!(ctx.contains("Subdir rules."));

        let root_pos = ctx.find("Root rules.").unwrap();
        let sub_pos = ctx.find("Subdir rules.").unwrap();
        assert!(
            root_pos < sub_pos,
            "root instructions should appear before subdirectory"
        );
    }

    // ── render_user_context ──

    #[test]
    fn render_user_context_with_instructions() {
        let ctx = render_user_context(&context("Project rules.", "2026-04-12"));
        assert!(ctx.starts_with("<system-reminder>"));
        assert!(ctx.ends_with("</system-reminder>"));
        assert!(ctx.contains("# CLAUDE.md\n\nProject rules."));
        assert!(ctx.contains("# Current date\n\nToday's date is 2026-04-12."));
        assert!(ctx.contains("IMPORTANT: this context may or may not be relevant"));
    }

    #[test]
    fn render_user_context_without_instructions_still_carries_date() {
        let ctx = render_user_context(&context("", "2026-04-12"));
        assert!(ctx.starts_with("<system-reminder>"));
        assert!(ctx.contains(indoc::indoc! {"
            following context:

            # Current date

            Today's date is 2026-04-12.
        "}));
        assert!(!ctx.contains("# CLAUDE.md"));
    }

    // ── find_git_root ──

    #[tokio::test]
    async fn find_git_root_inside_repo() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        init_git_repo(tmp.path());

        let root = find_git_root(tmp.path()).await.expect("expected git root");
        // Compare via canonicalize — macOS routes tempdirs through `/private`.
        let expected = tmp.path().canonicalize().unwrap();
        let actual = root.canonicalize().unwrap();
        assert_eq!(actual, expected);
        assert!(actual.join(".git").exists(), "root should contain .git");
    }

    #[tokio::test]
    async fn find_git_root_outside_repo() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let root = find_git_root(tmp.path()).await;
        assert!(root.is_none());
    }

    // ── constant content ──

    #[test]
    fn prompt_constants_have_no_leading_whitespace() {
        // Catch `indoc!` regressions that would leak source indentation.
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

    fn cwd() -> PathBuf {
        std::env::current_dir().expect("cwd should be available")
    }

    fn context(instructions: &str, date: &str) -> UserContext {
        UserContext {
            instructions: instructions.to_owned(),
            date: date.to_owned(),
        }
    }

    /// A recording from a machine unlike this one, so a replayed environment is recognizable.
    fn recorded_context(cwd: &Path) -> SessionContext {
        serde_json::from_value(serde_json::json!({
            "environment": {
                "cwd": cwd,
                "is_git": false,
                "platform": "plan9",
                "shell": "rc",
                "os_version": "Plan 9 4e",
                "model": "claude-opus-4-6",
            },
            "user_context": "recorded context",
            "seen": {"instructions": "Recorded rules.", "date": "2026-04-12"},
        }))
        .unwrap()
    }

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
