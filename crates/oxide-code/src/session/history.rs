//! Display-oriented reshaping of a loaded session transcript.
//!
//! The JSONL layer stores assistant turns as `Message { content: [ToolUse,
//! ToolUse, ...] }` followed by a single user turn with the matching
//! `ToolResult` blocks batched together. Live rendering, by contrast, emits
//! each `ToolCall` immediately followed by its `ToolResult`. Without some
//! rearrangement, a resumed session would scroll through all the calls and
//! then all the results — visually different from how the same conversation
//! rendered while streaming.
//!
//! [`walk_transcript`] produces an [`Interaction`] sequence that matches the
//! live layout: calls paired with their results inline, orphan results
//! preserved at their original position with an explicit
//! [`Interaction::OrphanToolResult`] marker, and adjacent text blocks within
//! the same message merged into a single entry. Any content that is not
//! surfaced (whitespace-only text, `RedactedThinking`) is dropped here so
//! renderers do not need to repeat the filtering.
//!
//! The transform is pure and owns no dependencies beyond [`Message`]. Tool
//! label resolution is left to the caller, which usually needs a
//! [`ToolRegistry`](crate::tool::ToolRegistry) that the session layer should not depend on.

use std::collections::HashMap;

use crate::message::{ContentBlock, Message, Role};

/// A logical interaction derived from a session transcript.
///
/// Emitted in display order: text and thinking appear in the order they
/// occurred inside a message; tool calls are followed immediately by their
/// paired [`ToolResult`][Self::ToolResult] (consumed from wherever it appears
/// in the transcript); unpaired results surface as
/// [`OrphanToolResult`][Self::OrphanToolResult] at their original position.
#[derive(Debug)]
pub(crate) enum Interaction<'a> {
    UserText(String),
    AssistantText(String),
    AssistantThinking(&'a str),
    ToolCall {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
    /// A tool result whose `tool_use_id` has no matching call in the same
    /// transcript — typically a leftover after crash-recovery sanitization
    /// trimmed the call. Callers render with a fallback label.
    OrphanToolResult {
        content: &'a str,
        is_error: bool,
    },
}

/// Walks a resumed transcript and emits interactions in display order.
///
/// Text blocks inside a single message are merged into one entry; whitespace
/// only entries are dropped. Tool calls are paired inline with their matching
/// results via the `tool_use_id` index, so the output mirrors the
/// live-streaming layout regardless of how the JSONL batched them.
pub(crate) fn walk_transcript(messages: &[Message]) -> Vec<Interaction<'_>> {
    let mut pairs: HashMap<&str, (&str, bool)> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();

    let mut out = Vec::new();
    for msg in messages {
        let mut text_buf = String::new();
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } if !text.trim().is_empty() => {
                    if !text_buf.is_empty() {
                        text_buf.push('\n');
                    }
                    text_buf.push_str(text);
                }
                ContentBlock::ToolUse { id, name, input }
                | ContentBlock::ServerToolUse { id, name, input } => {
                    flush_text(&mut out, &mut text_buf, msg.role);
                    out.push(Interaction::ToolCall { id, name, input });
                    if let Some((content, is_error)) = pairs.remove(id.as_str()) {
                        out.push(Interaction::ToolResult {
                            tool_use_id: id,
                            content,
                            is_error,
                        });
                    }
                }
                // Paired results were already emitted inline; only orphans
                // reach this arm (the pending-map guard ensures that).
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if pairs.remove(tool_use_id.as_str()).is_some() => {
                    flush_text(&mut out, &mut text_buf, msg.role);
                    out.push(Interaction::OrphanToolResult {
                        content,
                        is_error: *is_error,
                    });
                }
                ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                    flush_text(&mut out, &mut text_buf, msg.role);
                    out.push(Interaction::AssistantThinking(thinking));
                }
                _ => {}
            }
        }
        flush_text(&mut out, &mut text_buf, msg.role);
    }
    out
}

/// Emits any accumulated `text_buf` as a role-tagged text interaction and
/// reset the buffer. No-op when the buffer is empty.
fn flush_text(out: &mut Vec<Interaction<'_>>, text_buf: &mut String, role: Role) {
    if text_buf.is_empty() {
        return;
    }
    let text = std::mem::take(text_buf);
    out.push(match role {
        Role::User => Interaction::UserText(text),
        Role::Assistant => Interaction::AssistantText(text),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── walk_transcript ──

    #[test]
    fn walk_transcript_empty_yields_no_interactions() {
        assert!(walk_transcript(&[]).is_empty());
    }

    #[test]
    fn walk_transcript_emits_user_and_assistant_text() {
        let messages = [Message::user("hi"), Message::assistant("hello")];
        let out = walk_transcript(&messages);
        assert!(matches!(&out[..], [
            Interaction::UserText(u),
            Interaction::AssistantText(a),
        ] if u == "hi" && a == "hello"));
    }

    #[test]
    fn walk_transcript_joins_adjacent_text_blocks_with_newline() {
        let messages = [Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "first".to_owned(),
                },
                ContentBlock::Text {
                    text: "second".to_owned(),
                },
            ],
        }];
        let out = walk_transcript(&messages);
        assert!(matches!(&out[..], [Interaction::AssistantText(t)] if t == "first\nsecond"));
    }

    #[test]
    fn walk_transcript_drops_whitespace_only_text() {
        let messages = [Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "  \n  ".to_owned(),
            }],
        }];
        assert!(walk_transcript(&messages).is_empty());
    }

    #[test]
    fn walk_transcript_pairs_tool_call_and_result_inline() {
        let messages = [
            Message::user("ask"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "ls"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_owned(),
                    content: "output".to_owned(),
                    is_error: false,
                }],
            },
            Message::assistant("reply"),
        ];
        let out = walk_transcript(&messages);
        assert!(matches!(
            &out[..],
            [
                Interaction::UserText(_),
                Interaction::ToolCall {
                    id: "t1",
                    name: "bash",
                    ..
                },
                Interaction::ToolResult {
                    tool_use_id: "t1",
                    content: "output",
                    is_error: false
                },
                Interaction::AssistantText(_),
            ]
        ));
    }

    #[test]
    fn walk_transcript_pairs_multiple_calls_in_order_even_across_batched_results() {
        // JSONL stores calls and results batched: Assistant[Call1, Call2]
        // then User[Result1, Result2]. Live rendering emits them paired:
        // Call1 → Result1 → Call2 → Result2. The transform must produce
        // the paired order regardless of JSONL batching.
        let messages = [
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "t1".to_owned(),
                        name: "read".to_owned(),
                        input: serde_json::json!({"file_path": "a.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".to_owned(),
                        name: "grep".to_owned(),
                        input: serde_json::json!({"pattern": "TODO"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "file a".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "t2".to_owned(),
                        content: "3 matches".to_owned(),
                        is_error: false,
                    },
                ],
            },
        ];
        let out = walk_transcript(&messages);
        assert!(matches!(
            &out[..],
            [
                Interaction::ToolCall { id: "t1", .. },
                Interaction::ToolResult {
                    tool_use_id: "t1",
                    content: "file a",
                    ..
                },
                Interaction::ToolCall { id: "t2", .. },
                Interaction::ToolResult {
                    tool_use_id: "t2",
                    content: "3 matches",
                    ..
                },
            ]
        ));
    }

    #[test]
    fn walk_transcript_preserves_orphan_tool_result_at_original_position() {
        // "ghost" has no matching call — emit with OrphanToolResult at its
        // original position so it doesn't inherit a sibling's label.
        let messages = [
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({"file_path": "a.rs"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".to_owned(),
                        content: "file a".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "ghost".to_owned(),
                        content: "stale".to_owned(),
                        is_error: true,
                    },
                ],
            },
        ];
        let out = walk_transcript(&messages);
        assert!(matches!(
            &out[..],
            [
                Interaction::ToolCall { id: "t1", .. },
                Interaction::ToolResult {
                    tool_use_id: "t1",
                    ..
                },
                Interaction::OrphanToolResult {
                    content: "stale",
                    is_error: true,
                },
            ]
        ));
    }

    #[test]
    fn walk_transcript_emits_server_tool_use_as_tool_call() {
        let messages = [Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ServerToolUse {
                id: "srv1".to_owned(),
                name: "web_search".to_owned(),
                input: serde_json::json!({"query": "rust"}),
            }],
        }];
        let out = walk_transcript(&messages);
        assert!(matches!(
            &out[..],
            [Interaction::ToolCall {
                id: "srv1",
                name: "web_search",
                ..
            }]
        ));
    }

    #[test]
    fn walk_transcript_flushes_text_before_tool_call() {
        let messages = [Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Let me check".to_owned(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "ls"}),
                },
                ContentBlock::Text {
                    text: "Done".to_owned(),
                },
            ],
        }];
        let out = walk_transcript(&messages);
        assert!(matches!(&out[..], [
            Interaction::AssistantText(t1),
            Interaction::ToolCall { id: "t1", .. },
            Interaction::AssistantText(t2),
        ] if t1 == "Let me check" && t2 == "Done"));
    }

    #[test]
    fn walk_transcript_emits_thinking_and_drops_redacted() {
        let messages = [Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".to_owned(),
                },
                ContentBlock::Text {
                    text: "answer".to_owned(),
                },
            ],
        }];
        let out = walk_transcript(&messages);
        assert!(matches!(&out[..], [
            Interaction::AssistantThinking(t),
            Interaction::AssistantText(a),
        ] if *t == "reasoning" && a == "answer"));
    }

    #[test]
    fn walk_transcript_drops_whitespace_only_thinking() {
        let messages = [Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "  \n  ".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::Text {
                    text: "reply".to_owned(),
                },
            ],
        }];
        let out = walk_transcript(&messages);
        assert!(matches!(&out[..], [Interaction::AssistantText(t)] if t == "reply"));
    }

    #[test]
    fn walk_transcript_orphan_result_in_user_only_message() {
        // Entire user message is an orphan — sanitization would normally
        // drop it, but the transform should emit a fallback regardless so
        // the UI path stays robust to corrupted transcripts.
        let messages = [Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "missing".to_owned(),
                content: "stderr".to_owned(),
                is_error: true,
            }],
        }];
        let out = walk_transcript(&messages);
        assert!(matches!(
            &out[..],
            [Interaction::OrphanToolResult {
                content: "stderr",
                is_error: true,
            }]
        ));
    }
}
