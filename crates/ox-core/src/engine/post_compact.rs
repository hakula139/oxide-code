//! The synthetic user message that opens a compacted transcript. Compaction writes it with the
//! summary as its first block, and the resumed display strips that block again, since the boundary
//! block shows the summary.

use indoc::formatdoc;

use crate::message::{ContentBlock, Message, Role, TextOrigin};
use crate::prompt::system_reminder;
use crate::protocol::CompactTrigger;
use crate::tool::bash::background::BackgroundShell;
use crate::tool::tasks::Task;

/// Prefix for the synthetic post-compact user message.
const SUMMARY_PREFIX: &str = "This conversation has been compacted. \
                              The summary below covers the prior work. \
                              Continue from here without re-asking the engineer what to do.";

/// Composes the synthetic post-compact root message, reminding the model of the task list and the
/// open background shells the summarized transcript leaves. After a [`CompactTrigger::MidTurn`]
/// compaction it also tells the model to resume its turn.
pub(super) fn synthesize_post_compact_message(
    summary: &str,
    tasks: &[Task],
    shells: &[BackgroundShell],
    trigger: CompactTrigger,
) -> Message {
    let summary = summary.trim();
    let mut message = Message {
        role: Role::User,
        content: vec![ContentBlock::generated_text(
            formatdoc! {"
                {SUMMARY_PREFIX}

                {summary}
            "},
            TextOrigin::CompactionSummary,
        )],
    };
    let continuation = (trigger == CompactTrigger::MidTurn).then(|| {
        system_reminder(
            "You were partway through your turn when the conversation was compacted. \
             Resume the task directly: do not acknowledge the summary, recap what was happening, \
             or announce that you are continuing.",
        )
    });
    message.content.extend(
        [task_reminder(tasks), shell_reminder(shells), continuation]
            .into_iter()
            .flatten()
            .map(|text| ContentBlock::generated_text(text, TextOrigin::SystemReminder)),
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
        .map(|task| format!("- [{}] {}", task.status, task.content))
        .collect::<Vec<_>>()
        .join("\n");
    Some(system_reminder(&formatdoc! {"
        Your task list carries over from before the summary. Keep it current with the tasks tool:

        {list}"
    }))
}

/// The block that lists the open background shells after the summary, since the calls that started
/// them are gone. `None` when none are open.
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
        These background shells are still open from before the summary. Read them with bash_output, and stop the ones you no longer need with bash_stop:

        {list}"
    }))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::tool::bash::background::testing::shells_in_each_state;
    use crate::tool::tasks::testing::plan;

    mod synthesize_post_compact_message {
        use super::*;

        const TASK_REMINDER: &str = indoc! {"
            <system-reminder>
            Your task list carries over from before the summary. Keep it current with the tasks tool:

            - [completed] Read the config loader
            - [in_progress] Add the entry type
            - [pending] Write the tests
            </system-reminder>"
        };

        fn reminder(text: &str) -> ContentBlock {
            ContentBlock::generated_text(text, TextOrigin::SystemReminder)
        }

        #[test]
        fn prepends_summary_prefix() {
            let m =
                synthesize_post_compact_message(" did X, Y, Z\n", &[], &[], CompactTrigger::Manual);

            assert_eq!(m.role, Role::User);
            assert_eq!(
                m.content,
                [ContentBlock::generated_text(
                    indoc! {"
                        This conversation has been compacted. The summary below covers the prior work. Continue from here without re-asking the engineer what to do.

                        did X, Y, Z
                    "},
                    TextOrigin::CompactionSummary,
                )],
            );
        }

        #[test]
        fn reminds_the_model_of_the_task_list() {
            let m = synthesize_post_compact_message("did X", &plan(), &[], CompactTrigger::Manual);

            assert_eq!(m.content[1..], [reminder(TASK_REMINDER)], "{m:?}");
        }

        #[test]
        fn lists_the_open_background_shells_after_the_task_list() {
            let m = synthesize_post_compact_message(
                "did X",
                &plan(),
                &shells_in_each_state(),
                CompactTrigger::Manual,
            );

            assert_eq!(
                m.content[1..],
                [
                    reminder(TASK_REMINDER),
                    reminder(indoc! {"
                        <system-reminder>
                        These background shells are still open from before the summary. Read them with bash_output, and stop the ones you no longer need with bash_stop:

                        - shell-1 (running): cargo watch
                        - shell-2 (exit code 2): make test
                        - shell-3 (killed by signal 15): sleep 60
                        </system-reminder>"
                    }),
                ],
                "{m:?}",
            );
        }

        #[test]
        fn tells_the_model_to_resume_only_after_a_mid_turn_compaction() {
            let between_prompts = [CompactTrigger::Manual, CompactTrigger::Auto]
                .map(|trigger| synthesize_post_compact_message("did X", &[], &[], trigger));

            let mid_turn =
                synthesize_post_compact_message("did X", &[], &[], CompactTrigger::MidTurn);

            for m in between_prompts {
                assert_eq!(m.content, mid_turn.content[..1], "{m:?}");
            }
            assert_eq!(
                mid_turn.content[1..],
                [reminder(indoc! {"
                    <system-reminder>
                    You were partway through your turn when the conversation was compacted. Resume the task directly: do not acknowledge the summary, recap what was happening, or announce that you are continuing.
                    </system-reminder>"
                })],
                "{mid_turn:?}",
            );
        }
    }
}
