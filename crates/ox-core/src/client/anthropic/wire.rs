//! Serde types for Anthropic Messages API requests, streamed response frames, and error bodies.

use serde::ser::SerializeSeq as _;
use serde::{Deserialize, Serialize, Serializer};

use crate::client::{StopDetails, StopReason};
use crate::message::{ContentBlock, Message, Role, WireKind};
use crate::model::{Effort, ModelSpec, PromptCacheTtl, TokenUsage};
use crate::tool::ToolDefinition;

// ── Request Types ──

/// Body for `POST /v1/messages`, whose fields serialize in declaration order (see `system`).
#[derive(Serialize)]
pub(super) struct CreateMessageRequest<'a> {
    pub(super) model: &'a str,
    pub(super) max_tokens: u32,
    pub(super) stream: bool,
    pub(super) metadata: RequestMetadata,
    /// Precedes `messages` because [`super::billing::inject_cch`] replaces only the first
    /// `cch=00000`, which must be the billing placeholder even when user content echoes it.
    pub(super) system: Vec<SystemBlock<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) thinking: Option<&'a ThinkingConfig>,
    /// A side query's JSON-schema `format` and the `effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_config: Option<OutputConfig<'a>>,
    /// Partners the `context-management-2025-06-27` beta on streaming requests for known models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) context_management: Option<ContextManagement>,
    pub(super) messages: RequestMessages<'a>,
}

/// Serializes `messages` in the request shape. A user message holding `user_context` leads when
/// set, and `breakpoint` marks the last content block of the final message. Both exist only on
/// the wire, so the transcript never stores the context or cache state.
pub(super) struct RequestMessages<'a> {
    pub(super) user_context: Option<&'a str>,
    pub(super) messages: &'a [Message],
    pub(super) breakpoint: Option<CacheControl>,
}

impl Serialize for RequestMessages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let messages: Vec<RequestMessage<'_>> = self
            .user_context
            .map(RequestMessage::user_context)
            .into_iter()
            .chain(self.messages.iter().map(RequestMessage::of))
            .collect();
        let Some(cache_control) = &self.breakpoint else {
            return messages.serialize(serializer);
        };
        serialize_marking_last(serializer, &messages, |message| MarkedMessage {
            role: message.role,
            content: MarkedBlocks {
                blocks: &message.content,
                cache_control,
            },
        })
    }
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: Role,
    content: Vec<RequestBlock<'a>>,
}

impl<'a> RequestMessage<'a> {
    fn user_context(text: &'a str) -> Self {
        Self {
            role: Role::User,
            content: vec![RequestBlock::Text { text }],
        }
    }

    fn of(message: &'a Message) -> Self {
        Self {
            role: message.role,
            content: message.content.iter().map(RequestBlock::of).collect(),
        }
    }
}

/// A content block in the request shape, where reasoning carries its payload in the field the API
/// returned it in, and a server tool result takes back its API block type.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestBlock<'a> {
    Text {
        text: &'a str,
    },
    Thinking {
        thinking: &'a str,
        signature: &'a str,
    },
    RedactedThinking {
        data: &'a str,
    },
    #[serde(untagged)]
    ServerToolResult {
        r#type: &'a str,
        tool_use_id: &'a str,
        #[serde(flatten)]
        fields: &'a serde_json::Map<String, serde_json::Value>,
    },
    #[serde(untagged)]
    Verbatim(&'a ContentBlock),
}

impl<'a> RequestBlock<'a> {
    /// The shape `block` takes in a request. Reasoning goes back unchanged whichever model
    /// produced it, since the API drops or ignores the blocks the request's model cannot read.
    fn of(block: &'a ContentBlock) -> Self {
        match block {
            ContentBlock::Thinking { thinking, replay } => match replay.wire {
                WireKind::Anthropic => Self::Thinking {
                    thinking,
                    signature: &replay.payload,
                },
            },
            ContentBlock::RedactedThinking { replay } => match replay.wire {
                WireKind::Anthropic => Self::RedactedThinking {
                    data: &replay.payload,
                },
            },
            ContentBlock::ServerToolResult {
                tool_use_id,
                kind,
                fields,
            } => Self::ServerToolResult {
                r#type: kind,
                tool_use_id,
                fields,
            },
            block => Self::Verbatim(block),
        }
    }
}

#[derive(Serialize)]
struct MarkedMessage<'a> {
    role: Role,
    content: MarkedBlocks<'a>,
}

struct MarkedBlocks<'a> {
    blocks: &'a [RequestBlock<'a>],
    cache_control: &'a CacheControl,
}

impl Serialize for MarkedBlocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_marking_last(serializer, self.blocks, |block| MarkedBlock {
            block,
            cache_control: self.cache_control,
        })
    }
}

#[derive(Serialize)]
struct MarkedBlock<'a> {
    #[serde(flatten)]
    block: &'a RequestBlock<'a>,
    cache_control: &'a CacheControl,
}

/// Serializes `items` as a sequence with the last element replaced by `mark(last)`.
fn serialize_marking_last<'a, S, T, M>(
    serializer: S,
    items: &'a [T],
    mark: impl FnOnce(&'a T) -> M,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
    M: Serialize,
{
    let mut seq = serializer.serialize_seq(Some(items.len()))?;
    if let Some((last, earlier)) = items.split_last() {
        for item in earlier {
            seq.serialize_element(item)?;
        }
        seq.serialize_element(&mark(last))?;
    }
    seq.end()
}

#[derive(Serialize)]
pub(super) struct OutputConfig<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a OutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<Effort>,
}

impl<'a> OutputConfig<'a> {
    /// `None` when neither field is set.
    pub(super) fn new(format: Option<&'a OutputFormat>, effort: Option<Effort>) -> Option<Self> {
        (format.is_some() || effort.is_some()).then_some(Self { format, effort })
    }
}

/// One `clear_thinking_20251015` edit with `keep = "all"`, the directive Claude Code sends.
#[derive(Serialize)]
pub(super) struct ContextManagement {
    edits: [ContextEdit; 1],
}

impl ContextManagement {
    pub(super) fn clear_thinking_keep_all() -> Self {
        Self {
            edits: [ContextEdit { keep: "all" }],
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename = "clear_thinking_20251015")]
struct ContextEdit {
    keep: &'static str,
}

/// JSON-schema-constrained completion format.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "json_schema")]
pub(crate) struct OutputFormat {
    schema: serde_json::Value,
}

impl OutputFormat {
    pub(crate) fn json_schema(schema: serde_json::Value) -> Self {
        Self { schema }
    }
}

/// `user_id` is a stringified JSON object `{device_id, account_uuid, session_id}`.
#[derive(Serialize)]
pub(super) struct RequestMetadata {
    pub(super) user_id: String,
}

/// Array form is required so the identity prefix occupies its own block (OAuth non-Haiku gate).
#[derive(Serialize)]
#[serde(tag = "type", rename = "text")]
pub(super) struct SystemBlock<'a> {
    pub(super) text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_control: Option<CacheControl>,
}

/// An ephemeral prompt-cache breakpoint. `scope: "global"` shares the cache across sessions, and
/// `ttl` overrides the API's 5-minute lifetime.
#[derive(Clone, Copy, Serialize)]
#[serde(tag = "type", rename = "ephemeral")]
pub(super) struct CacheControl {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ttl: Option<&'static str>,
}

impl CacheControl {
    /// The breakpoint on the static system prefix. Claude Code sends `scope: "global"` only to the
    /// Anthropic API.
    pub(super) fn static_prefix(is_first_party: bool, ttl: PromptCacheTtl) -> Self {
        Self {
            scope: is_first_party.then_some("global"),
            ..Self::conversation(ttl)
        }
    }

    /// The breakpoint on the conversation's final block. The transcript is session-specific, so it
    /// never carries `scope`.
    pub(super) fn conversation(ttl: PromptCacheTtl) -> Self {
        Self {
            scope: None,
            ttl: match ttl {
                PromptCacheTtl::FiveMin => None,
                PromptCacheTtl::OneHour => Some(ttl.into()),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ThinkingConfig {
    /// Model decides the thinking budget.
    Adaptive {
        display: ThinkingDisplay,
        block_binding: BlockBinding,
    },
}

impl ThinkingConfig {
    /// Adaptive thinking for known models, dropping replayed blocks whose conversation prefix
    /// changed. `show_thinking` opts into summarized thinking text, otherwise models with progress
    /// updates return only those, and other models return none. `None` for unknown IDs, whose
    /// upstream may reject the field.
    pub(super) fn for_model(model: &ModelSpec, show_thinking: bool) -> Option<Self> {
        model.info().map(|info| Self::Adaptive {
            display: if show_thinking {
                ThinkingDisplay::Summarized
            } else if info.progress_updates {
                ThinkingDisplay::Updates
            } else {
                ThinkingDisplay::Omitted
            },
            block_binding: BlockBinding::DropBlock,
        })
    }

    /// Returns whether the request asks for thinking text, as summaries or progress notes.
    pub(super) fn asks_for_text(&self) -> bool {
        match self {
            Self::Adaptive { display, .. } => *display != ThinkingDisplay::Omitted,
        }
    }
}

/// `thinking.display` values, always sent so a reply carries only the thinking text ox renders,
/// whatever an upstream's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ThinkingDisplay {
    Summarized,
    /// Only the progress notes written between tool calls come back as text. Requires the
    /// `thinking-display-updates-2026-08-18` beta.
    Updates,
    /// No thinking text comes back, and the server can skip streaming it.
    Omitted,
}

/// `thinking.block_binding`. `DropBlock` makes the API drop a replayed thinking block whose
/// conversation prefix changed since it was produced, plus every later one, where it would
/// otherwise reject the request. Requires the `thinking-binding-controls-2026-08-01` beta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "prefix_mismatch_behavior", rename_all = "snake_case")]
pub(super) enum BlockBinding {
    DropBlock,
}

// ── SSE Response Types ──

/// One frame in the streaming response. `Unknown` swallows future event types so an upstream
/// addition does not break parsing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum StreamEvent {
    MessageStart {
        message: MessageResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockInfo,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop,
    MessageDelta {
        delta: MessageDeltaBody,
        usage: Option<TokenUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct MessageResponse {
    pub(super) usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ContentBlockInfo {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    ServerToolUse {
        id: String,
        name: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// An MCP connector result, skipped along with the `mcp_tool_use` call it answers. The call
    /// reads as unknown, so a reply never keeps one without the other.
    McpToolResult {},
    /// A server tool's result, such as `web_search_tool_result`, which streams no deltas. Any
    /// other block type carrying a `tool_use_id` reads as one.
    #[serde(untagged)]
    ServerToolResult {
        r#type: String,
        tool_use_id: String,
        #[serde(flatten)]
        fields: serde_json::Map<String, serde_json::Value>,
    },
    /// Serde tries the untagged variants after a tagged one fails, so a malformed known block
    /// lands here too. `type` is raw JSON because such a block may carry any.
    #[serde(untagged)]
    Unknown {
        #[serde(default)]
        r#type: serde_json::Value,
    },
}

#[expect(
    clippy::enum_variant_names,
    reason = "variant names mirror Anthropic API delta type values \
              (text_delta, input_json_delta, etc.)"
)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct MessageDeltaBody {
    pub(super) stop_reason: Option<StopReason>,
    /// Populated only on `refusal`.
    #[serde(default)]
    pub(super) stop_details: Option<StopDetails>,
}

// ── Error Types ──

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct ApiError {
    #[serde(rename = "type")]
    pub(super) error_type: String,
    pub(super) message: String,
}

/// Body of a non-2xx response.
#[derive(Debug, Deserialize)]
pub(super) struct ErrorResponse {
    pub(super) error: ApiError,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::message::Replay;

    mod request_messages {
        use super::*;

        #[test]
        fn breakpoint_marks_only_last_block_of_final_message() {
            let messages = [
                Message::user("question"),
                Message::assistant("answer"),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "toolu_1".to_owned(),
                            content: "ok".to_owned(),
                            is_error: false,
                        },
                        ContentBlock::Text {
                            text: "follow-up".to_owned(),
                        },
                    ],
                },
            ];
            let v = serde_json::to_value(RequestMessages {
                user_context: None,
                messages: &messages,
                breakpoint: Some(CacheControl::conversation(PromptCacheTtl::OneHour)),
            })
            .unwrap();

            assert_eq!(
                v,
                json!([
                    {"role": "user", "content": [{"type": "text", "text": "question"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "answer"}]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
                        {
                            "type": "text",
                            "text": "follow-up",
                            "cache_control": {"type": "ephemeral", "ttl": "1h"},
                        },
                    ]},
                ]),
            );
        }

        #[test]
        fn breakpoint_keeps_tool_result_fields_on_marked_block() {
            let messages = [Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".to_owned(),
                    content: "boom".to_owned(),
                    is_error: true,
                }],
            }];
            let v = serde_json::to_value(RequestMessages {
                user_context: None,
                messages: &messages,
                breakpoint: Some(CacheControl::conversation(PromptCacheTtl::OneHour)),
            })
            .unwrap();

            assert_eq!(
                v[0]["content"][0],
                json!({
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "boom",
                    "is_error": true,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                }),
            );
        }

        #[test]
        fn without_breakpoint_or_messages_serializes_verbatim() {
            let messages = [Message::user("question"), Message::assistant("answer")];
            let v = serde_json::to_value(RequestMessages {
                user_context: None,
                messages: &messages,
                breakpoint: None,
            })
            .unwrap();
            assert_eq!(v, serde_json::to_value(messages).unwrap());

            let empty = serde_json::to_value(RequestMessages {
                user_context: None,
                messages: &[],
                breakpoint: Some(CacheControl::conversation(PromptCacheTtl::OneHour)),
            })
            .unwrap();
            assert_eq!(empty, json!([]));
        }

        #[test]
        fn lead_with_the_user_context() {
            let text = |text: &str| json!({"type": "text", "text": text});
            let marked = |text: &str| {
                json!({
                    "type": "text",
                    "text": text,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                })
            };
            for (messages, expected) in [
                (
                    vec![Message::user("question")],
                    json!([
                        {"role": "user", "content": [text("context")]},
                        {"role": "user", "content": [marked("question")]},
                    ]),
                ),
                (
                    Vec::new(),
                    json!([{"role": "user", "content": [marked("context")]}]),
                ),
            ] {
                let v = serde_json::to_value(RequestMessages {
                    user_context: Some("context"),
                    messages: &messages,
                    breakpoint: Some(CacheControl::conversation(PromptCacheTtl::OneHour)),
                })
                .unwrap();

                assert_eq!(v, expected);
            }
        }

        #[test]
        fn send_reasoning_back_under_its_api_keys_whatever_model_wrote_it() {
            let messages = [
                Message::user("question"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "plan".to_owned(),
                            replay: Replay::with_model("claude-haiku-5-5", "sig"),
                        },
                        ContentBlock::RedactedThinking {
                            replay: Replay::with_model("claude-haiku-5-5", "blob"),
                        },
                        ContentBlock::Text {
                            text: "answer".to_owned(),
                        },
                    ],
                },
                Message::user("next"),
            ];

            let v = serde_json::to_value(RequestMessages {
                user_context: None,
                messages: &messages,
                breakpoint: None,
            })
            .unwrap();

            assert_eq!(
                v,
                json!([
                    {"role": "user", "content": [{"type": "text", "text": "question"}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "plan", "signature": "sig"},
                        {"type": "redacted_thinking", "data": "blob"},
                        {"type": "text", "text": "answer"},
                    ]},
                    {"role": "user", "content": [{"type": "text", "text": "next"}]},
                ]),
            );
        }

        #[test]
        fn send_server_tool_results_back_as_the_api_returned_them() {
            let server_result = |tool_use_id: &str, kind: &str, content: serde_json::Value| {
                ContentBlock::ServerToolResult {
                    tool_use_id: tool_use_id.to_owned(),
                    kind: kind.to_owned(),
                    fields: serde_json::Map::from_iter([("content".to_owned(), content)]),
                }
            };
            let messages = [
                Message::user("question"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        server_result(
                            "stu_1",
                            "web_search_tool_result",
                            json!([{"type": "web_search_result", "encrypted_content": "opaque"}]),
                        ),
                        ContentBlock::Text {
                            text: "answer".to_owned(),
                        },
                    ],
                },
                Message::user("next"),
                Message {
                    role: Role::Assistant,
                    content: vec![server_result(
                        "stu_2",
                        "web_fetch_tool_result",
                        json!({"type": "web_fetch_tool_error", "error_code": "unavailable"}),
                    )],
                },
            ];

            let v = serde_json::to_value(RequestMessages {
                user_context: None,
                messages: &messages,
                breakpoint: Some(CacheControl::conversation(PromptCacheTtl::OneHour)),
            })
            .unwrap();

            assert_eq!(
                v[1]["content"][0],
                json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": "stu_1",
                    "content": [{"type": "web_search_result", "encrypted_content": "opaque"}],
                }),
            );
            assert_eq!(
                v[3]["content"][0],
                json!({
                    "type": "web_fetch_tool_result",
                    "tool_use_id": "stu_2",
                    "content": {"type": "web_fetch_tool_error", "error_code": "unavailable"},
                    "cache_control": {"type": "ephemeral", "ttl": "1h"},
                }),
            );
        }
    }

    mod context_management {
        use super::*;

        mod clear_thinking_keep_all {
            use super::*;

            #[test]
            fn serializes_tagged_shape() {
                let v = serde_json::to_value(ContextManagement::clear_thinking_keep_all()).unwrap();

                assert_eq!(
                    v,
                    json!({
                        "edits": [{"type": "clear_thinking_20251015", "keep": "all"}],
                    }),
                );
            }
        }
    }

    mod output_format {
        use super::*;

        mod json_schema {
            use super::*;

            #[test]
            fn serializes_with_type_and_schema() {
                let schema = json!({
                    "type": "object",
                    "properties": {"title": {"type": "string"}},
                    "required": ["title"],
                });
                let fmt = OutputFormat::json_schema(schema.clone());

                let v = serde_json::to_value(&fmt).unwrap();

                assert_eq!(v, json!({"type": "json_schema", "schema": schema}));
            }
        }
    }

    mod cache_control {
        use super::*;

        mod static_prefix {
            use super::*;

            #[test]
            fn shares_the_cache_globally_on_first_party_only() {
                for (is_first_party, ttl, expected) in [
                    (
                        true,
                        PromptCacheTtl::OneHour,
                        r#"{"type":"ephemeral","scope":"global","ttl":"1h"}"#,
                    ),
                    (
                        false,
                        PromptCacheTtl::OneHour,
                        r#"{"type":"ephemeral","ttl":"1h"}"#,
                    ),
                    (false, PromptCacheTtl::FiveMin, r#"{"type":"ephemeral"}"#),
                ] {
                    let cache = CacheControl::static_prefix(is_first_party, ttl);
                    assert_eq!(serde_json::to_string(&cache).unwrap(), expected);
                }
            }
        }

        mod conversation {
            use super::*;

            #[test]
            fn carries_the_ttl_without_scope_and_omits_the_five_minute_default() {
                for (ttl, expected) in [
                    (
                        PromptCacheTtl::OneHour,
                        r#"{"type":"ephemeral","ttl":"1h"}"#,
                    ),
                    (PromptCacheTtl::FiveMin, r#"{"type":"ephemeral"}"#),
                ] {
                    let cache = CacheControl::conversation(ttl);
                    assert_eq!(serde_json::to_string(&cache).unwrap(), expected);
                }
            }
        }
    }

    mod thinking_config {
        use super::*;

        mod for_model {
            use super::*;

            #[test]
            fn known_model_is_adaptive_with_display_following_show_thinking() {
                let drop_block = json!({"prefix_mismatch_behavior": "drop_block"});
                for (id, hidden_display) in [
                    ("claude-fable-5-1", "updates"),
                    ("claude-opus-5-5", "updates"),
                    ("claude-sonnet-5-5", "updates"),
                    ("claude-haiku-5-5", "omitted"),
                ] {
                    let model = ModelSpec::resolve(id);
                    assert_eq!(
                        serde_json::to_value(ThinkingConfig::for_model(&model, true)).unwrap(),
                        json!({
                            "type": "adaptive",
                            "display": "summarized",
                            "block_binding": drop_block,
                        }),
                        "{id}",
                    );
                    assert_eq!(
                        serde_json::to_value(ThinkingConfig::for_model(&model, false)).unwrap(),
                        json!({
                            "type": "adaptive",
                            "display": hidden_display,
                            "block_binding": drop_block,
                        }),
                        "{id}"
                    );
                }
            }

            #[test]
            fn omits_thinking_for_an_unknown_model() {
                assert!(
                    ThinkingConfig::for_model(&ModelSpec::resolve("custom-model"), true).is_none()
                );
            }
        }
    }

    mod stream_event {
        use super::*;

        #[test]
        fn content_block_start_carries_its_text_block() {
            let json = r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;

            let event: StreamEvent = serde_json::from_str(json).unwrap();

            assert_eq!(
                event,
                StreamEvent::ContentBlockStart {
                    index: 0,
                    content_block: ContentBlockInfo::Text {
                        text: String::new(),
                    },
                }
            );
        }

        #[test]
        fn content_block_stop_drops_its_index() {
            let event: StreamEvent =
                serde_json::from_str(r#"{"type":"content_block_stop","index":2}"#).unwrap();

            assert_eq!(event, StreamEvent::ContentBlockStop);
        }

        #[test]
        fn message_delta_carries_the_stop_reason_and_usage() {
            let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;

            let event: StreamEvent = serde_json::from_str(json).unwrap();

            assert_eq!(
                event,
                StreamEvent::MessageDelta {
                    delta: MessageDeltaBody {
                        stop_reason: Some(StopReason::EndTurn),
                        stop_details: None,
                    },
                    usage: Some(TokenUsage::new(0, 42)),
                }
            );
        }

        #[test]
        fn message_delta_refusal_carries_stop_details() {
            let json = r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"Exploit development"}},"usage":{"output_tokens":3}}"#;

            let event: StreamEvent = serde_json::from_str(json).unwrap();

            assert_eq!(
                event,
                StreamEvent::MessageDelta {
                    delta: MessageDeltaBody {
                        stop_reason: Some(StopReason::Refusal),
                        stop_details: Some(StopDetails {
                            category: Some("cyber".to_owned()),
                            explanation: Some("Exploit development".to_owned()),
                        }),
                    },
                    usage: Some(TokenUsage::new(0, 3)),
                }
            );
        }

        #[test]
        fn message_delta_unknown_stop_reason_keeps_usage() {
            let json = r#"{"type":"message_delta","delta":{"stop_reason":"some_future_reason","stop_details":null},"usage":{"output_tokens":7}}"#;

            let event: StreamEvent = serde_json::from_str(json).unwrap();

            assert_eq!(
                event,
                StreamEvent::MessageDelta {
                    delta: MessageDeltaBody {
                        stop_reason: Some(StopReason::Unknown),
                        stop_details: None,
                    },
                    usage: Some(TokenUsage::new(0, 7)),
                }
            );
        }

        #[test]
        fn message_stop_parses_from_its_type_alone() {
            let event: StreamEvent = serde_json::from_str(r#"{"type":"message_stop"}"#).unwrap();

            assert_eq!(event, StreamEvent::MessageStop);
        }
    }

    mod content_block_info {
        use super::*;

        #[test]
        fn text_keeps_its_text() {
            let info: ContentBlockInfo =
                serde_json::from_str(r#"{"type":"text","text":"Hello world"}"#).unwrap();

            assert_eq!(
                info,
                ContentBlockInfo::Text {
                    text: "Hello world".to_owned(),
                }
            );
        }

        #[test]
        fn tool_use_keeps_its_id_and_name() {
            let info: ContentBlockInfo =
                serde_json::from_str(r#"{"type":"tool_use","id":"toolu_01","name":"bash"}"#)
                    .unwrap();

            assert_eq!(
                info,
                ContentBlockInfo::ToolUse {
                    id: "toolu_01".to_owned(),
                    name: "bash".to_owned(),
                }
            );
        }

        #[test]
        fn server_tool_use_keeps_its_id_and_name() {
            let info: ContentBlockInfo = serde_json::from_str(
                r#"{"type":"server_tool_use","id":"stu_01","name":"advisor"}"#,
            )
            .unwrap();

            assert_eq!(
                info,
                ContentBlockInfo::ServerToolUse {
                    id: "stu_01".to_owned(),
                    name: "advisor".to_owned(),
                }
            );
        }

        #[test]
        fn thinking_keeps_its_text_and_signature() {
            let json =
                r#"{"type":"thinking","thinking":"Let me analyze this","signature":"sig_xyz"}"#;

            let info: ContentBlockInfo = serde_json::from_str(json).unwrap();

            assert_eq!(
                info,
                ContentBlockInfo::Thinking {
                    thinking: "Let me analyze this".to_owned(),
                    signature: "sig_xyz".to_owned(),
                }
            );
        }

        #[test]
        fn redacted_thinking_keeps_its_data() {
            let info: ContentBlockInfo =
                serde_json::from_str(r#"{"type":"redacted_thinking","data":"base64data=="}"#)
                    .unwrap();

            assert_eq!(
                info,
                ContentBlockInfo::RedactedThinking {
                    data: "base64data==".to_owned(),
                }
            );
        }

        #[test]
        fn server_tool_result_keeps_its_type_and_fields() {
            let json = r#"{"type":"advisor_tool_result","tool_use_id":"stu_1","content":{"type":"advisor_redacted_result","encrypted_content":"opaque"}}"#;

            let info: ContentBlockInfo = serde_json::from_str(json).unwrap();

            assert_eq!(
                info,
                ContentBlockInfo::ServerToolResult {
                    r#type: "advisor_tool_result".to_owned(),
                    tool_use_id: "stu_1".to_owned(),
                    fields: serde_json::Map::from_iter([(
                        "content".to_owned(),
                        json!({"type": "advisor_redacted_result", "encrypted_content": "opaque"}),
                    )]),
                }
            );
        }

        #[test]
        fn mcp_tool_result_is_no_server_tool_result() {
            let json = r#"{"type":"mcp_tool_result","tool_use_id":"m1","content":[]}"#;

            let info: ContentBlockInfo = serde_json::from_str(json).unwrap();

            assert_eq!(info, ContentBlockInfo::McpToolResult {});
        }

        #[test]
        fn unknown_or_malformed_block_keeps_its_type() {
            for (json, expected) in [
                (
                    r#"{"type":"some_future_block","data":"opaque"}"#,
                    json!("some_future_block"),
                ),
                (r#"{"type":"tool_use","id":"toolu_01"}"#, json!("tool_use")),
            ] {
                let info: ContentBlockInfo = serde_json::from_str(json).unwrap();

                assert_eq!(
                    info,
                    ContentBlockInfo::Unknown { r#type: expected },
                    "{json}"
                );
            }
        }
    }

    mod delta {
        use super::*;

        #[test]
        fn text_delta_keeps_its_text() {
            let delta: Delta =
                serde_json::from_str(r#"{"type":"text_delta","text":"Hello"}"#).unwrap();

            assert_eq!(
                delta,
                Delta::TextDelta {
                    text: "Hello".to_owned(),
                }
            );
        }

        #[test]
        fn input_json_delta_keeps_the_partial_json() {
            let delta: Delta =
                serde_json::from_str(r#"{"type":"input_json_delta","partial_json":"{\"key\":"}"#)
                    .unwrap();

            assert_eq!(
                delta,
                Delta::InputJsonDelta {
                    partial_json: r#"{"key":"#.to_owned(),
                }
            );
        }

        #[test]
        fn thinking_delta_keeps_its_text() {
            let delta: Delta =
                serde_json::from_str(r#"{"type":"thinking_delta","thinking":"partial reasoning"}"#)
                    .unwrap();

            assert_eq!(
                delta,
                Delta::ThinkingDelta {
                    thinking: "partial reasoning".to_owned(),
                }
            );
        }

        #[test]
        fn signature_delta_keeps_the_signature() {
            let delta: Delta =
                serde_json::from_str(r#"{"type":"signature_delta","signature":"sig_abc123"}"#)
                    .unwrap();

            assert_eq!(
                delta,
                Delta::SignatureDelta {
                    signature: "sig_abc123".to_owned(),
                }
            );
        }

        #[test]
        fn unknown_type_parses_for_forward_compat() {
            let json = r#"{"type":"some_future_delta","data":"opaque"}"#;

            let delta: Delta = serde_json::from_str(json).unwrap();

            assert_eq!(delta, Delta::Unknown);
        }
    }
}
