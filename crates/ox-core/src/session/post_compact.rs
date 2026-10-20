//! The synthetic user message that opens a compacted transcript. Compaction writes it with the
//! summary as its first block, and the resumed display strips that block again, since the
//! boundary block shows the summary.

use indoc::{formatdoc, indoc};

use crate::message::{ContentBlock, Message, Role};
use crate::prompt::system_reminder;
use crate::tool::bash::background::BackgroundShell;
use crate::tool::tasks::Task;

/// Prefix for the synthetic post-compact user message.
const SUMMARY_PREFIX: &str = indoc! {"
    This conversation has been compacted. The summary below covers the prior work. Continue from here
    without re-asking the engineer what to do.
"};

/// Composes the synthetic post-compact root message for the next turn, reminding the model of
/// the task list and the open background shells the summarized transcript leaves.
pub(crate) fn synthesize_post_compact_message(
    summary: &str,
    tasks: &[Task],
    shells: &[BackgroundShell],
) -> Message {
    let prefix = SUMMARY_PREFIX.trim();
    let summary = summary.trim();
    let mut message = Message::user(formatdoc! {"
        {prefix}

        {summary}
    "});
    message.content.extend(
        [task_reminder(tasks), shell_reminder(shells)]
            .into_iter()
            .flatten()
            .map(|text| ContentBlock::Text { text }),
    );
    message
}

/// The block that lists `tasks` after the summary, since the calls that built the list are gone.
/// `None` for an empty list.
fn task_reminder(tasks: &[Task]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let list = tasks
        .iter()
        .map(|task| format!("- [{}] {}", task.status.as_str(), task.content))
        .collect::<Vec<_>>()
        .join("\n");
    Some(system_reminder(&formatdoc! {"
        Your task list carries over from before the summary. Keep it current with the tasks tool:

        {list}"
    }))
}

/// The block that lists the open background shells after the summary, since the calls that
/// started them are gone. `None` when none are open.
fn shell_reminder(shells: &[BackgroundShell]) -> Option<String> {
    if shells.is_empty() {
        return None;
    }
    let list = shells
        .iter()
        .map(|shell| {
            format!(
                "- {} ({}): {}",
                shell.id,
                shell.state.describe(),
                shell.command_preview()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(system_reminder(&formatdoc! {"
        These background shells are still open from before the summary. Read them with bash_output,
        and stop the ones you no longer need with bash_stop:

        {list}"
    }))
}

/// Removes the synthetic summary block from a resumed post-compact root message, returning whether
/// it was there.
pub(crate) fn strip_synthetic_post_compact_prefix(message: &mut Message) -> bool {
    if message.role != Role::User {
        return false;
    }
    let Some(ContentBlock::Text { text }) = message.content.first() else {
        return false;
    };
    if !text.trim_start().starts_with(SUMMARY_PREFIX.trim()) {
        return false;
    }

    message.content.remove(0);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::bash::background::testing::shells_in_each_state;
    use crate::tool::tasks::testing::plan;

    mod synthesize_post_compact_message {
        use super::*;

        #[test]
        fn prepends_summary_prefix() {
            let m = synthesize_post_compact_message("did X, Y, Z", &[], &[]);
            assert_eq!(m.role, Role::User);
            let [ContentBlock::Text { text }] = &m.content[..] else {
                panic!("expected one text block: {m:?}");
            };
            assert!(text.contains("This conversation has been compacted"));
            assert!(text.contains("did X, Y, Z"));
        }

        #[test]
        fn reminds_the_model_of_the_task_list() {
            let m = synthesize_post_compact_message("did X", &plan(), &[]);

            let [
                ContentBlock::Text { text: summary },
                ContentBlock::Text { text: reminder },
            ] = &m.content[..]
            else {
                panic!("expected the summary and a reminder: {m:?}");
            };
            assert!(summary.ends_with("did X\n"), "{summary:?}");
            assert_eq!(
                reminder,
                indoc! {"
                    <system-reminder>
                    Your task list carries over from before the summary. Keep it current with the tasks tool:

                    - [completed] Read the config loader
                    - [in_progress] Add the entry type
                    - [pending] Write the tests
                    </system-reminder>"
                },
            );
        }

        #[test]
        fn lists_the_open_background_shells_after_the_task_list() {
            let m = synthesize_post_compact_message("did X", &plan(), &shells_in_each_state());

            let [
                _,
                ContentBlock::Text { text: tasks },
                ContentBlock::Text { text: shells },
            ] = &m.content[..]
            else {
                panic!("expected the summary and two reminders: {m:?}");
            };
            assert!(tasks.contains("Your task list carries over"), "{tasks:?}");
            assert_eq!(
                shells,
                indoc! {"
                    <system-reminder>
                    These background shells are still open from before the summary. Read them with bash_output,
                    and stop the ones you no longer need with bash_stop:

                    - shell-1 (running): cargo watch
                    - shell-2 (exit code 2): make test
                    - shell-3 (killed by signal 15): sleep 60
                    </system-reminder>"
                },
            );
        }
    }

    mod strip_synthetic_post_compact_prefix {
        use super::*;

        #[test]
        fn removes_only_the_prefix_block() {
            let mut m = synthesize_post_compact_message("internal summary", &[], &[]);
            m.content.push(ContentBlock::Text {
                text: "real prompt".to_owned(),
            });

            assert!(strip_synthetic_post_compact_prefix(&mut m));
            assert_eq!(m.content.len(), 1);
            assert!(matches!(&m.content[0], ContentBlock::Text { text } if text == "real prompt"));
        }

        #[test]
        fn leaves_regular_user_text() {
            let mut m = Message::user("This conversation should stay visible");

            assert!(!strip_synthetic_post_compact_prefix(&mut m));
            assert_eq!(m.content.len(), 1);
        }

        #[test]
        fn leaves_assistant_messages() {
            let prefix = SUMMARY_PREFIX.trim();
            let mut m = Message::assistant(formatdoc! {"
                {prefix}

                not a synthetic user root"
            });

            assert!(!strip_synthetic_post_compact_prefix(&mut m));
            assert_eq!(m.role, Role::Assistant);
            assert_eq!(m.content.len(), 1);
        }

        #[test]
        fn leaves_non_text_first_block() {
            let mut m = Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".to_owned(),
                    content: SUMMARY_PREFIX.trim().to_owned(),
                    is_error: false,
                }],
            };

            assert!(!strip_synthetic_post_compact_prefix(&mut m));
            assert!(matches!(m.content[0], ContentBlock::ToolResult { .. }));
        }
    }
}
