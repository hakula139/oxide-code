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

/// One entry in a [`Message::content`] vector. Variants follow the Anthropic Messages API
/// content-block taxonomy, in the shape session files store. Each wire sends blocks in its own
/// request shape, and a reasoning block carries a [`Replay`] that only its own wire reads.
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
    /// Reasoning the model shows, empty when the request omits it.
    Thinking {
        thinking: String,
        #[serde(flatten)]
        replay: Replay,
    },
    /// Reasoning the provider withheld, kept only for its wire to send back.
    RedactedThinking {
        #[serde(flatten)]
        replay: Replay,
    },
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

// ── Replay ──

/// The provider wire a content block came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireKind {
    /// The Anthropic Messages API.
    Anthropic,
}

/// What a wire needs to send a reasoning block back. `payload` is opaque to all but `wire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Replay {
    pub(crate) wire: WireKind,
    /// The model that produced the block.
    pub(crate) model: String,
    pub(crate) payload: String,
}

#[cfg(test)]
impl Replay {
    /// An Anthropic replay that `model` produced.
    pub(crate) fn with_model(model: &str, payload: &str) -> Self {
        Self {
            wire: WireKind::Anthropic,
            model: model.to_owned(),
            payload: payload.to_owned(),
        }
    }
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

    /// The user message that ends a turn the user interrupted, so the model reads the prompts
    /// before it as abandoned.
    pub(crate) fn interruption() -> Self {
        Self::user(INTERRUPTION_MARKER)
    }
}

/// Text of [`Message::interruption`], the one Claude Code records for a cancel mid-stream. Session
/// files store it and resume matches it verbatim, so new text would show older markers as prompts.
pub(crate) const INTERRUPTION_MARKER: &str = "[Request interrupted by user]";

// ── Progress Updates ──

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

    const MODEL: &str = "claude-opus-5-5";

    #[test]
    fn thinking_stores_its_replay_beside_the_thinking_text() {
        let block = ContentBlock::Thinking {
            thinking: "reasoning".to_owned(),
            replay: Replay::with_model(MODEL, "sig_abc"),
        };

        let json = serde_json::to_value(&block).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "type": "thinking",
                "thinking": "reasoning",
                "wire": "anthropic",
                "model": "claude-opus-5-5",
                "payload": "sig_abc",
            }),
        );
        assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
    }

    #[test]
    fn thinking_without_a_full_replay_fails_to_load() {
        let cases = [
            (
                serde_json::json!({"type": "thinking", "thinking": "r", "signature": "sig"}),
                "missing field `wire`",
            ),
            (
                serde_json::json!({
                    "type": "thinking",
                    "thinking": "r",
                    "wire": "anthropic",
                    "payload": "sig",
                }),
                "missing field `model`",
            ),
            (
                serde_json::json!({
                    "type": "thinking",
                    "thinking": "r",
                    "signature": "sig",
                    "wire": "anthropic",
                    "model": MODEL,
                }),
                "missing field `payload`",
            ),
        ];

        for (json, expected) in cases {
            let err = serde_json::from_value::<ContentBlock>(json.clone()).unwrap_err();

            assert_eq!(err.to_string(), expected, "{json}");
        }
    }

    // ── ContentBlock::RedactedThinking ──

    #[test]
    fn redacted_thinking_stores_its_replay_alone() {
        let block = ContentBlock::RedactedThinking {
            replay: Replay::with_model(MODEL, "base64data=="),
        };

        let json = serde_json::to_value(&block).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "type": "redacted_thinking",
                "wire": "anthropic",
                "model": "claude-opus-5-5",
                "payload": "base64data==",
            }),
        );
        assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
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
            replay: Replay::with_model(MODEL, "sig"),
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
                replay: Replay::with_model(MODEL, "opaque"),
            },
            tool_use(),
        ];
        assert_eq!(progress_update_before(&blocks, 2), Some("note"));
    }
}
