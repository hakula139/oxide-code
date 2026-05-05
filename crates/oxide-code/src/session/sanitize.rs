//! Resume-time transcript repair. Drops unresolved `tool_use` / orphan `tool_result` blocks,
//! collapses same-role neighbors, and patches head / tail edges the API would reject.

use std::collections::HashSet;

use crate::message::{ContentBlock, Message, Role, strip_trailing_thinking};

// ── Constants ──

/// Stub appended after a trailing tool-result-only user turn so the next API call has a
/// valid assistant follow-up.
pub(super) const RESUME_CONTINUATION_SENTINEL: &str =
    "[Previous turn was interrupted; continuing.]";

/// Stub prepended when sanitization leaves an assistant as the first message — the API rejects
/// transcripts that start with assistant.
pub(super) const RESUME_HEAD_SENTINEL: &str = "[Previous session prefix lost in recovery.]";

// ── Entry Point ──

/// Repairs a transcript loaded from JSONL so the next API call won't 400. Pass order matters:
/// drop unresolved `tool_use` first so any `tool_result` paired only with a dropped call is
/// orphaned in the next pass; collapse roles after both filter passes so emptied messages
/// can't desync the alternation; sentinels run last because head / tail shape only stabilizes
/// post-collapse. The trailing thinking strip runs twice — once before, once after — so a
/// thinking block at the end of a now-removed assistant turn doesn't survive into the next API
/// call.
pub(super) fn sanitize_resumed_messages(messages: &mut Vec<Message>) {
    strip_trailing_thinking(messages);
    drop_unresolved_tool_uses(messages);
    drop_orphan_tool_results(messages);
    collapse_consecutive_roles(messages);
    insert_resume_sentinels(messages);
    strip_trailing_thinking(messages);
}

// ── Passes ──

/// Drops assistant `tool_use` blocks whose id has no matching `tool_result`. Ids are owned
/// because the `HashSet` is built from a shared borrow before mutating below.
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

/// Drops user `tool_result` blocks with no surviving assistant `tool_use`, then strips
/// messages that prior passes emptied so role-collapse sees no stubs.
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

/// Merges adjacent same-role runs so role alternation holds after the filter passes.
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

/// Patches head / tail edges the API rejects: prepends a user stub on leading-assistant,
/// appends an assistant stub on trailing tool-results-only user.
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

    fn server_tool_use(id: &str) -> ContentBlock {
        ContentBlock::ServerToolUse {
            id: id.to_owned(),
            name: "web_search".to_owned(),
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
        // Mismatched ids: pass 1 strips the assistant's unresolved t1,
        // leaving no surviving ids, so pass 2 drops the user's t2.
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
    fn drop_unresolved_tool_uses_treats_server_tool_use_like_tool_use() {
        // Both `ToolUse` and `ServerToolUse` arms must match — covers the OR in the filter.
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![server_tool_use("kept"), server_tool_use("ghost")],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("kept", "ok")],
            },
        ];
        drop_unresolved_tool_uses(&mut messages);

        assert_eq!(messages[0].content.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ServerToolUse { id, .. } if id == "kept")
        );
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
    fn drop_orphan_tool_results_keeps_results_paired_to_server_tool_use() {
        // Surviving-id set must include `ServerToolUse` ids, else its result would be orphaned.
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![server_tool_use("srv_1")],
            },
            Message {
                role: Role::User,
                content: vec![tool_result("srv_1", "ok")],
            },
        ];
        drop_orphan_tool_results(&mut messages);

        assert_eq!(messages[1].content.len(), 1);
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
        // Mixed text + tool_result is a normal follow-up, not the post-crash shape — no sentinel.
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
