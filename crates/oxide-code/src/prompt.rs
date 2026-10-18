//! System prompt assembly: cacheable static sections plus a per-session user-context reminder.

pub(crate) mod environment;
pub(crate) mod instructions;
mod sections;

use std::path::Path;

use indoc::formatdoc;
use serde::{Deserialize, Serialize};

use crate::model::ModelSpec;
use crate::util::git::repo_root;
use environment::Environment;
use sections::{
    CAUTION, INTRO, OUTPUT_EFFICIENCY, STYLE, SYSTEM_SECTION, TASK_GUIDANCE, TOOL_GUIDANCE,
};

/// Marker between cacheable static and per-session dynamic sections.
pub(crate) const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";

/// Output of [`build_prompt`] or [`restore_prompt`], held for a whole session so every request
/// shares a byte-identical prefix.
#[cfg_attr(test, derive(Default))]
pub(crate) struct PromptParts {
    environment: Environment,
    /// The static sections, [`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`] as its own element, then the
    /// per-session sections. The client applies `cache_control` to the prefix before the marker.
    pub(crate) system_sections: Vec<String>,
    /// The instruction files, date, and model identity reminder, sent as the first user message so
    /// a `/model` swap leaves the system prompt byte-identical.
    pub(crate) user_context: String,
    /// What the model has seen of the user context, in `user_context` or a later reminder.
    pub(crate) seen: UserContext,
}

impl PromptParts {
    /// Returns a reminder of how `fresh` differs from what the model has seen, then counts
    /// `fresh` as seen.
    pub(crate) fn catch_up(&mut self, fresh: UserContext) -> Option<String> {
        if fresh == self.seen {
            return None;
        }
        let reminder = render_reminder(&self.seen, &fresh);
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
    /// Rendered model name, exact ID, and knowledge cutoff.
    pub(crate) model: String,
}

impl UserContext {
    /// Reads the instruction files that apply in `cwd`, plus today's date and `model`'s identity.
    /// Also returns a warning for each instruction file that exists but cannot be read.
    pub(crate) async fn load(cwd: &Path, model: &ModelSpec) -> (Self, Vec<String>) {
        let git_root = repo_root(cwd).await;
        Self::load_with_root(cwd, git_root.as_deref(), model).await
    }

    async fn load_with_root(
        cwd: &Path,
        git_root: Option<&Path>,
        model: &ModelSpec,
    ) -> (Self, Vec<String>) {
        let (instructions, warnings) = instructions::load(cwd, git_root).await;
        let context = Self {
            instructions,
            date: environment::current_date(),
            model: environment::model_identity(model),
        };
        (context, warnings)
    }
}

/// The prompt state a session records, which [`restore_prompt`] rebuilds on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionContext {
    environment: Environment,
    user_context: String,
    seen: UserContext,
}

/// Detects the environment and loads instruction files for a new session. Also returns the
/// warnings [`UserContext::load`] does.
pub(crate) async fn build_prompt(model: &ModelSpec, cwd: &Path) -> (PromptParts, Vec<String>) {
    let git_root = repo_root(cwd).await;
    assemble(model, cwd, git_root.as_deref()).await
}

/// Rebuilds the prompt `stored` describes, so a resumed session sends the prefix it sent before.
/// A session recorded in another directory gets a freshly detected environment.
pub(crate) async fn restore_prompt(stored: SessionContext, cwd: &Path) -> PromptParts {
    let SessionContext {
        mut environment,
        user_context,
        seen,
    } = stored;
    if !environment.describes(cwd) {
        let git_root = repo_root(cwd).await;
        environment = Environment::detect(cwd, git_root.as_deref());
    }

    PromptParts {
        system_sections: system_sections(&environment),
        environment,
        user_context,
        seen,
    }
}

async fn assemble(
    model: &ModelSpec,
    cwd: &Path,
    git_root: Option<&Path>,
) -> (PromptParts, Vec<String>) {
    let environment = Environment::detect(cwd, git_root);
    let (context, warnings) = UserContext::load_with_root(cwd, git_root, model).await;

    let parts = PromptParts {
        system_sections: system_sections(&environment),
        environment,
        user_context: render_user_context(&context),
        seen: context,
    };
    (parts, warnings)
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
    let mut sections = Vec::new();
    if !context.instructions.is_empty() {
        sections.push(render_instructions(&context.instructions));
    }
    sections.push(render_date(&context.date));
    sections.push(render_model(&context.model));

    let sections = sections.join("\n\n");
    system_reminder(&formatdoc! {"
        As you answer the user's questions, you can use the following context:

        {sections}

        IMPORTANT: this context may or may not be relevant to your tasks. You should
        not respond to this context unless it is highly relevant to your task."
    })
}

/// Reminder carrying each part of `fresh` that differs from `seen`, appended to a user message.
fn render_reminder(seen: &UserContext, fresh: &UserContext) -> String {
    let mut sections = Vec::new();
    if fresh.instructions != seen.instructions {
        let body = if fresh.instructions.is_empty() {
            "The instruction files were removed, so the earlier instructions no longer apply."
        } else {
            &fresh.instructions
        };
        sections.push(render_instructions(body));
    }
    if fresh.date != seen.date {
        sections.push(render_date(&fresh.date));
    }
    if fresh.model != seen.model {
        sections.push(render_model(&fresh.model));
    }

    let sections = sections.join("\n\n");
    system_reminder(&formatdoc! {"
        The context from the start of this conversation has changed. Use these values from now on:

        {sections}"
    })
}

fn render_instructions(instructions: &str) -> String {
    formatdoc! {"
        # CLAUDE.md

        {instructions}"
    }
}

fn render_date(date: &str) -> String {
    formatdoc! {"
        # Current date

        Today's date is {date}."
    }
}

fn render_model(model: &str) -> String {
    formatdoc! {"
        # Model

        {model}"
    }
}

/// `body` wrapped in the `<system-reminder>` block that [`is_system_reminder`] detects.
pub(crate) fn system_reminder(body: &str) -> String {
    formatdoc! {"
        <system-reminder>
        {body}
        </system-reminder>"
    }
}

/// Returns whether `text` is a `<system-reminder>` block, which transcript displays leave out.
pub(crate) fn is_system_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;

    use super::*;
    use crate::util::git::testing::isolated_git;

    const IDENTITY: &str = "You are powered by the model test-model.";

    fn cwd() -> PathBuf {
        std::env::current_dir().expect("cwd should be available")
    }

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::resolve(id.to_owned())
    }

    fn context(instructions: &str, date: &str) -> UserContext {
        UserContext {
            instructions: instructions.to_owned(),
            date: date.to_owned(),
            model: IDENTITY.to_owned(),
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
            },
            "user_context": "recorded context",
            "seen": {
                "instructions": "Recorded rules.",
                "date": "2026-04-12",
                "model": IDENTITY,
            },
        }))
        .unwrap()
    }

    fn init_git_repo(path: &Path) {
        let status = isolated_git(path)
            .args(["init"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to spawn git");
        assert!(status.success(), "git init exited with non-zero status");
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
        assert!(reminder.contains(indoc! {"
            # Current date

            Today's date is 2026-04-13."
        }));
        assert!(!reminder.contains("# CLAUDE.md"), "{reminder}");

        let reminder = parts.catch_up(context("New rules.", "2026-04-13")).unwrap();
        assert!(reminder.contains(indoc! {"
            # CLAUDE.md

            New rules."
        }));
        assert!(!reminder.contains("# Current date"), "{reminder}");

        let swapped = UserContext {
            model: "You are powered by the model other-model.".to_owned(),
            ..context("New rules.", "2026-04-13")
        };
        let reminder = parts.catch_up(swapped).unwrap();
        assert!(reminder.contains(indoc! {"
            # Model

            You are powered by the model other-model."
        }));
        assert!(!reminder.contains("# CLAUDE.md"), "{reminder}");

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
    async fn build_prompt_names_the_model_in_user_context_only() {
        let model = spec("claude-opus-5-5");
        let (parts, _) = build_prompt(&model, &cwd()).await;
        let identity = environment::model_identity(&model);
        assert!(
            parts.user_context.contains(&formatdoc! {"
                # Model

                {identity}"
            }),
            "{}",
            parts.user_context,
        );
        assert_eq!(parts.seen.model, identity);
        assert!(!parts.system_joined().contains("claude-opus-5-5"));
    }

    // ── restore_prompt ──

    #[tokio::test]
    async fn restore_prompt_replays_the_recording_in_its_directory() {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");

        let parts = restore_prompt(recorded_context(tmp.path()), tmp.path()).await;

        let joined = parts.system_joined();
        assert!(joined.contains("- Platform: plan9"), "{joined}");
        assert_eq!(parts.user_context, "recorded context");
        assert_eq!(parts.seen, context("Recorded rules.", "2026-04-12"));
    }

    #[tokio::test]
    async fn restore_prompt_detects_the_environment_again_in_another_directory() {
        let recorded_in = tempfile::tempdir().expect("failed to create tempdir");
        let resumed_in = tempfile::tempdir().expect("failed to create tempdir");

        let parts = restore_prompt(recorded_context(recorded_in.path()), resumed_in.path()).await;

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

        let (parts, _) = assemble(&spec("test-model"), tmp.path(), Some(tmp.path())).await;

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
        assert!(joined.starts_with(INTRO));
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

        assert!(
            parts
                .system_sections
                .iter()
                .any(|s| s == SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
            "system_sections should contain the boundary marker"
        );

        assert!(!joined.contains("Test project rules."));

        let ctx = &parts.user_context;
        assert!(ctx.contains("Test project rules."));
        assert!(ctx.contains("<system-reminder>"));
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

        let (parts, _) = assemble(&spec("test-model"), &sub, Some(root)).await;
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
    fn render_user_context_with_instructions_includes_every_section() {
        assert_eq!(
            render_user_context(&context("Project rules.", "2026-04-12")),
            indoc! {"
                <system-reminder>
                As you answer the user's questions, you can use the following context:

                # CLAUDE.md

                Project rules.

                # Current date

                Today's date is 2026-04-12.

                # Model

                You are powered by the model test-model.

                IMPORTANT: this context may or may not be relevant to your tasks. You should
                not respond to this context unless it is highly relevant to your task.
                </system-reminder>"
            },
        );
    }

    #[test]
    fn render_user_context_without_instructions_still_carries_date() {
        assert_eq!(
            render_user_context(&context("", "2026-04-12")),
            indoc! {"
                <system-reminder>
                As you answer the user's questions, you can use the following context:

                # Current date

                Today's date is 2026-04-12.

                # Model

                You are powered by the model test-model.

                IMPORTANT: this context may or may not be relevant to your tasks. You should
                not respond to this context unless it is highly relevant to your task.
                </system-reminder>"
            },
        );
    }

    // ── render_reminder ──

    #[test]
    fn render_reminder_includes_every_changed_section_in_order() {
        assert_eq!(
            render_reminder(
                &UserContext::default(),
                &context("Project rules.", "2026-04-12")
            ),
            indoc! {"
                <system-reminder>
                The context from the start of this conversation has changed. Use these values from now on:

                # CLAUDE.md

                Project rules.

                # Current date

                Today's date is 2026-04-12.

                # Model

                You are powered by the model test-model.
                </system-reminder>"
            },
        );
    }

    #[test]
    fn render_reminder_removed_instructions_revoke_earlier_rules() {
        assert_eq!(
            render_reminder(
                &context("Project rules.", "2026-04-12"),
                &context("", "2026-04-12")
            ),
            indoc! {"
                <system-reminder>
                The context from the start of this conversation has changed. Use these values from now on:

                # CLAUDE.md

                The instruction files were removed, so the earlier instructions no longer apply.
                </system-reminder>"
            },
        );
    }

    // ── sections ──

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
}
