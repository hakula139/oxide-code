//! Conversation message types shared by the API client, session transcript, and TUI renderer.

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

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
        #[serde(flatten, serialize_with = "replay_as_signature")]
        replay: Replay,
    },
    /// Reasoning the provider withheld, kept only for its wire to send back.
    RedactedThinking {
        #[serde(flatten, serialize_with = "replay_as_data")]
        replay: Replay,
    },
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

// ── Replay ──

/// The provider wire a content block came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireKind {
    /// The Anthropic Messages API, which every block saved without a wire came from.
    #[default]
    Anthropic,
}

/// What a wire needs to send a reasoning block back. `payload` is opaque to all but `wire`. A
/// session file flattens it into its block with `payload` under the Anthropic key of the block
/// type, the shape that earlier releases read and write.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Replay {
    #[serde(default)]
    pub(crate) wire: WireKind,
    /// The model that produced the block, `None` for a block saved before blocks recorded it.
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(alias = "signature", alias = "data")]
    pub(crate) payload: String,
}

#[cfg(test)]
impl Replay {
    /// An Anthropic replay saved without its model, as blocks from earlier releases load.
    pub(crate) fn without_model(payload: &str) -> Self {
        Self {
            wire: WireKind::Anthropic,
            model: None,
            payload: payload.to_owned(),
        }
    }

    /// An Anthropic replay that `model` produced.
    pub(crate) fn with_model(model: &str, payload: &str) -> Self {
        Self {
            model: Some(model.to_owned()),
            ..Self::without_model(payload)
        }
    }
}

fn replay_as_signature<S: Serializer>(replay: &Replay, serializer: S) -> Result<S::Ok, S::Error> {
    serialize_replay(replay, "signature", serializer)
}

fn replay_as_data<S: Serializer>(replay: &Replay, serializer: S) -> Result<S::Ok, S::Error> {
    serialize_replay(replay, "data", serializer)
}

fn serialize_replay<S: Serializer>(
    replay: &Replay,
    payload_key: &'static str,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut fields = serializer.serialize_struct("Replay", 3)?;
    fields.serialize_field(payload_key, &replay.payload)?;
    fields.serialize_field("wire", &replay.wire)?;
    match &replay.model {
        Some(model) => fields.serialize_field("model", model)?,
        None => fields.skip_field("model")?,
    }
    fields.end()
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
    fn thinking_stores_the_payload_as_its_signature_beside_the_wire_and_model() {
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
                "signature": "sig_abc",
                "wire": "anthropic",
                "model": "claude-opus-5-5",
            }),
        );
        assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
    }

    #[test]
    fn thinking_saved_without_a_wire_or_model_loads_as_anthropic_without_a_model() {
        let json = serde_json::json!({"type": "thinking", "thinking": "r", "signature": "sig"});

        let block: ContentBlock = serde_json::from_value(json).unwrap();

        assert_eq!(
            block,
            ContentBlock::Thinking {
                thinking: "r".to_owned(),
                replay: Replay::without_model("sig"),
            },
        );
    }

    // ── ContentBlock::RedactedThinking ──

    #[test]
    fn redacted_thinking_stores_the_payload_as_its_data_beside_the_wire_and_model() {
        let block = ContentBlock::RedactedThinking {
            replay: Replay::with_model(MODEL, "base64data=="),
        };

        let json = serde_json::to_value(&block).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "type": "redacted_thinking",
                "data": "base64data==",
                "wire": "anthropic",
                "model": "claude-opus-5-5",
            }),
        );
        assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
    }

    #[test]
    fn redacted_thinking_saved_without_a_wire_or_model_loads_as_anthropic_without_a_model() {
        let json = serde_json::json!({"type": "redacted_thinking", "data": "base64data=="});

        let block: ContentBlock = serde_json::from_value(json).unwrap();

        assert_eq!(
            block,
            ContentBlock::RedactedThinking {
                replay: Replay::without_model("base64data=="),
            },
        );
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
            replay: Replay::without_model("sig"),
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
                replay: Replay::without_model("opaque"),
            },
            tool_use(),
        ];
        assert_eq!(progress_update_before(&blocks, 2), Some("note"));
    }
}
