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
pub(crate) fn synthesize_post_compact_message(
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

/// Restores provenance for the synthetic root an older compact boundary identifies. Explicit
/// origins survive unchanged, and the model-facing text is never rewritten.
pub(super) fn restore_post_compact_origin(message: &mut Message) {
    if message.role != Role::User {
        return;
    }
    let [ContentBlock::Text { origin, .. }, reminders @ ..] = &mut message.content[..] else {
        return;
    };
    origin.get_or_insert(TextOrigin::CompactionSummary);
    for block in reminders {
        if let ContentBlock::Text { origin, .. } = block {
            origin.get_or_insert(TextOrigin::SystemReminder);
        }
    }
}

/// Removes the generated summary block that the compaction boundary already displays.
pub(super) fn strip_post_compact_summary(message: &mut Message) -> bool {
    if message.role != Role::User
        || !matches!(
            message.content.first(),
            Some(ContentBlock::Text {
                origin: Some(TextOrigin::CompactionSummary),
                ..
            })
        )
    {
        return false;
    }
    message.content.remove(0);
    true
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::tool::bash::background::testing::shells_in_each_state;
    use crate::tool::tasks::testing::plan;

    mod synthesize_post_compact_message {
        use super::*;

        #[test]
        fn prepends_summary_prefix() {
            let m =
                synthesize_post_compact_message(" did X, Y, Z\n", &[], &[], CompactTrigger::Manual);

            assert_eq!(m.role, Role::User);
            let [ContentBlock::Text { text, .. }] = &m.content[..] else {
                panic!("expected one text block: {m:?}");
            };
            assert_eq!(
                text,
                indoc! {"
                    This conversation has been compacted. The summary below covers the prior work. Continue from here without re-asking the engineer what to do.

                    did X, Y, Z
                "},
            );
        }

        #[test]
        fn reminds_the_model_of_the_task_list() {
            let m = synthesize_post_compact_message("did X", &plan(), &[], CompactTrigger::Manual);

            let [
                ContentBlock::Text { text: summary, .. },
                ContentBlock::Text { text: reminder, .. },
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
            let m = synthesize_post_compact_message(
                "did X",
                &plan(),
                &shells_in_each_state(),
                CompactTrigger::Manual,
            );

            let [
                _,
                ContentBlock::Text { text: tasks, .. },
                ContentBlock::Text { text: shells, .. },
            ] = &m.content[..]
            else {
                panic!("expected the summary and two reminders: {m:?}");
            };
            assert!(tasks.contains("Your task list carries over"), "{tasks:?}");
            assert_eq!(
                shells,
                indoc! {"
                    <system-reminder>
                    These background shells are still open from before the summary. Read them with bash_output, and stop the ones you no longer need with bash_stop:

                    - shell-1 (running): cargo watch
                    - shell-2 (exit code 2): make test
                    - shell-3 (killed by signal 15): sleep 60
                    </system-reminder>"
                },
            );
        }

        #[test]
        fn tells_the_model_to_resume_only_after_a_mid_turn_compaction() {
            let between_prompts = [CompactTrigger::Manual, CompactTrigger::Auto]
                .map(|trigger| synthesize_post_compact_message("did X", &[], &[], trigger));

            let mid_turn =
                synthesize_post_compact_message("did X", &[], &[], CompactTrigger::MidTurn);

            for m in between_prompts {
                assert!(
                    matches!(&m.content[..], [ContentBlock::Text { .. }]),
                    "{m:?}"
                );
            }
            let [_, ContentBlock::Text { text, .. }] = &mid_turn.content[..] else {
                panic!("expected the summary and the continuation: {mid_turn:?}");
            };
            assert_eq!(
                text,
                indoc! {"
                    <system-reminder>
                    You were partway through your turn when the conversation was compacted. Resume the task directly: do not acknowledge the summary, recap what was happening, or announce that you are continuing.
                    </system-reminder>"
                },
            );
        }
    }

    mod restore_post_compact_origin {
        use super::*;

        #[test]
        fn preserves_explicit_origins_and_leaves_other_root_shapes_unchanged() {
            let mut mixed = Message {
                role: Role::User,
                content: vec![
                    ContentBlock::generated_text("explicit", TextOrigin::Interruption),
                    ContentBlock::generated_text("kept", TextOrigin::CompactionTruncation),
                    ContentBlock::tool_result("t1", "tool", false),
                    ContentBlock::text("legacy reminder"),
                ],
            };

            restore_post_compact_origin(&mut mixed);

            assert_eq!(
                mixed.content,
                [
                    ContentBlock::generated_text("explicit", TextOrigin::Interruption),
                    ContentBlock::generated_text("kept", TextOrigin::CompactionTruncation),
                    ContentBlock::tool_result("t1", "tool", false),
                    ContentBlock::generated_text("legacy reminder", TextOrigin::SystemReminder),
                ]
            );

            for mut message in [
                Message::assistant("assistant text"),
                Message {
                    role: Role::User,
                    content: Vec::new(),
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::tool_result("t1", "tool", false)],
                },
            ] {
                let expected = message.clone();

                restore_post_compact_origin(&mut message);

                assert_eq!(message, expected);
            }
        }
    }

    mod strip_post_compact_summary {
        use super::*;

        #[test]
        fn preserves_a_literal_summary_prefix() {
            let mut message = Message::user(SUMMARY_PREFIX);
            let expected = message.clone();

            let stripped = strip_post_compact_summary(&mut message);

            assert!(!stripped);
            assert_eq!(message, expected);
        }

        #[test]
        fn removes_only_the_prefix_block() {
            let mut m = synthesize_post_compact_message(
                "internal summary",
                &[],
                &[],
                CompactTrigger::Manual,
            );
            restore_post_compact_origin(&mut m);
            m.content.push(ContentBlock::text("real prompt"));

            assert!(strip_post_compact_summary(&mut m));
            assert_eq!(m, Message::user("real prompt"));
        }

        #[test]
        fn removes_the_prefix_as_earlier_releases_wrapped_it() {
            let mut m = Message::user(indoc! {"
                This conversation has been compacted. The summary below covers the prior work. Continue from here
                without re-asking the engineer what to do.

                internal summary"
            });
            restore_post_compact_origin(&mut m);
            m.content.push(ContentBlock::text("real prompt"));

            assert!(strip_post_compact_summary(&mut m));
            assert_eq!(m, Message::user("real prompt"));
        }

        #[test]
        fn leaves_regular_user_text() {
            let mut m = Message::user("This conversation should stay visible");

            assert!(!strip_post_compact_summary(&mut m));
            assert_eq!(m.content.len(), 1);
        }

        #[test]
        fn leaves_assistant_messages() {
            let mut m = Message::assistant(formatdoc! {"
                {SUMMARY_PREFIX}

                not a synthetic user root"
            });

            assert!(!strip_post_compact_summary(&mut m));
            assert_eq!(m.role, Role::Assistant);
            assert_eq!(m.content.len(), 1);
        }

        #[test]
        fn leaves_non_text_first_block() {
            let mut m = Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("toolu_1", SUMMARY_PREFIX, false)],
            };

            assert!(!strip_post_compact_summary(&mut m));
            assert!(matches!(m.content[0], ContentBlock::ToolResult { .. }));
        }
    }
}
