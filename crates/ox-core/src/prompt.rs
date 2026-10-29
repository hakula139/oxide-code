//! System prompt assembly: stable static sections plus a per-session user-context reminder.

mod environment;
pub(crate) mod instructions;
mod sections;

use std::path::Path;

use indoc::formatdoc;
use serde::{Deserialize, Serialize};

use self::environment::Environment;
use self::sections::{
    CAUTION, INTRO, OUTPUT_EFFICIENCY, STYLE, SYSTEM_SECTION, TASK_GUIDANCE, TOOL_GUIDANCE,
};
use crate::git::repo_root;
use crate::model::ModelSpec;

/// Output of [`build_prompt`] or [`restore_prompt`], held for a whole session so every request
/// shares a byte-identical prefix.
#[cfg_attr(test, derive(Default))]
pub(crate) struct PromptParts {
    environment: Environment,
    pub(crate) system: SystemPrompt,
    /// The instruction files, date, and model identity reminder, sent as the first user message so
    /// a `/model` swap leaves the system prompt byte-identical.
    pub(crate) user_context: String,
    /// What the model has seen of the user context, in `user_context` or a later reminder.
    pub(crate) seen: UserContext,
}

impl PromptParts {
    /// Returns a reminder of how `fresh` differs from what the model has seen, then counts `fresh`
    /// as seen.
    pub(crate) fn catch_up(&mut self, fresh: UserContext) -> Option<String> {
        if fresh == self.seen {
            return None;
        }
        let reminder = render_reminder(&self.seen, &fresh);
        self.seen = fresh;
        Some(reminder)
    }

    /// Re-renders `user_context` from `fresh` for a transcript no earlier request started with.
    /// [`Self::adopt`] installs it.
    pub(crate) fn refreshed(&self, fresh: UserContext) -> SessionContext {
        SessionContext {
            environment: self.environment.clone(),
            user_context: render_user_context(&fresh),
            seen: fresh,
        }
    }

    /// Switches to `context`, which [`Self::refreshed`] built from this prompt.
    pub(crate) fn adopt(&mut self, context: SessionContext) {
        self.user_context = context.user_context;
        self.seen = context.seen;
    }

    /// What the session records so [`restore_prompt`] can rebuild this prompt on resume.
    pub(crate) fn session_context(&self) -> SessionContext {
        SessionContext {
            environment: self.environment.clone(),
            user_context: self.user_context.clone(),
            seen: self.seen.clone(),
        }
    }
}

/// The system prompt, split where its per-session text begins.
#[cfg_attr(test, derive(Default))]
pub(crate) struct SystemPrompt {
    /// Text identical in every session, which a wire may cache across sessions.
    pub(crate) stable: String,
    /// Text that varies by session, such as the environment. Empty when there is none.
    pub(crate) dynamic: String,
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
    /// Also returns a warning for each instruction file that exists but cannot or may not be read.
    pub(crate) async fn load(cwd: &Path, model: &ModelSpec) -> (Self, Vec<String>) {
        let (instructions, warnings) = instructions::load(cwd).await;
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
#[cfg_attr(test, derive(Default))]
pub(crate) struct SessionContext {
    environment: Environment,
    user_context: String,
    seen: UserContext,
}

/// Detects the environment and loads instruction files for a new session. Also returns the warnings
/// [`UserContext::load`] does.
pub(crate) async fn build_prompt(model: &ModelSpec, cwd: &Path) -> (PromptParts, Vec<String>) {
    let git_root = repo_root(cwd).await;
    assemble(model, cwd, git_root.as_deref()).await
}

/// Rebuilds the prompt `stored` describes, so a resumed session sends the prefix it sent before. A
/// session recorded in another directory gets a freshly detected environment.
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
        system: system_prompt(&environment),
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
    let (context, warnings) = UserContext::load(cwd, model).await;

    let parts = PromptParts {
        system: system_prompt(&environment),
        environment,
        user_context: render_user_context(&context),
        seen: context,
    };
    (parts, warnings)
}

fn system_prompt(environment: &Environment) -> SystemPrompt {
    SystemPrompt {
        stable: [
            INTRO,
            SYSTEM_SECTION,
            TASK_GUIDANCE,
            CAUTION,
            TOOL_GUIDANCE,
            STYLE,
            OUTPUT_EFFICIENCY,
        ]
        .join("\n\n"),
        dynamic: environment.render(),
    }
}

fn render_user_context(context: &UserContext) -> String {
    let mut sections = Vec::new();
    if !context.instructions.is_empty() {
        sections.push(context.instructions.clone());
    }
    sections.push(render_date(&context.date));
    sections.push(render_model(&context.model));

    let sections = sections.join("\n\n");
    system_reminder(&formatdoc! {"
        As you answer the user's questions, you can use the following context:

        {sections}

        IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task."
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
        sections.push(body.to_owned());
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

/// `body` wrapped in the model-facing `<system-reminder>` envelope.
pub(crate) fn system_reminder(body: &str) -> String {
    formatdoc! {"
        <system-reminder>
        {body}
        </system-reminder>"
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::format_description;
    use time::{Date, OffsetDateTime};

    use super::*;
    use crate::git::local_testing::init_fixture_repo;
    use crate::git::testing::with_isolated_git_env_async;
    use crate::util::path::testing::pinned_home;

    const IDENTITY: &str = "You are powered by the model test-model.";

    fn context(instructions: &str, date: &str) -> UserContext {
        UserContext {
            instructions: instructions.to_owned(),
            date: date.to_owned(),
            model: IDENTITY.to_owned(),
        }
    }

    mod catch_up {
        use super::*;

        #[test]
        fn reminds_only_of_what_changed() {
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
            assert!(!reminder.contains("# User instructions"), "{reminder}");

            let reminder = parts.catch_up(context("New rules.", "2026-04-13")).unwrap();
            assert!(reminder.contains(indoc! {"
                The context from the start of this conversation has changed. Use these values from now on:

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
            assert!(!reminder.contains("# User instructions"), "{reminder}");

            let reminder = parts.catch_up(context("", "2026-04-13")).unwrap();
            assert!(reminder.contains("The instruction files were removed"));
            assert_eq!(parts.seen, context("", "2026-04-13"));
            assert!(
                parts.user_context.is_empty(),
                "catching up must leave the first message alone"
            );
        }
    }

    mod refreshed {
        use super::*;

        #[test]
        fn rerenders_user_context_and_counts_it_as_seen_once_adopted() {
            let mut parts = PromptParts {
                user_context: "stale".to_owned(),
                ..PromptParts::default()
            };

            let refreshed = parts.refreshed(context("Rules.", "2026-04-13"));
            parts.adopt(refreshed);

            assert_eq!(
                parts.user_context,
                render_user_context(&context("Rules.", "2026-04-13"))
            );
            assert_eq!(parts.catch_up(context("Rules.", "2026-04-13")), None);
        }
    }

    mod build_prompt {
        use super::*;

        #[tokio::test]
        async fn names_the_model_in_user_context_only() {
            let (home, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let model = ModelSpec::resolve("claude-opus-5-5");

            let (parts, warnings) = with_isolated_git_env_async(temp_env::async_with_vars(
                pinned_home(home.path()),
                build_prompt(&model, cwd.path()),
            ))
            .await;

            assert_eq!(warnings, [] as [String; 0]);
            assert_eq!(parts.seen.instructions, "");
            let date = Date::parse(
                &parts.seen.date,
                format_description!("[year]-[month]-[day]"),
            )
            .unwrap();
            let today = OffsetDateTime::now_utc().date();
            assert!(
                (today.previous_day().unwrap()..=today.next_day().unwrap()).contains(&date),
                "{date} is not within a day of {today}",
            );
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
            assert!(!parts.system.stable.contains("claude-opus-5-5"));
            assert!(!parts.system.dynamic.contains("claude-opus-5-5"));
        }
    }

    mod restore_prompt {
        use super::*;

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

        #[tokio::test]
        async fn replays_the_recording_in_its_directory() {
            let tmp = tempfile::tempdir().expect("failed to create tempdir");

            let parts = restore_prompt(recorded_context(tmp.path()), tmp.path()).await;

            let dynamic = &parts.system.dynamic;
            assert!(dynamic.contains("- Platform: plan9"), "{dynamic}");
            assert_eq!(parts.user_context, "recorded context");
            assert_eq!(parts.seen, context("Recorded rules.", "2026-04-12"));
        }

        #[tokio::test]
        async fn detects_the_environment_again_in_another_directory() {
            let recorded_in = tempfile::tempdir().expect("failed to create tempdir");
            let resumed_in = tempfile::tempdir().expect("failed to create tempdir");

            let parts =
                restore_prompt(recorded_context(recorded_in.path()), resumed_in.path()).await;

            let dynamic = &parts.system.dynamic;
            assert!(!dynamic.contains("plan9"), "{dynamic}");
            assert!(dynamic.contains(&format!(
                "Primary working directory: {}",
                resumed_in.path().display()
            )));
            assert_eq!(parts.user_context, "recorded context");
        }
    }

    mod assemble {
        use super::*;

        #[tokio::test]
        async fn with_a_git_root_has_system_and_user_context() {
            let (home, tmp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let path = tmp.path().join("CLAUDE.md");
            std::fs::write(&path, "Test project rules.").unwrap();
            let model = ModelSpec::resolve("test-model");

            let (parts, warnings) = temp_env::async_with_vars(
                pinned_home(home.path()),
                assemble(&model, tmp.path(), Some(tmp.path())),
            )
            .await;
            assert_eq!(warnings, [] as [String; 0]);

            let expected_stable_sections = [
                "You are an interactive agent",
                "# System",
                "# Doing tasks",
                "# Executing actions with care",
                "# Using your tools",
                "# Tone and style",
                "# Output efficiency",
            ];
            let SystemPrompt { stable, dynamic } = &parts.system;
            assert!(stable.starts_with(INTRO));
            let mut prev_pos = 0;
            for header in &expected_stable_sections {
                let pos = stable
                    .find(header)
                    .unwrap_or_else(|| panic!("missing section: {header}"));
                assert!(
                    pos >= prev_pos,
                    "{header} should come after previous section"
                );
                prev_pos = pos;
            }

            assert!(dynamic.starts_with("# Environment"), "{dynamic}");
            assert!(dynamic.contains(&format!(
                "Primary working directory: {}",
                tmp.path().display()
            )));
            assert!(dynamic.contains("Is a git repository: true"));

            assert!(!stable.contains("Test project rules."));
            assert!(!dynamic.contains("Test project rules."));

            let path = path.display();
            let expected_instructions = formatdoc! {"
                # User instructions

                Codebase and user instructions are shown below. Be sure to adhere to these instructions.

                Contents of {path} (project instructions):

                Test project rules."
            };
            assert_eq!(parts.seen.instructions, expected_instructions);
            let ctx = &parts.user_context;
            assert!(ctx.contains(&expected_instructions), "{ctx}");
            assert!(ctx.contains("<system-reminder>"));
        }

        #[tokio::test]
        async fn walks_root_to_cwd_for_instruction_discovery() {
            let (home, tmp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let root = tmp.path();
            init_fixture_repo(root);

            let root_path = root.join("CLAUDE.md");
            std::fs::write(&root_path, "Root rules.").unwrap();
            let sub = root.join("crates").join("core");
            std::fs::create_dir_all(&sub).unwrap();
            let sub_path = sub.join("CLAUDE.md");
            std::fs::write(&sub_path, "Subdir rules.").unwrap();
            let model = ModelSpec::resolve("test-model");

            let (parts, warnings) = temp_env::async_with_vars(
                pinned_home(home.path()),
                assemble(&model, &sub, Some(root)),
            )
            .await;
            assert_eq!(warnings, [] as [String; 0]);

            let root_path = root_path.display();
            let sub_path = sub_path.display();
            let expected_instructions = formatdoc! {"
                # User instructions

                Codebase and user instructions are shown below. Be sure to adhere to these instructions.

                Contents of {root_path} (project instructions):

                Root rules.

                Contents of {sub_path} (project instructions):

                Subdir rules."
            };
            assert_eq!(parts.seen.instructions, expected_instructions);
            let ctx = &parts.user_context;
            assert!(ctx.contains(&expected_instructions), "{ctx}");
        }
    }

    mod system_prompt {
        use super::*;

        #[test]
        fn joins_the_stable_sections_with_blank_lines_and_keeps_the_environment_apart() {
            let environment = Environment::default();

            let system = system_prompt(&environment);

            assert_eq!(
                system.stable,
                formatdoc! {"
                    {INTRO}

                    {SYSTEM_SECTION}

                    {TASK_GUIDANCE}

                    {CAUTION}

                    {TOOL_GUIDANCE}

                    {STYLE}

                    {OUTPUT_EFFICIENCY}"
                },
            );
            assert_eq!(system.dynamic, environment.render());
        }
    }

    mod render_user_context {
        use super::*;

        #[test]
        fn with_instructions_includes_every_section() {
            assert_eq!(
                render_user_context(&context("Project rules.", "2026-04-12")),
                indoc! {"
                    <system-reminder>
                    As you answer the user's questions, you can use the following context:

                    Project rules.

                    # Current date

                    Today's date is 2026-04-12.

                    # Model

                    You are powered by the model test-model.

                    IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.
                    </system-reminder>"
                },
            );
        }

        #[test]
        fn without_instructions_still_carries_date() {
            assert_eq!(
                render_user_context(&context("", "2026-04-12")),
                indoc! {"
                    <system-reminder>
                    As you answer the user's questions, you can use the following context:

                    # Current date

                    Today's date is 2026-04-12.

                    # Model

                    You are powered by the model test-model.

                    IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.
                    </system-reminder>"
                },
            );
        }
    }

    mod render_reminder {
        use super::*;

        #[test]
        fn includes_every_changed_section_in_order() {
            assert_eq!(
                render_reminder(
                    &UserContext::default(),
                    &context("Project rules.", "2026-04-12")
                ),
                indoc! {"
                    <system-reminder>
                    The context from the start of this conversation has changed. Use these values from now on:

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
        fn removed_instructions_revoke_earlier_rules() {
            assert_eq!(
                render_reminder(
                    &context("Project rules.", "2026-04-12"),
                    &context("", "2026-04-12")
                ),
                indoc! {"
                    <system-reminder>
                    The context from the start of this conversation has changed. Use these values from now on:

                    The instruction files were removed, so the earlier instructions no longer apply.
                    </system-reminder>"
                },
            );
        }
    }

    mod sections {
        use super::*;

        #[test]
        fn state_their_distinctive_rules() {
            let sections: [(&str, &str, &[&str]); 6] = [
                (
                    "INTRO",
                    INTRO,
                    &[
                        "You run in oxide-code",
                        "A project's config cannot add allow rules",
                        "have no effect in oxide-code",
                    ],
                ),
                (
                    "SYSTEM_SECTION",
                    SYSTEM_SECTION,
                    &[
                        "In `plan`, every `edit`, `write`, and `bash` call is denied",
                        "reads each command of a chain",
                        "needs approval in `manual`, so prefer plain commands",
                    ],
                ),
                (
                    "TASK_GUIDANCE",
                    TASK_GUIDANCE,
                    &[
                        "asks only for a review or a report, \
                         share your findings without editing files",
                        "inspect the actual files, logs, or command output",
                        "Report a task as done only after the checks that prove it",
                        "Create commits only when the user asks.",
                        "without references to the change, the task, or an issue number",
                        "Delete code instead of commenting it out.",
                        "Tests you write must fail against a plausible bug",
                    ],
                ),
                (
                    "CAUTION",
                    CAUTION,
                    &["Never print credentials", "redact any credential"],
                ),
                ("STYLE", STYLE, &["give its full URL"]),
                (
                    "OUTPUT_EFFICIENCY",
                    OUTPUT_EFFICIENCY,
                    &[
                        "task that changed files or ran commands, \
                         close with what you verified and how",
                        "Skip this for questions.",
                    ],
                ),
            ];

            for (name, content, rules) in sections {
                let prose = content.split_whitespace().collect::<Vec<_>>().join(" ");
                for rule in rules {
                    assert!(prose.contains(rule), "{name} should state: {rule}");
                }
            }
        }
    }
}
