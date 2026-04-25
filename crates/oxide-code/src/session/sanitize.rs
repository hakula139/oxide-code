//! Resume-time transcript repair.
//!
//! Turns a loaded conversation into a state the API will accept as the
//! prefix of a new turn. A mid-turn crash, a partial JSONL write, or a
//! corrupted line during load can leave any of the following shapes in
//! the chain that the Anthropic API would reject:
//!
//! - assistant `tool_use` blocks with no matching `tool_result`
//!   (crashed between the tool call and its execution);
//! - user `tool_result` blocks with no matching `tool_use` (crashed
//!   between writing the result and the next assistant turn, or whose
//!   matching call was lost to a corrupted line);
//! - two same-role messages adjacent after the filters above empty
//!   one of their neighbors;
//! - a leading assistant turn (transcripts must start with user);
//! - a trailing user turn whose only blocks are `tool_result`s — the
//!   crash window between writing tool results and the next assistant
//!   response.
//!
//! [`sanitize_resumed_messages`] is the orchestrator. It walks four
//! named passes — [`drop_unresolved_tool_uses`],
//! [`drop_orphan_tool_results`], [`collapse_consecutive_roles`], and
//! [`insert_resume_sentinels`] — each of which is also tested in
//! isolation here. The orchestrator brackets the passes with
//! [`strip_trailing_thinking`] calls because the API rejects assistant
//! turns ending in `thinking` blocks both on input and on the synthetic
//! sentinel we may have just appended.

use std::collections::HashSet;

use crate::message::{ContentBlock, Message, Role, strip_trailing_thinking};

/// Synthetic assistant content injected when resume detects a trailing
/// user turn with only `tool_result`s (i.e., the previous run crashed
/// between writing the `tool_result` message and the next assistant
/// response). Keeps role alternation valid for the next API call.
pub(super) const RESUME_CONTINUATION_SENTINEL: &str =
    "[Previous turn was interrupted; continuing.]";

/// Synthetic user content injected when resume leaves an assistant as
/// the first message. Happens when sanitization drops a leading user
/// turn whose only blocks were orphan `tool_result`s; the API rejects
/// transcripts that start with assistant, so we prepend a stub.
pub(super) const RESUME_HEAD_SENTINEL: &str = "[Previous session prefix lost in recovery.]";

/// Normalizes a loaded conversation to a state the API will accept as
/// the prefix of a new turn. See module docs for the failure modes
/// this repairs.
pub(super) fn sanitize_resumed_messages(messages: &mut Vec<Message>) {
    strip_trailing_thinking(messages);
    drop_unresolved_tool_uses(messages);
    drop_orphan_tool_results(messages);
    collapse_consecutive_roles(messages);
    insert_resume_sentinels(messages);
    strip_trailing_thinking(messages);
}

/// Drops assistant `tool_use` blocks (including server tool uses) whose
/// id never appears as a `tool_result.tool_use_id` anywhere in the
/// transcript. Owned `String` ids are an intentional allocation: a
/// borrowed-`&str` set into `messages` would conflict with the
/// follow-up mutable iteration. The cleanup-follow-ups doc tracks the
/// optimization separately as item 2.
fn drop_unresolved_tool_uses(messages: &mut [Message]) {
    let resolved: HashSet<String> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();

    for msg in messages {
        if msg.role != Role::Assistant {
            continue;
        }
        msg.content.retain(|b| match b {
            ContentBlock::ToolUse { id, .. } | ContentBlock::ServerToolUse { id, .. } => {
                resolved.contains(id)
            }
            _ => true,
        });
    }
}

/// Drops user `tool_result` blocks whose `tool_use_id` does not match
/// any surviving assistant `tool_use`. Run after
/// [`drop_unresolved_tool_uses`] so that a `tool_use` filtered out in
/// step one also takes its paired `tool_result` with it. Empty messages
/// left behind by either filter are removed in a single retain pass at
/// the end so subsequent passes see no stubs.
fn drop_orphan_tool_results(messages: &mut Vec<Message>) {
    let surviving: HashSet<String> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } | ContentBlock::ServerToolUse { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect();

    for msg in &mut *messages {
        if msg.role != Role::User {
            continue;
        }
        msg.content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => surviving.contains(tool_use_id),
            _ => true,
        });
    }

    messages.retain(|m| !m.content.is_empty());
}

/// Merges every pair of consecutive same-role messages by extending the
/// earlier message's content with the later one's and dropping the
/// later one. The API requires strict user / assistant alternation;
/// after the filter passes drop messages whole, two same-role neighbors
/// can become adjacent, and merging their content preserves every
/// block while restoring alternation.
fn collapse_consecutive_roles(messages: &mut Vec<Message>) {
    messages.dedup_by(|next, prev| {
        if prev.role == next.role {
            prev.content.append(&mut next.content);
            true
        } else {
            false
        }
    });
}

/// Patches the head and tail of the transcript so the next API call
/// has a valid shape:
///
/// - prepends a synthetic user turn if the chain starts with an
///   assistant message (reached when the leading user turn had only
///   orphan `tool_result`s and was dropped);
/// - appends a synthetic assistant turn when the last message is a
///   user turn containing only `tool_result`s — the crash window
///   between writing tool results and the next assistant response.
fn insert_resume_sentinels(messages: &mut Vec<Message>) {
    if messages
        .first()
        .is_some_and(|first| first.role == Role::Assistant)
    {
        messages.insert(0, Message::user(RESUME_HEAD_SENTINEL));
    }

    if messages.last().is_some_and(|last| {
        last.role == Role::User
            && last
                .content
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }) {
        messages.push(Message::assistant(RESUME_CONTINUATION_SENTINEL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unresolved_tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::Value::Null,
        }
    }

    fn tool_result(id: &str, content: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_owned(),
        }
    }

    // ── sanitize_resumed_messages ──

    #[test]
    fn sanitize_noop_for_clean_transcript() {
        let mut messages = vec![
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("bye"),
        ];
        let before = messages.clone();
        sanitize_resumed_messages(&mut messages);
        assert_eq!(messages.len(), before.len());
        for (got, want) in messages.iter().zip(before.iter()) {
            assert_eq!(got.role, want.role);
            assert_eq!(got.content.len(), want.content.len());
        }
    }

    #[test]
    fn sanitize_pairs_tool_use_with_result_and_appends_continuation_sentinel() {
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::Assistant,
                content: vec![
                    text("checking"),
                    unresolved_tool_use("t1"),
                    unresolved_tool_use("t2"),
                ],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("t1", "ok")],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        // Assistant: text + t1 (resolved) — t2 dropped as unresolved.
        let assistant_blocks = &messages[1].content;
        assert_eq!(assistant_blocks.len(), 2);
        assert!(matches!(&assistant_blocks[0], ContentBlock::Text { .. }));
        assert!(matches!(&assistant_blocks[1], ContentBlock::ToolUse { id, .. } if id == "t1"));
        // Trailing tool_result-only user → assistant sentinel appended.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].role, Role::Assistant);
        assert!(
            matches!(&messages[3].content[0], ContentBlock::Text { text } if text == RESUME_CONTINUATION_SENTINEL)
        );
    }

    #[test]
    fn sanitize_drops_orphan_tool_result_block_and_keeps_siblings() {
        let mut messages = vec![
            Message::user("do X"),
            Message::assistant("done, no tool needed"),
            Message {
                role: Role::User,
                content: vec![tool_result("orphan", "ghost"), text("follow-up")],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 3);
        let last = &messages[2];
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.len(), 1);
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == "follow-up"));
    }

    #[test]
    fn sanitize_drops_user_message_with_only_orphan_tool_result() {
        let mut messages = vec![
            Message::user("do X"),
            Message::assistant("all clear"),
            Message {
                role: Role::User,
                content: vec![tool_result("ghost", "nobody asked")],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn sanitize_collapses_adjacent_users_after_empty_assistant_drop() {
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::Assistant,
                content: vec![unresolved_tool_use("unresolved")],
            },
            Message::user("and now Y"),
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content.len(), 2);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "do X"));
        assert!(
            matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "and now Y")
        );
    }

    #[test]
    fn sanitize_collapses_adjacent_assistants_after_orphan_user_drop() {
        let mut messages = vec![
            Message::user("do X"),
            Message::assistant("first answer"),
            Message {
                role: Role::User,
                content: vec![tool_result("ghost", "stale")],
            },
            Message::assistant("second answer"),
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        let answer = &messages[1].content;
        assert_eq!(answer.len(), 2);
        assert!(matches!(&answer[0], ContentBlock::Text { text } if text == "first answer"));
        assert!(matches!(&answer[1], ContentBlock::Text { text } if text == "second answer"));
    }

    #[test]
    fn sanitize_drops_orphan_when_assistant_tool_use_was_dropped() {
        // Assistant's "t1" is unresolved; user references "t2"; after
        // step 1 there are no surviving tool_use ids, so step 2 drops
        // the user's tool_result. Sibling text on the assistant
        // survives so both turns remain.
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::Assistant,
                content: vec![text("checking"), unresolved_tool_use("t1")],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("t2", "stale")],
            },
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        let assistant = &messages[1].content;
        assert_eq!(assistant.len(), 1);
        assert!(matches!(&assistant[0], ContentBlock::Text { text } if text == "checking"));
    }

    #[test]
    fn sanitize_prepends_head_sentinel_when_leading_user_is_dropped() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![tool_result("ghost", "stale")],
            },
            Message::assistant("carrying on"),
            Message::user("next question"),
        ];
        sanitize_resumed_messages(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == RESUME_HEAD_SENTINEL)
        );
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);
    }

    // ── drop_unresolved_tool_uses ──

    #[test]
    fn drop_unresolved_tool_uses_removes_assistant_block_without_matching_result() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![text("checking"), unresolved_tool_use("ghost")],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("other", "irrelevant")],
            },
        ];
        drop_unresolved_tool_uses(&mut messages);

        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn drop_unresolved_tool_uses_keeps_resolved_calls_and_ignores_user_role() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![unresolved_tool_use("kept")],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("kept", "ok")],
            },
        ];
        drop_unresolved_tool_uses(&mut messages);

        assert_eq!(messages[0].content.len(), 1);
        assert_eq!(messages[1].content.len(), 1, "user role untouched");
    }

    // ── drop_orphan_tool_results ──

    #[test]
    fn drop_orphan_tool_results_removes_user_block_without_matching_call() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![unresolved_tool_use("real")],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("real", "ok"), tool_result("ghost", "stale")],
            },
        ];
        drop_orphan_tool_results(&mut messages);

        assert_eq!(messages[1].content.len(), 1);
        assert!(
            matches!(&messages[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "real")
        );
    }

    #[test]
    fn drop_orphan_tool_results_drops_emptied_messages() {
        let mut messages = vec![
            Message::user("first"),
            Message {
                role: Role::User,
                content: vec![tool_result("ghost", "stale")],
            },
        ];
        drop_orphan_tool_results(&mut messages);

        assert_eq!(messages.len(), 1, "user message with only orphan dropped");
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "first"));
    }

    // ── collapse_consecutive_roles ──

    #[test]
    fn collapse_consecutive_roles_merges_runs_and_preserves_alternation() {
        let mut messages = vec![
            Message::assistant("a1"),
            Message::assistant("a2"),
            Message::assistant("a3"),
            Message::user("u1"),
            Message::user("u2"),
        ];
        collapse_consecutive_roles(&mut messages);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[0].content.len(), 3);
        assert_eq!(messages[1].role, Role::User);
        assert_eq!(messages[1].content.len(), 2);
    }

    #[test]
    fn collapse_consecutive_roles_noop_on_alternating_transcript() {
        let mut messages = vec![
            Message::user("u"),
            Message::assistant("a"),
            Message::user("u2"),
        ];
        collapse_consecutive_roles(&mut messages);

        assert_eq!(messages.len(), 3);
    }

    // ── insert_resume_sentinels ──

    #[test]
    fn insert_resume_sentinels_prepends_user_when_leading_assistant() {
        let mut messages = vec![Message::assistant("orphaned"), Message::user("next")];
        insert_resume_sentinels(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == RESUME_HEAD_SENTINEL)
        );
    }

    #[test]
    fn insert_resume_sentinels_appends_assistant_after_tool_result_only_user() {
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::User,
                content: vec![tool_result("t1", "ok")],
            },
        ];
        insert_resume_sentinels(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::Assistant);
        assert!(
            matches!(&messages[2].content[0], ContentBlock::Text { text } if text == RESUME_CONTINUATION_SENTINEL)
        );
    }

    #[test]
    fn insert_resume_sentinels_skips_append_when_trailing_user_has_text() {
        // A trailing user turn that mixes text with tool_result is a
        // normal followup, not the post-crash shape — no sentinel.
        let mut messages = vec![
            Message::user("do X"),
            Message {
                role: Role::User,
                content: vec![tool_result("t1", "ok"), text("and a question")],
            },
        ];
        let before_len = messages.len();
        insert_resume_sentinels(&mut messages);

        assert_eq!(messages.len(), before_len);
    }

    #[test]
    fn insert_resume_sentinels_noop_on_empty_input() {
        let mut messages: Vec<Message> = Vec::new();
        insert_resume_sentinels(&mut messages);
        assert!(messages.is_empty());
    }
}
