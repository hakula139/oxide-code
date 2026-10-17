//! The synthetic user message that opens a compacted transcript. Compaction writes it with the
//! summary, and the resumed display strips its prefix again, since the boundary block shows it.

use std::fmt::Write as _;

use indoc::{formatdoc, indoc};

use crate::message::{ContentBlock, Message, Role};
use crate::tool::tasks::Task;

/// Prefix for the synthetic post-compact user message.
const SUMMARY_PREFIX: &str = indoc! {r"
    This conversation has been compacted. The summary below covers the prior work. Continue from here
    without re-asking the engineer what to do.
"};

/// Composes the synthetic post-compact root message for the next turn, reminding the model of
/// the task list the summarized transcript leaves.
pub(crate) fn synthesize_post_compact_message(summary: &str, tasks: &[Task]) -> Message {
    let mut message = Message::user(formatdoc! {"
        {prefix}

        {summary}
    ", prefix = SUMMARY_PREFIX.trim(), summary = summary.trim()});
    message
        .content
        .extend(task_reminder(tasks).map(|text| ContentBlock::Text { text }));
    message
}

/// The block that lists `tasks` after the summary, since the calls that built the list are gone.
/// `None` for an empty list.
fn task_reminder(tasks: &[Task]) -> Option<String> {
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

/// Removes the synthetic summary prefix from a resumed post-compact root message.
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
    use crate::tool::tasks::testing::plan;

    // ── synthesize_post_compact_message ──

    #[test]
    fn synthesize_post_compact_prepends_summary_prefix() {
        let m = synthesize_post_compact_message("did X, Y, Z", &[]);
        assert_eq!(m.role, Role::User);
        let [ContentBlock::Text { text }] = &m.content[..] else {
            panic!("expected one text block: {m:?}");
        };
        assert!(text.contains("This conversation has been compacted"));
        assert!(text.contains("did X, Y, Z"));
    }

    #[test]
    fn synthesize_post_compact_reminds_the_model_of_the_task_list() {
        let m = synthesize_post_compact_message("did X", &plan());

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

    // ── strip_synthetic_post_compact_prefix ──

    #[test]
    fn strip_synthetic_post_compact_prefix_removes_only_the_prefix_block() {
        let mut m = synthesize_post_compact_message("internal summary", &[]);
        m.content.push(ContentBlock::Text {
            text: "real prompt".to_owned(),
        });

        assert!(strip_synthetic_post_compact_prefix(&mut m));
        assert_eq!(m.content.len(), 1);
        assert!(matches!(&m.content[0], ContentBlock::Text { text } if text == "real prompt"));
    }

    #[test]
    fn strip_synthetic_post_compact_prefix_leaves_regular_user_text() {
        let mut m = Message::user("This conversation should stay visible");

        assert!(!strip_synthetic_post_compact_prefix(&mut m));
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn strip_synthetic_post_compact_prefix_leaves_assistant_messages() {
        let mut m = Message::assistant(formatdoc! {"
            {}

            not a synthetic user root",
            SUMMARY_PREFIX.trim(),
        });

        assert!(!strip_synthetic_post_compact_prefix(&mut m));
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn strip_synthetic_post_compact_prefix_leaves_non_text_first_block() {
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
