//! Resume-time transcript repair. Drops unresolved tool calls and orphan tool results, collapses
//! same-role neighbors, and patches head / tail edges the API would reject. File-tracker snapshots
//! follow the repaired transcript.

use std::collections::HashSet;

use crate::file_tracker::FileSnapshot;
use crate::message::{ContentBlock, Message, Role};

// ── Constants ──

/// Stub appended after a trailing tool-result-only user turn so the next API call has a
/// valid assistant follow-up.
const RESUME_CONTINUATION_SENTINEL: &str = "[Previous turn was interrupted. Continuing.]";

/// Stub prepended when sanitization leaves an assistant as the first message, since the API
/// rejects transcripts that start with assistant.
const RESUME_HEAD_SENTINEL: &str = "[Previous session prefix lost in recovery.]";

// ── Entry Point ──

/// Repairs a transcript loaded from JSONL so the next API call won't 400. Pass order matters.
/// Unresolved `tool_use` drops first, so a `tool_result` paired only with a dropped call is
/// orphaned in the next pass. Roles collapse after both filters, so emptied messages can't desync
/// the alternation. Sentinels run last, once the head / tail shape has settled.
pub(super) fn sanitize_resumed_messages(messages: &mut Vec<Message>) {
    drop_unresolved_tool_uses(messages);
    drop_orphan_tool_results(messages);
    collapse_consecutive_roles(messages);
    insert_resume_sentinels(messages);
}

/// Keeps only the snapshots whose recording tool call still has its result in the repaired
/// `messages`, so the Read-before-Edit gate never clears for content the model cannot see.
pub(super) fn retain_visible_file_snapshots(
    snapshots: &mut Vec<FileSnapshot>,
    messages: &[Message],
) {
    let visible = tool_result_ids(messages).client;
    snapshots.retain(|snap| visible.contains(snap.state.tool_use_id.as_str()));
}

// ── Passes ──

/// Drops assistant tool calls with no result anywhere in the transcript. That includes a server
/// call a `pause_turn` left waiting, because the next prompt closes the turn and the API rejects a
/// server call left without its result.
fn drop_unresolved_tool_uses(messages: &mut [Message]) {
    let resolved = tool_result_ids(messages);

    for msg in messages {
        if msg.role != Role::Assistant {
            continue;
        }
        msg.content.retain(|b| match b {
            ContentBlock::ToolUse { id, .. } => resolved.client.contains(id),
            ContentBlock::ServerToolUse { id, .. } => resolved.server.contains(id),
            _ => true,
        });
    }
}

/// Drops tool results with no surviving call, then strips messages that prior passes emptied so
/// role-collapse sees no stubs.
fn drop_orphan_tool_results(messages: &mut Vec<Message>) {
    let surviving = tool_use_ids(messages);

    for msg in &mut *messages {
        msg.content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => surviving.client.contains(tool_use_id),
            ContentBlock::ServerToolResult { tool_use_id, .. } => {
                surviving.server.contains(tool_use_id)
            }
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

/// Patches head / tail edges the API rejects. Prepends a user stub before a leading assistant and
/// appends an assistant stub after a trailing tool-results-only user.
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

/// Tool-use IDs by where the call ran, since IDs pair only on their own side. A client call's
/// result comes back in a user `tool_result`. A server call's comes back in an assistant reply,
/// the next one when a `pause_turn` or a client call in the same reply deferred it.
#[derive(Default)]
struct ToolIds {
    client: HashSet<String>,
    server: HashSet<String>,
}

fn tool_result_ids(messages: &[Message]) -> ToolIds {
    let mut ids = ToolIds::default();
    for block in messages.iter().flat_map(|m| &m.content) {
        let (side, id) = match block {
            ContentBlock::ToolResult { tool_use_id, .. } => (&mut ids.client, tool_use_id),
            ContentBlock::ServerToolResult { tool_use_id, .. } => (&mut ids.server, tool_use_id),
            _ => continue,
        };
        side.insert(id.clone());
    }
    ids
}

fn tool_use_ids(messages: &[Message]) -> ToolIds {
    let mut ids = ToolIds::default();
    for block in messages.iter().flat_map(|m| &m.content) {
        let (side, id) = match block {
            ContentBlock::ToolUse { id, .. } => (&mut ids.client, id),
            ContentBlock::ServerToolUse { id, .. } => (&mut ids.server, id),
            _ => continue,
        };
        side.insert(id.clone());
    }
    ids
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::file_tracker::{FileState, LastView};

    fn unresolved_tool_use(id: &str) -> ContentBlock {
        ContentBlock::tool_use(id, "bash", serde_json::Value::Null)
    }

    fn server_tool_use(id: &str) -> ContentBlock {
        ContentBlock::ServerToolUse {
            id: id.to_owned(),
            name: "web_search".to_owned(),
            input: serde_json::Value::Null,
        }
    }

    fn server_tool_result(id: &str) -> ContentBlock {
        ContentBlock::ServerToolResult {
            tool_use_id: id.to_owned(),
            kind: "web_search_tool_result".to_owned(),
            fields: serde_json::Map::new(),
        }
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_owned(),
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    fn user(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            content,
        }
    }

    mod sanitize_resumed_messages {
        use super::*;

        #[test]
        fn noop_for_clean_transcript() {
            let mut messages = vec![
                Message::user("hello"),
                Message::assistant("hi"),
                Message::user("bye"),
            ];
            let before = messages.clone();

            sanitize_resumed_messages(&mut messages);

            assert_eq!(messages, before);
        }

        #[test]
        fn pairs_tool_use_with_result_and_appends_continuation_sentinel() {
            let mut messages = vec![
                Message::user("do X"),
                assistant(vec![
                    text("checking"),
                    unresolved_tool_use("t1"),
                    unresolved_tool_use("t2"),
                ]),
                user(vec![ContentBlock::tool_result("t1", "ok", false)]),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(
                messages,
                [
                    Message::user("do X"),
                    assistant(vec![text("checking"), unresolved_tool_use("t1")]),
                    user(vec![ContentBlock::tool_result("t1", "ok", false)]),
                    Message::assistant(RESUME_CONTINUATION_SENTINEL),
                ],
            );
        }

        #[test]
        fn drops_orphan_tool_result_block_and_keeps_siblings() {
            let mut messages = vec![
                Message::user("do X"),
                Message::assistant("done, no tool needed"),
                user(vec![
                    ContentBlock::tool_result("orphan", "ghost", false),
                    text("follow-up"),
                ]),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(
                messages,
                [
                    Message::user("do X"),
                    Message::assistant("done, no tool needed"),
                    Message::user("follow-up"),
                ],
            );
        }

        #[test]
        fn drops_user_message_with_only_orphan_tool_result() {
            let mut messages = vec![
                Message::user("do X"),
                Message::assistant("all clear"),
                user(vec![ContentBlock::tool_result(
                    "ghost",
                    "nobody asked",
                    false,
                )]),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(
                messages,
                [Message::user("do X"), Message::assistant("all clear")],
            );
        }

        #[test]
        fn collapses_adjacent_users_after_empty_assistant_drop() {
            let mut messages = vec![
                Message::user("do X"),
                assistant(vec![unresolved_tool_use("unresolved")]),
                Message::user("and now Y"),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(messages, [user(vec![text("do X"), text("and now Y")])]);
        }

        #[test]
        fn collapses_adjacent_assistants_after_orphan_user_drop() {
            let mut messages = vec![
                Message::user("do X"),
                Message::assistant("first answer"),
                user(vec![ContentBlock::tool_result("ghost", "stale", false)]),
                Message::assistant("second answer"),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(
                messages,
                [
                    Message::user("do X"),
                    assistant(vec![text("first answer"), text("second answer")]),
                ],
            );
        }

        #[test]
        fn drops_orphan_when_assistant_tool_use_was_dropped() {
            let mut messages = vec![
                Message::user("do X"),
                assistant(vec![text("checking"), unresolved_tool_use("t1")]),
                user(vec![ContentBlock::tool_result("t2", "stale", false)]),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(
                messages,
                [Message::user("do X"), Message::assistant("checking")],
            );
        }

        #[test]
        fn keeps_answered_server_calls_and_drops_a_paused_one() {
            let mut messages = vec![
                Message::user("research X"),
                assistant(vec![text("Searching."), server_tool_use("s1")]),
                assistant(vec![
                    server_tool_result("s1"),
                    text("Found X."),
                    server_tool_use("s2"),
                ]),
            ];

            sanitize_resumed_messages(&mut messages);

            // `s1` paused and its continuation answered it, but `s2` paused as the session ended.
            assert_eq!(
                messages,
                [
                    Message::user("research X"),
                    assistant(vec![
                        text("Searching."),
                        server_tool_use("s1"),
                        server_tool_result("s1"),
                        text("Found X."),
                    ]),
                ],
            );
        }

        #[test]
        fn prepends_head_sentinel_when_leading_user_is_dropped() {
            let mut messages = vec![
                user(vec![ContentBlock::tool_result("ghost", "stale", false)]),
                Message::assistant("carrying on"),
                Message::user("next question"),
            ];

            sanitize_resumed_messages(&mut messages);

            assert_eq!(
                messages,
                [
                    Message::user(RESUME_HEAD_SENTINEL),
                    Message::assistant("carrying on"),
                    Message::user("next question"),
                ],
            );
        }
    }

    mod retain_visible_file_snapshots {
        use super::*;

        fn snapshot(path: &str, tool_use_id: &str) -> FileSnapshot {
            FileSnapshot {
                path: PathBuf::from(path),
                state: FileState {
                    content_hash: 1,
                    last_view: LastView::Full,
                    recorded_at: OffsetDateTime::UNIX_EPOCH,
                    tool_use_id: tool_use_id.to_owned(),
                },
            }
        }

        #[test]
        fn drops_records_whose_call_sanitize_removed() {
            let mut messages = vec![
                Message::user("read both"),
                assistant(vec![
                    unresolved_tool_use("seen"),
                    unresolved_tool_use("lost"),
                ]),
                user(vec![ContentBlock::tool_result("seen", "a body", false)]),
                Message::assistant("done"),
            ];
            let mut snapshots = vec![snapshot("/a", "seen"), snapshot("/b", "lost")];

            sanitize_resumed_messages(&mut messages);
            retain_visible_file_snapshots(&mut snapshots, &messages);

            assert_eq!(snapshots, [snapshot("/a", "seen")]);
        }
    }

    mod drop_unresolved_tool_uses {
        use super::*;

        #[test]
        fn removes_assistant_block_without_matching_result() {
            let mut messages = vec![
                assistant(vec![text("checking"), unresolved_tool_use("ghost")]),
                user(vec![ContentBlock::tool_result(
                    "other",
                    "irrelevant",
                    false,
                )]),
            ];

            drop_unresolved_tool_uses(&mut messages);

            assert_eq!(
                messages,
                [
                    assistant(vec![text("checking")]),
                    user(vec![ContentBlock::tool_result(
                        "other",
                        "irrelevant",
                        false
                    )]),
                ],
            );
        }

        #[test]
        fn pairs_server_calls_only_with_results_in_replies() {
            // The API ran `deferred` only after client call `t1` returned, so its result opens the
            // next reply. `misplaced` has only a user `tool_result`, which answers no server call.
            let mut messages = vec![
                assistant(vec![
                    server_tool_use("deferred"),
                    unresolved_tool_use("t1"),
                    server_tool_use("misplaced"),
                ]),
                user(vec![
                    ContentBlock::tool_result("t1", "ok", false),
                    ContentBlock::tool_result("misplaced", "stale", false),
                ]),
                assistant(vec![
                    server_tool_result("deferred"),
                    server_tool_use("same"),
                    server_tool_result("same"),
                    server_tool_use("ghost"),
                ]),
            ];

            drop_unresolved_tool_uses(&mut messages);

            assert_eq!(
                messages,
                [
                    assistant(vec![server_tool_use("deferred"), unresolved_tool_use("t1")]),
                    user(vec![
                        ContentBlock::tool_result("t1", "ok", false),
                        ContentBlock::tool_result("misplaced", "stale", false),
                    ]),
                    assistant(vec![
                        server_tool_result("deferred"),
                        server_tool_use("same"),
                        server_tool_result("same"),
                    ]),
                ],
            );
        }

        #[test]
        fn keeps_resolved_calls_and_ignores_user_role() {
            let mut messages = vec![
                assistant(vec![unresolved_tool_use("kept")]),
                user(vec![ContentBlock::tool_result("kept", "ok", false)]),
            ];
            let before = messages.clone();

            drop_unresolved_tool_uses(&mut messages);

            assert_eq!(messages, before);
        }
    }

    mod drop_orphan_tool_results {
        use super::*;

        #[test]
        fn removes_user_block_without_matching_call() {
            let mut messages = vec![
                assistant(vec![unresolved_tool_use("real")]),
                user(vec![
                    ContentBlock::tool_result("real", "ok", false),
                    ContentBlock::tool_result("ghost", "stale", false),
                ]),
            ];

            drop_orphan_tool_results(&mut messages);

            assert_eq!(
                messages,
                [
                    assistant(vec![unresolved_tool_use("real")]),
                    user(vec![ContentBlock::tool_result("real", "ok", false)]),
                ],
            );
        }

        #[test]
        fn pairs_server_results_only_with_server_calls() {
            let mut messages = vec![
                assistant(vec![
                    server_tool_use("srv_1"),
                    server_tool_result("srv_1"),
                    server_tool_result("ghost"),
                ]),
                user(vec![ContentBlock::tool_result("srv_1", "stale", false)]),
            ];

            drop_orphan_tool_results(&mut messages);

            assert_eq!(
                messages,
                [assistant(vec![
                    server_tool_use("srv_1"),
                    server_tool_result("srv_1")
                ])],
            );
        }

        #[test]
        fn drops_emptied_messages() {
            let mut messages = vec![
                Message::user("first"),
                user(vec![ContentBlock::tool_result("ghost", "stale", false)]),
            ];

            drop_orphan_tool_results(&mut messages);

            assert_eq!(messages, [Message::user("first")]);
        }
    }

    mod collapse_consecutive_roles {
        use super::*;

        #[test]
        fn merges_runs_and_preserves_alternation() {
            let mut messages = vec![
                Message::assistant("a1"),
                Message::assistant("a2"),
                Message::assistant("a3"),
                Message::user("u1"),
                Message::user("u2"),
            ];

            collapse_consecutive_roles(&mut messages);

            assert_eq!(
                messages,
                [
                    assistant(vec![text("a1"), text("a2"), text("a3")]),
                    user(vec![text("u1"), text("u2")]),
                ],
            );
        }

        #[test]
        fn noop_on_alternating_transcript() {
            let mut messages = vec![
                Message::user("u"),
                Message::assistant("a"),
                Message::user("u2"),
            ];
            let before = messages.clone();

            collapse_consecutive_roles(&mut messages);

            assert_eq!(messages, before);
        }
    }

    mod insert_resume_sentinels {
        use super::*;

        #[test]
        fn prepends_user_when_leading_assistant() {
            let mut messages = vec![Message::assistant("orphaned"), Message::user("next")];

            insert_resume_sentinels(&mut messages);

            assert_eq!(
                messages,
                [
                    Message::user(RESUME_HEAD_SENTINEL),
                    Message::assistant("orphaned"),
                    Message::user("next"),
                ],
            );
        }

        #[test]
        fn appends_assistant_after_tool_result_only_user() {
            let mut messages = vec![
                Message::user("do X"),
                user(vec![ContentBlock::tool_result("t1", "ok", false)]),
            ];

            insert_resume_sentinels(&mut messages);

            assert_eq!(
                messages,
                [
                    Message::user("do X"),
                    user(vec![ContentBlock::tool_result("t1", "ok", false)]),
                    Message::assistant(RESUME_CONTINUATION_SENTINEL),
                ],
            );
        }

        #[test]
        fn skips_append_when_trailing_user_has_text() {
            let mut messages = vec![
                Message::user("do X"),
                user(vec![
                    ContentBlock::tool_result("t1", "ok", false),
                    text("and a question"),
                ]),
            ];
            let before = messages.clone();

            insert_resume_sentinels(&mut messages);

            assert_eq!(messages, before);
        }

        #[test]
        fn noop_on_empty_input() {
            let mut messages: Vec<Message> = Vec::new();
            insert_resume_sentinels(&mut messages);
            assert_eq!(messages, []);
        }
    }
}
