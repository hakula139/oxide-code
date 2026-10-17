//! Session task list: the `tasks` tool the model tracks multi-step work with, and the list a
//! transcript leaves behind.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use indoc::formatdoc;
use serde::{Deserialize, Serialize};

use super::{Tool, ToolMetadata, ToolOutput, ToolResultView};
use crate::message::{ContentBlock, Message, Role};
use crate::permission::PermissionClass;
use crate::util::text::plural;

const NAME: &str = "tasks";

// ── Task ──

/// One item on the task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Task {
    pub(crate) content: String,
    pub(crate) status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    /// The name the tool schema uses.
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// Completed tasks out of all tasks on a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Progress {
    pub(crate) completed: usize,
    pub(crate) total: usize,
}

impl Progress {
    /// `None` for an empty list, which has no progress to show.
    pub(crate) fn of(tasks: &[Task]) -> Option<Self> {
        (!tasks.is_empty()).then(|| Self {
            completed: count(tasks, TaskStatus::Completed),
            total: tasks.len(),
        })
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
    fn name(&self) -> &'static str {
        NAME
    }

    fn description(&self) -> &'static str {
        "Maintain the task list for this session, which the user sees while you work. Use it for \
         work that takes three or more steps or when the user asks for several things at once, \
         and skip it for a single straightforward step or a conversational reply. Each call \
         replaces the whole list, so send every task with its current status each time. Mark a \
         task in_progress just before starting it and completed as soon as it is done, with at \
         most one task in_progress at a time. Once every task is completed, the list clears."
    }

    /// Touches only the session's own task list, so it never needs approval.
    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Read
    }

    fn input_schema(&self) -> serde_json::Value {
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
                                "description": "What the task does, in the imperative (e.g. \"Run the tests\")"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    fn icon(&self) -> &'static str {
        "≡"
    }

    fn result_view(
        &self,
        input: &serde_json::Value,
        _content: &str,
        _metadata: &ToolMetadata,
    ) -> Option<ToolResultView> {
        let tasks = requested(input).ok()?;
        Some(ToolResultView::Tasks { tasks })
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(std::future::ready(run(&input)))
    }
}

// ── Input ──

#[derive(Deserialize)]
struct Input {
    tasks: Vec<Task>,
}

/// The list a call with `input` sends, or the message telling the model why it is invalid.
fn requested(input: &serde_json::Value) -> Result<Vec<Task>, String> {
    let Input { tasks } = Input::deserialize(input).map_err(|e| format!("Invalid input: {e}"))?;
    if let Some(index) = tasks.iter().position(|task| task.content.trim().is_empty()) {
        return Err(format!(
            "Task {} has no content. Describe each task in a few words.",
            index + 1,
        ));
    }
    let in_progress = count(&tasks, TaskStatus::InProgress);
    if in_progress > 1 {
        return Err(format!(
            "{in_progress} tasks are in_progress, but at most one can be at a time. Mark the \
             others pending or completed."
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

    let output = match Progress::of(&tasks) {
        None => ToolOutput::success("Cleared the task list.").with_title("Cleared the task list"),
        Some(Progress { completed, total }) if completed == total => {
            ToolOutput::success("Every task is completed, so the task list was cleared.")
                .with_title(format!(
                    "Completed {total} {}",
                    plural(total, "task", "tasks")
                ))
        }
        Some(Progress { completed, total }) => ToolOutput::success(format!(
            "Updated the task list: {completed} of {total} completed."
        ))
        .with_title(format!(
            "{completed} of {total} {} completed",
            plural(total, "task", "tasks"),
        )),
    };
    output.with_tasks(kept(tasks))
}

/// The list the session keeps after a call sends `tasks`. A finished list clears, so it stops
/// showing once the work is done.
fn kept(tasks: Vec<Task>) -> Vec<Task> {
    if tasks
        .iter()
        .all(|task| task.status == TaskStatus::Completed)
    {
        return Vec::new();
    }
    tasks
}

// ── Transcript ──

/// The list the last successful `tasks` call in `messages` left, or `None` when no such call is
/// there, as in a transcript that starts from a compaction summary.
pub(crate) fn latest(messages: &[Message]) -> Option<Vec<Task>> {
    let mut succeeded: HashMap<&str, bool> = HashMap::new();
    for message in messages.iter().rev() {
        for block in message.content.iter().rev() {
            match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } if message.role == Role::User => {
                    succeeded.insert(tool_use_id.as_str(), !is_error);
                }
                ContentBlock::ToolUse { id, name, input }
                    if name == NAME && succeeded.get(id.as_str()) == Some(&true) =>
                {
                    if let Ok(tasks) = requested(input) {
                        return Some(kept(tasks));
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Reminds the model of `tasks` in a transcript that no longer holds the calls that built them,
/// such as one a compaction summary starts. `None` for an empty list.
pub(crate) fn reminder(tasks: &[Task]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let mut list = String::new();
    for task in tasks {
        _ = writeln!(list, "- [{}] {}", task.status.as_str(), task.content);
    }
    Some(formatdoc! {"
        <system-reminder>
        Your task list carries over from before the summary. Keep it current with the tasks tool:

        {list}</system-reminder>"
    })
}

#[cfg(test)]
pub(crate) mod testing {
    use super::{Task, TaskStatus};

    /// A task with `content` at `status`.
    pub(crate) fn task(content: &str, status: TaskStatus) -> Task {
        Task {
            content: content.to_owned(),
            status,
        }
    }

    /// Three tasks: one completed, one in progress, one pending.
    pub(crate) fn plan() -> Vec<Task> {
        vec![
            task("Read the config loader", TaskStatus::Completed),
            task("Add the entry type", TaskStatus::InProgress),
            task("Write the tests", TaskStatus::Pending),
        ]
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use serde_json::json;

    use super::testing::{plan, task};
    use super::*;

    fn input(tasks: &[Task]) -> serde_json::Value {
        json!({ "tasks": tasks })
    }

    fn tasks_call(id: &str, tasks: &[Task]) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: NAME.to_owned(),
                input: input(tasks),
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

    // ── Progress::of ──

    #[test]
    fn progress_of_counts_completed_tasks_out_of_all() {
        assert_eq!(
            Progress::of(&plan()),
            Some(Progress {
                completed: 1,
                total: 3,
            }),
        );
        assert_eq!(Progress::of(&[]), None);
    }

    // ── TasksTool::run ──

    #[tokio::test]
    async fn run_keeps_the_sent_list_and_reports_its_progress() {
        let output = TasksTool.run(input(&plan())).await;

        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "Updated the task list: 1 of 3 completed.");
        assert_eq!(
            output.metadata.title.as_deref(),
            Some("1 of 3 tasks completed")
        );
        assert_eq!(output.tasks, Some(plan()));
    }

    #[tokio::test]
    async fn run_moves_a_task_through_each_status_across_calls() {
        let steps = [
            (
                TaskStatus::Pending,
                "Updated the task list: 0 of 2 completed.",
            ),
            (
                TaskStatus::InProgress,
                "Updated the task list: 0 of 2 completed.",
            ),
            (
                TaskStatus::Completed,
                "Updated the task list: 1 of 2 completed.",
            ),
        ];
        for (status, expected) in steps {
            let tasks = [
                task("Fix the parser", status),
                task("Run the tests", TaskStatus::Pending),
            ];

            let output = TasksTool.run(input(&tasks)).await;

            assert_eq!(output.content, expected, "{status:?}");
            assert_eq!(output.tasks, Some(tasks.to_vec()), "{status:?}");
        }
    }

    #[tokio::test]
    async fn run_clears_a_list_once_every_task_is_completed() {
        let done = [
            task("Fix the parser", TaskStatus::Completed),
            task("Run the tests", TaskStatus::Completed),
        ];

        let output = TasksTool.run(input(&done)).await;

        assert_eq!(
            output.content,
            "Every task is completed, so the task list was cleared."
        );
        assert_eq!(output.metadata.title.as_deref(), Some("Completed 2 tasks"));
        assert_eq!(output.tasks, Some(Vec::new()));
    }

    #[tokio::test]
    async fn run_clears_the_list_for_an_empty_one() {
        let output = TasksTool.run(input(&[])).await;

        assert_eq!(output.content, "Cleared the task list.");
        assert_eq!(output.tasks, Some(Vec::new()));
    }

    #[tokio::test]
    async fn run_rejects_more_than_one_task_in_progress() {
        let tasks = [
            task("Fix the parser", TaskStatus::InProgress),
            task("Run the tests", TaskStatus::InProgress),
        ];

        let output = TasksTool.run(input(&tasks)).await;

        assert!(output.is_error);
        assert_eq!(
            output.content,
            "2 tasks are in_progress, but at most one can be at a time. Mark the others pending \
             or completed."
        );
        assert_eq!(output.tasks, None);
    }

    #[tokio::test]
    async fn run_rejects_a_task_without_content() {
        let tasks = [
            task("Fix the parser", TaskStatus::Pending),
            task("  ", TaskStatus::Pending),
        ];

        let output = TasksTool.run(input(&tasks)).await;

        assert!(output.is_error);
        assert_eq!(
            output.content,
            "Task 2 has no content. Describe each task in a few words."
        );
        assert_eq!(output.tasks, None);
    }

    #[tokio::test]
    async fn run_rejects_an_unknown_status() {
        let output = TasksTool
            .run(json!({"tasks": [{"content": "Fix the parser", "status": "blocked"}]}))
            .await;

        assert!(output.is_error);
        assert_eq!(
            output.content,
            "Invalid input: unknown variant `blocked`, expected one of `pending`, `in_progress`, `completed`",
        );
        assert_eq!(output.tasks, None);
    }

    // ── TasksTool::result_view ──

    #[test]
    fn result_view_shows_the_list_as_sent_even_when_it_cleared() {
        let done = [task("Fix the parser", TaskStatus::Completed)];

        let view = TasksTool.result_view(&input(&done), "", &ToolMetadata::default());

        assert_eq!(
            view,
            Some(ToolResultView::Tasks {
                tasks: done.to_vec(),
            }),
        );
    }

    // ── latest ──

    #[test]
    fn latest_takes_the_last_successful_call() {
        let earlier = [task("Fix the parser", TaskStatus::Pending)];
        let messages = [
            tasks_call("t1", &earlier),
            result("t1", false),
            tasks_call("t2", &plan()),
            result("t2", false),
            tasks_call("t3", &[task("Rejected", TaskStatus::Pending)]),
            result("t3", true),
        ];

        assert_eq!(latest(&messages), Some(plan()));
    }

    #[test]
    fn latest_skips_a_call_without_a_result() {
        let earlier = plan();
        let messages = [
            tasks_call("t1", &earlier),
            result("t1", false),
            tasks_call("t2", &[task("Unanswered", TaskStatus::Pending)]),
        ];

        assert_eq!(latest(&messages), Some(earlier));
    }

    #[test]
    fn latest_skips_a_successful_call_whose_input_does_not_parse() {
        let unparsable = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t2".to_owned(),
                name: NAME.to_owned(),
                input: json!({"tasks": [{"content": "Fix the parser", "status": "blocked"}]}),
            }],
        };
        let messages = [
            tasks_call("t1", &plan()),
            result("t1", false),
            unparsable,
            result("t2", false),
        ];

        assert_eq!(latest(&messages), Some(plan()));
    }

    #[test]
    fn latest_reports_a_cleared_list_as_empty() {
        let done = [task("Fix the parser", TaskStatus::Completed)];
        let messages = [
            tasks_call("t1", &plan()),
            result("t1", false),
            tasks_call("t2", &done),
            result("t2", false),
        ];

        assert_eq!(latest(&messages), Some(Vec::new()));
    }

    #[test]
    fn latest_is_none_without_a_call() {
        let messages = [Message::user("hi"), Message::assistant("hello")];

        assert_eq!(latest(&messages), None);
    }

    // ── reminder ──

    #[test]
    fn reminder_lists_each_task_with_its_status() {
        assert_eq!(
            reminder(&plan()).as_deref(),
            Some(indoc! {"
                <system-reminder>
                Your task list carries over from before the summary. Keep it current with the tasks tool:

                - [completed] Read the config loader
                - [in_progress] Add the entry type
                - [pending] Write the tests
                </system-reminder>"
            }),
        );
        assert_eq!(reminder(&[]), None);
    }
}
