//! Session task list: the `tasks` tool the model tracks multi-step work with, and the list a
//! transcript leaves behind.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use strum::{Display, VariantArray};

use super::view::ToolResultView;
use super::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec, invalid_input};
use crate::message::{ContentBlock, Message};
use crate::permission::PermissionClass;
use crate::util::text::plural;

const NAME: &str = "tasks";

// ── Task ──

/// One item on the task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub content: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// Completed tasks out of all tasks on a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub completed: usize,
    pub total: usize,
}

impl Progress {
    /// `None` for an empty list, which has no progress to show.
    pub fn of(tasks: &[Task]) -> Option<Self> {
        (!tasks.is_empty()).then(|| Self {
            completed: count(tasks, TaskStatus::Completed),
            total: tasks.len(),
        })
    }
}

/// Shows the progress as `completed/total`, as in `1/3`.
impl fmt::Display for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.completed, self.total)
    }
}

fn count(tasks: &[Task], status: TaskStatus) -> usize {
    tasks.iter().filter(|task| task.status == status).count()
}

// ── Tasks Tool ──

/// Replaces the session's task list with the one the model sends. The engine adopts the list
/// [`ToolOutput::tasks`] carries.
pub(crate) struct TasksTool;

impl Tool for TasksTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: NAME.to_owned(),
                description: "Maintain the task list for this session, \
                              which the user sees while you work. \
                              Use it for work that takes three or more steps \
                              or when the user asks for several things at once, \
                              and skip it for a single straightforward step \
                              or a conversational reply. \
                              Each call replaces the whole list, \
                              so send every task with its current status each time. \
                              Mark a task in_progress just before starting it, \
                              with at most one in_progress at a time, \
                              and completed only once it is fully done. \
                              Remove tasks that no longer apply, \
                              and send an empty list when the user changes course. \
                              Once every task is completed, the list clears. \
                              The user already sees the list, so don't repeat it in your reply."
                    .to_owned(),
                input_schema: input_schema(),
            },
            // It touches only the session's own task list, so it never needs approval.
            class: PermissionClass::Read,
            parallel_safe: false,
        }
    }

    fn icon(&self) -> &'static str {
        "≡"
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        _content: &str,
        _cwd: &Path,
    ) -> Option<ToolResultView> {
        let tasks = requested(input).ok()?;
        Some(ToolResultView::Tasks { tasks })
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(std::future::ready(run(&prepared.input)))
    }
}

// ── Input ──

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "description": "The complete task list in order. An empty list clears it.",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The task as a short imperative phrase, \
                                            such as \"Run the tests\""
                        },
                        "status": {
                            "type": "string",
                            "enum": TaskStatus::VARIANTS
                        }
                    },
                    "required": ["content", "status"]
                }
            }
        },
        "required": ["tasks"]
    })
}

#[derive(Deserialize)]
struct Input {
    tasks: Vec<Task>,
}

/// The tasks `input` lists, each with its content collapsed onto one line.
fn parse(input: &serde_json::Value) -> Result<Vec<Task>, serde_json::Error> {
    let Input { mut tasks } = Input::deserialize(input)?;
    for task in &mut tasks {
        task.content = task
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
    }
    Ok(tasks)
}

/// The list a call with `input` sends, or the message telling the model why it is invalid.
fn requested(input: &serde_json::Value) -> Result<Vec<Task>, String> {
    let tasks = parse(input).map_err(|e| invalid_input(&e))?;
    if let Some(index) = tasks.iter().position(|task| task.content.is_empty()) {
        return Err(format!(
            "Task {} has no content. Describe each task in a few words.",
            index + 1,
        ));
    }
    let in_progress = count(&tasks, TaskStatus::InProgress);
    if in_progress > 1 {
        return Err(format!(
            "{in_progress} tasks are in_progress, but at most one can be at a time. \
             Mark the others pending or completed."
        ));
    }
    Ok(tasks)
}

// ── Execution ──

fn run(input: &serde_json::Value) -> ToolOutput {
    let tasks = match requested(input) {
        Ok(tasks) => tasks,
        Err(message) => return ToolOutput::error(message),
    };

    match Progress::of(&tasks) {
        None => ToolOutput::success("Cleared the task list.")
            .with_title("Cleared the task list")
            .with_tasks(tasks),
        Some(Progress { completed, total }) if completed == total => {
            ToolOutput::success("Every task is completed, so the task list was cleared.")
                .with_title(format!(
                    "Completed {total} {}",
                    plural(total, "task", "tasks")
                ))
                .with_tasks(Vec::new())
        }
        Some(Progress { completed, total }) => ToolOutput::success(format!(
            "Updated the task list: {completed} of {total} completed. \
             Carry on with the remaining tasks."
        ))
        .with_title(format!(
            "Updated {total} {}, {completed} completed",
            plural(total, "task", "tasks"),
        ))
        .with_tasks(tasks),
    }
}

// ── Transcript ──

/// The list the last successful `tasks` call in `messages` left, or `None` when no such call is
/// there, as in a transcript that starts from a compaction summary.
pub(crate) fn latest(messages: &[Message]) -> Option<Vec<Task>> {
    let mut succeeded: HashMap<&str, bool> = HashMap::new();
    let blocks = messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev());
    for block in blocks {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                succeeded.insert(tool_use_id.as_str(), !is_error);
            }
            ContentBlock::ToolUse { id, name, input }
                if name == NAME && succeeded.get(id.as_str()) == Some(&true) =>
            {
                if let Some(tasks) = run(input).tasks {
                    return Some(tasks);
                }
            }
            _ => {}
        }
    }
    None
}

// ── Testing ──

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::{Task, TaskStatus};

    /// A task with `content` at `status`.
    pub fn task(content: &str, status: TaskStatus) -> Task {
        Task {
            content: content.to_owned(),
            status,
        }
    }

    /// Three tasks: one completed, one in progress, one pending.
    pub fn plan() -> Vec<Task> {
        vec![
            task("Read the config loader", TaskStatus::Completed),
            task("Add the entry type", TaskStatus::InProgress),
            task("Write the tests", TaskStatus::Pending),
        ]
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testing::{plan, task};
    use super::*;
    use crate::message::Role;
    use crate::tool::local_testing::{self, temp_ctx};

    fn input(tasks: &[Task]) -> serde_json::Value {
        json!({ "tasks": tasks })
    }

    mod task_status {
        use super::*;

        #[test]
        fn names_each_status_the_way_serde_does() {
            for &status in TaskStatus::VARIANTS {
                assert_eq!(serde_json::to_value(status).unwrap(), status.to_string());
            }
        }
    }

    mod progress {
        use super::*;

        #[test]
        fn displays_completed_out_of_total() {
            let progress = Progress {
                completed: 1,
                total: 3,
            };

            assert_eq!(progress.to_string(), "1/3");
        }

        mod of {
            use super::*;

            #[test]
            fn counts_completed_tasks_out_of_all() {
                assert_eq!(
                    Progress::of(&plan()),
                    Some(Progress {
                        completed: 1,
                        total: 3,
                    }),
                );
                assert_eq!(Progress::of(&[]), None);
            }
        }
    }

    mod result_view {
        use super::*;

        #[test]
        fn shows_the_list_as_sent_even_when_it_cleared() {
            let done = [task("Fix the parser", TaskStatus::Completed)];

            let view = TasksTool.result_view(&input(&done), "", Path::new("/repo"));

            assert_eq!(
                view,
                Some(ToolResultView::Tasks {
                    tasks: done.to_vec(),
                }),
            );
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn keeps_the_sent_list_and_reports_its_progress() {
            let output = local_testing::run(&TasksTool, input(&plan()), &temp_ctx()).await;

            assert!(!output.is_error, "{}", output.content);
            assert_eq!(
                output.content,
                "Updated the task list: 1 of 3 completed. Carry on with the remaining tasks."
            );
            assert_eq!(
                output.metadata.title.as_deref(),
                Some("Updated 3 tasks, 1 completed")
            );
            assert_eq!(output.tasks, Some(plan()));
        }

        #[tokio::test]
        async fn collapses_each_task_onto_one_line() {
            let sent = [task(
                "  Fix the parser\nthen   rerun\tthe tests ",
                TaskStatus::Pending,
            )];

            let output = local_testing::run(&TasksTool, input(&sent), &temp_ctx()).await;

            assert_eq!(
                output.tasks,
                Some(vec![task(
                    "Fix the parser then rerun the tests",
                    TaskStatus::Pending
                )]),
            );
        }

        #[tokio::test]
        async fn clears_a_list_once_every_task_is_completed() {
            let done = [
                task("Fix the parser", TaskStatus::Completed),
                task("Run the tests", TaskStatus::Completed),
            ];

            let output = local_testing::run(&TasksTool, input(&done), &temp_ctx()).await;

            assert_eq!(
                output.content,
                "Every task is completed, so the task list was cleared."
            );
            assert_eq!(output.metadata.title.as_deref(), Some("Completed 2 tasks"));
            assert_eq!(output.tasks, Some(Vec::new()));
        }

        #[tokio::test]
        async fn clears_the_list_for_an_empty_one() {
            let output = local_testing::run(&TasksTool, input(&[]), &temp_ctx()).await;

            assert_eq!(output.content, "Cleared the task list.");
            assert_eq!(output.tasks, Some(Vec::new()));
        }

        #[tokio::test]
        async fn rejects_more_than_one_task_in_progress() {
            let tasks = [
                task("Fix the parser", TaskStatus::InProgress),
                task("Run the tests", TaskStatus::InProgress),
            ];

            let output = local_testing::run(&TasksTool, input(&tasks), &temp_ctx()).await;

            assert!(output.is_error);
            assert_eq!(
                output.content,
                "2 tasks are in_progress, but at most one can be at a time. \
                 Mark the others pending or completed."
            );
            assert_eq!(output.tasks, None);
        }

        #[tokio::test]
        async fn rejects_a_task_without_content() {
            let tasks = [
                task("Fix the parser", TaskStatus::Pending),
                task(" \n ", TaskStatus::Pending),
            ];

            let output = local_testing::run(&TasksTool, input(&tasks), &temp_ctx()).await;

            assert!(output.is_error);
            assert_eq!(
                output.content,
                "Task 2 has no content. Describe each task in a few words."
            );
            assert_eq!(output.tasks, None);
        }

        #[tokio::test]
        async fn rejects_an_unknown_status() {
            let raw = json!({"tasks": [{"content": "Fix the parser", "status": "blocked"}]});

            let output = local_testing::run(&TasksTool, raw, &temp_ctx()).await;

            assert!(output.is_error);
            assert_eq!(
                output.content,
                "Invalid input: unknown variant `blocked`, \
                 expected one of `pending`, `in_progress`, `completed`",
            );
            assert_eq!(output.tasks, None);
        }
    }

    mod latest {
        use super::*;

        fn tasks_call(id: &str, input: serde_json::Value) -> Message {
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.to_owned(),
                    name: NAME.to_owned(),
                    input,
                }],
            }
        }

        fn result(id: &str, is_error: bool) -> Message {
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: "ok".to_owned(),
                    is_error,
                }],
            }
        }

        #[test]
        fn takes_the_last_successful_call() {
            let earlier = [task("Fix the parser", TaskStatus::Pending)];
            let messages = [
                tasks_call("t1", input(&earlier)),
                result("t1", false),
                tasks_call("t2", input(&plan())),
                result("t2", false),
                tasks_call("t3", input(&[task("Rejected", TaskStatus::Pending)])),
                result("t3", true),
            ];

            assert_eq!(latest(&messages), Some(plan()));
        }

        #[test]
        fn skips_a_call_without_a_result() {
            let messages = [
                tasks_call("t1", input(&plan())),
                result("t1", false),
                tasks_call("t2", input(&[task("Unanswered", TaskStatus::Pending)])),
            ];

            assert_eq!(latest(&messages), Some(plan()));
        }

        #[test]
        fn skips_a_successful_call_whose_input_does_not_parse() {
            let unknown_status =
                json!({"tasks": [{"content": "Fix the parser", "status": "blocked"}]});
            let messages = [
                tasks_call("t1", input(&plan())),
                result("t1", false),
                tasks_call("t2", unknown_status),
                result("t2", false),
            ];

            assert_eq!(latest(&messages), Some(plan()));
        }

        #[test]
        fn reports_a_cleared_list_as_empty() {
            let done = [task("Fix the parser", TaskStatus::Completed)];
            let messages = [
                tasks_call("t1", input(&plan())),
                result("t1", false),
                tasks_call("t2", input(&done)),
                result("t2", false),
            ];

            assert_eq!(latest(&messages), Some(Vec::new()));
        }

        #[test]
        fn finds_no_list_without_a_call() {
            let messages = [Message::user("hi"), Message::assistant("hello")];

            assert_eq!(latest(&messages), None);
        }
    }
}
