//! Conversation message types shared by the client, engine, session transcript, and tools.

use serde::{Deserialize, Serialize};

use crate::model::lacks_progress_updates;

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
    /// A server tool's result, which the API returns in a reply and expects back unchanged.
    ServerToolResult {
        tool_use_id: String,
        /// The API's block type, such as `web_search_tool_result`.
        kind: String,
        /// The block's other fields, such as `content`, which only the API reads.
        #[serde(flatten)]
        fields: serde_json::Map<String, serde_json::Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
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

impl ContentBlock {
    /// The text of a [`ContentBlock::Text`] block, `None` for any other block.
    pub(crate) fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

#[cfg(test)]
impl ContentBlock {
    pub(crate) fn tool_use(id: &str, name: &str, input: serde_json::Value) -> Self {
        Self::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
        }
    }

    pub(crate) fn tool_result(tool_use_id: &str, content: &str, is_error: bool) -> Self {
        Self::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: content.to_owned(),
            is_error,
        }
    }
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

/// The progress note introducing the tool use at `blocks[index]`. See [`progress_note`]. Reasoning
/// from a catalog model that writes no progress updates counts as empty.
pub(crate) fn progress_update_before(blocks: &[ContentBlock], index: usize) -> Option<&str> {
    progress_note(blocks[..index].iter().map(|block| match block {
        ContentBlock::Thinking { replay, .. } if lacks_progress_updates(&replay.model) => Some(""),
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

    const MODEL: &str = "claude-opus-5-5";

    mod content_block {
        use super::*;

        #[test]
        fn server_tool_use_round_trips_through_json() {
            let block = ContentBlock::ServerToolUse {
                id: "stu_01".to_owned(),
                name: "advisor".to_owned(),
                input: serde_json::json!({"query": "test"}),
            };

            let json = serde_json::to_value(&block).unwrap();

            assert_eq!(
                json,
                serde_json::json!({
                    "type": "server_tool_use",
                    "id": "stu_01",
                    "name": "advisor",
                    "input": {"query": "test"},
                }),
            );
            assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
        }

        #[test]
        fn server_tool_result_stores_its_api_type_beside_the_fields_it_keeps() {
            let fields = serde_json::json!({
                "caller": {"type": "direct"},
                "content": [{"type": "web_search_result", "encrypted_content": "opaque"}],
            });
            let block = ContentBlock::ServerToolResult {
                tool_use_id: "stu_01".to_owned(),
                kind: "web_search_tool_result".to_owned(),
                fields: fields.as_object().unwrap().clone(),
            };

            let json = serde_json::to_value(&block).unwrap();

            assert_eq!(
                json,
                serde_json::json!({
                    "type": "server_tool_result",
                    "tool_use_id": "stu_01",
                    "kind": "web_search_tool_result",
                    "caller": {"type": "direct"},
                    "content": [{"type": "web_search_result", "encrypted_content": "opaque"}],
                }),
            );
            assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
        }

        #[test]
        fn tool_result_writes_is_error_only_when_set() {
            for (is_error, expected) in [
                (
                    true,
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": "id",
                        "content": "out",
                        "is_error": true,
                    }),
                ),
                (
                    false,
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": "id",
                        "content": "out",
                    }),
                ),
            ] {
                let block = ContentBlock::ToolResult {
                    tool_use_id: "id".to_owned(),
                    content: "out".to_owned(),
                    is_error,
                };

                let json = serde_json::to_value(&block).unwrap();

                assert_eq!(json, expected);
                assert_eq!(serde_json::from_value::<ContentBlock>(json).unwrap(), block);
            }
        }

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

        mod as_text {
            use super::*;

            #[test]
            fn yields_a_text_blocks_text_and_skips_every_other_block() {
                let blocks = [
                    ContentBlock::Text {
                        text: "hello".to_owned(),
                    },
                    ContentBlock::Thinking {
                        thinking: "reasoning".to_owned(),
                        replay: Replay::with_model(MODEL, "sig"),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "id".to_owned(),
                        content: "out".to_owned(),
                        is_error: false,
                    },
                ];

                let texts: Vec<Option<&str>> = blocks.iter().map(ContentBlock::as_text).collect();

                assert_eq!(texts, [Some("hello"), None, None]);
            }
        }
    }

    mod user {
        use super::*;

        #[test]
        fn creates_user_role_with_text() {
            assert_eq!(
                Message::user("hello"),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "hello".to_owned(),
                    }],
                },
            );
        }
    }

    mod assistant {
        use super::*;

        #[test]
        fn creates_assistant_role_with_text() {
            assert_eq!(
                Message::assistant("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "hi".to_owned(),
                    }],
                },
            );
        }
    }

    mod progress_update_before {
        use super::*;

        fn thinking(text: &str) -> ContentBlock {
            ContentBlock::Thinking {
                thinking: text.to_owned(),
                replay: Replay::with_model(MODEL, "sig"),
            }
        }

        fn tool_use() -> ContentBlock {
            ContentBlock::tool_use("t1", "read", serde_json::json!({}))
        }

        #[test]
        fn finds_note_beside_empty_reasoning() {
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
        fn stops_at_non_thinking_blocks() {
            let text = ContentBlock::Text {
                text: "Checking.".to_owned(),
            };
            for (blocks, index) in [
                (vec![thinking(""), tool_use()], 1),
                (vec![thinking("Found it."), text, tool_use()], 2),
                (vec![tool_use()], 0),
            ] {
                let note = progress_update_before(&blocks, index);

                assert_eq!(note, None, "{blocks:?}");
            }
        }

        #[test]
        fn reads_through_redacted_thinking() {
            let blocks = [
                thinking("note"),
                ContentBlock::RedactedThinking {
                    replay: Replay::with_model(MODEL, "opaque"),
                },
                tool_use(),
            ];

            assert_eq!(progress_update_before(&blocks, 2), Some("note"));
        }

        #[test]
        fn skips_reasoning_from_a_model_without_progress_updates() {
            let haiku = ContentBlock::Thinking {
                thinking: "Summarized reasoning.".to_owned(),
                replay: Replay::with_model("claude-haiku-5-5", "sig"),
            };

            assert_eq!(
                progress_update_before(&[haiku.clone(), tool_use()], 1),
                None
            );
            assert_eq!(
                progress_update_before(&[thinking("note"), haiku, tool_use()], 2),
                Some("note")
            );
        }
    }
}
