//! Conversation message types shared by the API client, session transcript, and TUI renderer.

use serde::{Deserialize, Serialize};

// ── Role ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    User,
    Assistant,
}

// ── ContentBlock ──

/// One entry in a [`Message::content`] vector. Variants mirror the Anthropic Messages API
/// content-block taxonomy verbatim so the same struct serves the wire format, the JSONL
/// session transcript, and the TUI renderer without lossy conversions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_default")]
        is_error: bool,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Opaque safety-redacted block; must round-trip verbatim or the API rejects it.
    RedactedThinking {
        data: String,
    },
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

// ── Message ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Message {
    pub(crate) role: Role,
    pub(crate) content: Vec<ContentBlock>,
}

impl Message {
    pub(crate) fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub(crate) fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

// ── Progress updates ──

/// The progress note introducing the tool use at `blocks[index]`. See [`progress_note`].
pub(crate) fn progress_update_before(blocks: &[ContentBlock], index: usize) -> Option<&str> {
    progress_note(blocks[..index].iter().map(|block| match block {
        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
        ContentBlock::RedactedThinking { .. } => Some(""),
        _ => None,
    }))
}

/// Picks the progress note from the blocks before a tool use, given in order as each block's
/// thinking text (`""` for redacted thinking, `None` for any other block): the non-empty text in
/// the run of thinking blocks directly before the tool use. Under `thinking.display = "updates"`
/// reasoning comes back empty, so that text is the note.
pub(crate) fn progress_note<'a>(
    thinking: impl DoubleEndedIterator<Item = Option<&'a str>>,
) -> Option<&'a str> {
    thinking
        .rev()
        .map_while(|text| text)
        .find(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ContentBlock::ServerToolUse ──

    #[test]
    fn server_tool_use_round_trips_through_json() {
        let block = ContentBlock::ServerToolUse {
            id: "stu_01".to_owned(),
            name: "advisor".to_owned(),
            input: serde_json::json!({"query": "test"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "server_tool_use");
        assert_eq!(json["id"], "stu_01");
        assert_eq!(json["name"], "advisor");

        let deserialized: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::ServerToolUse { id, name, input } = deserialized else {
            panic!("expected ServerToolUse");
        };
        assert_eq!(id, "stu_01");
        assert_eq!(name, "advisor");
        assert_eq!(input["query"], "test");
    }

    // ── ContentBlock::ToolResult ──

    #[test]
    fn tool_result_serializes_is_error_when_true() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id".to_owned(),
            content: "error msg".to_owned(),
            is_error: true,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "id");
        assert_eq!(json["content"], "error msg");
        assert_eq!(json["is_error"], true);
    }

    #[test]
    fn tool_result_omits_is_error_when_false() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id".to_owned(),
            content: "ok".to_owned(),
            is_error: false,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert!(json.get("is_error").is_none());
    }

    #[test]
    fn tool_result_deserializes_missing_is_error_as_false() {
        let json = serde_json::json!({
            "type": "tool_result",
            "tool_use_id": "id",
            "content": "ok"
        });
        let block: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        else {
            panic!("expected ToolResult");
        };
        assert_eq!(tool_use_id, "id");
        assert_eq!(content, "ok");
        assert!(!is_error);
    }

    // ── ContentBlock::Thinking ──

    #[test]
    fn thinking_round_trips_through_json() {
        let block = ContentBlock::Thinking {
            thinking: "reasoning".to_owned(),
            signature: "sig_abc".to_owned(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["thinking"], "reasoning");
        assert_eq!(json["signature"], "sig_abc");

        let deserialized: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::Thinking {
            thinking,
            signature,
        } = deserialized
        else {
            panic!("expected Thinking");
        };
        assert_eq!(thinking, "reasoning");
        assert_eq!(signature, "sig_abc");
    }

    // ── ContentBlock::RedactedThinking ──

    #[test]
    fn redacted_thinking_round_trips_through_json() {
        let block = ContentBlock::RedactedThinking {
            data: "base64data==".to_owned(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "redacted_thinking");
        assert_eq!(json["data"], "base64data==");

        let deserialized: ContentBlock = serde_json::from_value(json).unwrap();
        let ContentBlock::RedactedThinking { data } = deserialized else {
            panic!("expected RedactedThinking");
        };
        assert_eq!(data, "base64data==");
    }

    // ── Message::user ──

    #[test]
    fn user_creates_user_role_with_text() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "hello"));
    }

    // ── Message::assistant ──

    #[test]
    fn assistant_creates_assistant_role_with_text() {
        let msg = Message::assistant("hi");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "hi"));
    }

    // ── progress_update_before ──

    fn thinking(text: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: text.to_owned(),
            signature: "sig".to_owned(),
        }
    }

    fn tool_use() -> ContentBlock {
        ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "read".to_owned(),
            input: serde_json::json!({}),
        }
    }

    #[test]
    fn progress_update_before_finds_note_beside_empty_reasoning() {
        for blocks in [
            [thinking(""), thinking("Found the loader."), tool_use()],
            [thinking("Found the loader."), thinking("  "), tool_use()],
        ] {
            assert_eq!(
                progress_update_before(&blocks, 2),
                Some("Found the loader.")
            );
        }
    }

    #[test]
    fn progress_update_before_stops_at_non_thinking_blocks() {
        let empty = [thinking(""), tool_use()];
        assert_eq!(progress_update_before(&empty, 1), None);

        let text = [
            thinking("Found it."),
            ContentBlock::Text {
                text: "Checking.".to_owned(),
            },
            tool_use(),
        ];
        assert_eq!(progress_update_before(&text, 2), None);

        assert_eq!(progress_update_before(&[tool_use()], 0), None);
    }

    #[test]
    fn progress_update_before_reads_through_redacted_thinking() {
        let blocks = [
            thinking("note"),
            ContentBlock::RedactedThinking {
                data: "opaque".to_owned(),
            },
            tool_use(),
        ];
        assert_eq!(progress_update_before(&blocks, 2), Some("note"));
    }
}
