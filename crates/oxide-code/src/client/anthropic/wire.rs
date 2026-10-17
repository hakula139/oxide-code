//! Serde types for Anthropic Messages API requests, streamed response frames, and error bodies.

use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};

use crate::client::{StopDetails, StopReason};
use crate::config::Effort;
use crate::message::{ContentBlock, Message, Role, WireKind};
use crate::model::ThinkingConfig;
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
    /// The one-shot JSON-schema `format` and the `effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_config: Option<OutputConfig<'a>>,
    /// Partners the `context-management-2025-06-27` beta on streaming requests for known models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) context_management: Option<ContextManagement>,
    pub(super) messages: RequestMessages<'a>,
}

/// Serializes `messages` in the request shape, with `breakpoint` (when set) stamped onto the last
/// content block of the final message. The marker exists only on the wire, so in-memory
/// [`Message`]s never carry cache state.
pub(super) struct RequestMessages<'a> {
    pub(super) messages: &'a [Message],
    pub(super) breakpoint: Option<CacheControl>,
}

impl Serialize for RequestMessages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let messages: Vec<RequestMessage<'_>> =
            self.messages.iter().map(RequestMessage::of).collect();
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

/// Mirrors claude-code 2.1.119: a single `clear_thinking_20251015` edit with `keep = "all"`.
#[derive(Serialize)]
pub(super) struct ContextManagement {
    edits: [ContextEdit; 1],
}

impl ContextManagement {
    pub(super) fn clear_thinking_keep_all() -> Self {
        Self {
            edits: [ContextEdit {
                r#type: "clear_thinking_20251015",
                keep: "all",
            }],
        }
    }
}

#[derive(Serialize)]
struct ContextEdit {
    r#type: &'static str,
    keep: &'static str,
}

/// JSON-schema-constrained completion format.
#[derive(Debug, Serialize)]
pub(crate) struct OutputFormat {
    r#type: &'static str,
    schema: serde_json::Value,
}

impl OutputFormat {
    pub(crate) fn json_schema(schema: serde_json::Value) -> Self {
        Self {
            r#type: "json_schema",
            schema,
        }
    }
}

/// `user_id` is a stringified JSON object `{device_id, account_uuid, session_id}`.
#[derive(Serialize)]
pub(super) struct RequestMetadata {
    pub(super) user_id: String,
}

/// Array form is required so the identity prefix occupies its own block (OAuth non-Haiku gate).
#[derive(Serialize)]
pub(super) struct SystemBlock<'a> {
    pub(super) r#type: &'static str,
    pub(super) text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_control: Option<CacheControl>,
}

/// `scope: "global"` shares cache across sessions (1P only). Default ttl is `"1h"`. Opt out via
/// `prompt_cache_ttl = "5m"` to use the server-side 5-minute default.
#[derive(Serialize)]
pub(super) struct CacheControl {
    pub(super) r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ttl: Option<&'static str>,
}

// ── SSE Response Types ──

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields are populated by serde and used in downstream matching"
    )
)]
/// One frame in the streaming response. `Unknown` swallows future event types so an upstream
/// addition does not break parsing.
#[derive(Debug, Clone, Deserialize)]
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
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
    #[serde(other)]
    Unknown,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub(super) struct MessageResponse {
    pub(super) id: String,
    pub(super) model: String,
    pub(super) usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
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
    /// lands here too.
    #[serde(untagged)]
    Unknown {},
}

#[expect(
    clippy::enum_variant_names,
    reason = "variant names mirror Anthropic API delta type values (text_delta, input_json_delta, etc.)"
)]
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub(super) struct MessageDeltaBody {
    pub(super) stop_reason: Option<StopReason>,
    /// Populated only on `refusal`.
    #[serde(default)]
    pub(super) stop_details: Option<StopDetails>,
}

#[expect(
    clippy::struct_field_names,
    reason = "field names mirror Anthropic's usage payload"
)]
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Usage {
    #[serde(default)]
    pub(super) input_tokens: u32,
    #[serde(default)]
    pub(super) cache_creation_input_tokens: u32,
    #[serde(default)]
    pub(super) cache_read_input_tokens: u32,
    #[serde(default)]
    pub(super) output_tokens: u32,
}

// ── Error Types ──

#[derive(Debug, Clone, Deserialize)]
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

    // ── RequestMessages ──

    fn one_hour_breakpoint() -> CacheControl {
        CacheControl {
            r#type: "ephemeral",
            scope: None,
            ttl: Some("1h"),
        }
    }

    #[test]
    fn request_messages_breakpoint_marks_only_last_block_of_final_message() {
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
            messages: &messages,
            breakpoint: Some(one_hour_breakpoint()),
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
    fn request_messages_breakpoint_keeps_tool_result_fields_on_marked_block() {
        let messages = [Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_owned(),
                content: "boom".to_owned(),
                is_error: true,
            }],
        }];
        let v = serde_json::to_value(RequestMessages {
            messages: &messages,
            breakpoint: Some(one_hour_breakpoint()),
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
    fn request_messages_without_breakpoint_or_messages_serializes_verbatim() {
        let messages = [Message::user("question"), Message::assistant("answer")];
        let v = serde_json::to_value(RequestMessages {
            messages: &messages,
            breakpoint: None,
        })
        .unwrap();
        assert_eq!(v, serde_json::to_value(messages).unwrap());

        let empty = serde_json::to_value(RequestMessages {
            messages: &[],
            breakpoint: Some(one_hour_breakpoint()),
        })
        .unwrap();
        assert_eq!(empty, json!([]));
    }

    #[test]
    fn request_messages_send_reasoning_back_under_its_api_keys_whatever_model_wrote_it() {
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
    fn request_messages_send_server_tool_results_back_as_the_api_returned_them() {
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
            messages: &messages,
            breakpoint: Some(one_hour_breakpoint()),
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

    // ── ContextManagement ──

    #[test]
    fn context_management_clear_thinking_keep_all_serializes_tagged_shape() {
        let v = serde_json::to_value(ContextManagement::clear_thinking_keep_all()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "edits": [{"type": "clear_thinking_20251015", "keep": "all"}],
            }),
        );
    }

    // ── OutputFormat ──

    #[test]
    fn output_format_json_schema_serializes_with_type_and_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
        });
        let fmt = OutputFormat::json_schema(schema.clone());
        let v = serde_json::to_value(&fmt).unwrap();
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["schema"], schema);
    }

    // ── StreamEvent ──

    #[test]
    fn stream_event_content_block_start_text() {
        let json =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockInfo::Text { text },
            } if text.is_empty(),
        ));
    }

    #[test]
    fn stream_event_content_block_stop() {
        let event: StreamEvent =
            serde_json::from_str(r#"{"type":"content_block_stop","index":2}"#).unwrap();
        assert!(matches!(event, StreamEvent::ContentBlockStop { index: 2 }));
    }

    #[test]
    fn stream_event_message_delta_with_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(
            event,
            StreamEvent::MessageDelta {
                delta: MessageDeltaBody {
                    stop_reason: Some(StopReason::EndTurn),
                    stop_details: None,
                },
                usage: Some(Usage {
                    input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens: 42,
                }),
            },
        ));
    }

    #[test]
    fn stream_event_message_delta_refusal_carries_stop_details() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"Exploit development"}},"usage":{"output_tokens":3}}"#;
        let StreamEvent::MessageDelta { delta, .. } = serde_json::from_str(json).unwrap() else {
            panic!("expected MessageDelta");
        };
        assert_eq!(delta.stop_reason, Some(StopReason::Refusal));
        assert_eq!(
            delta.stop_details,
            Some(StopDetails {
                category: Some("cyber".to_owned()),
                explanation: Some("Exploit development".to_owned()),
            }),
        );
    }

    #[test]
    fn stream_event_message_delta_unknown_stop_reason_keeps_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"some_future_reason","stop_details":null},"usage":{"output_tokens":7}}"#;
        let StreamEvent::MessageDelta { delta, usage } = serde_json::from_str(json).unwrap() else {
            panic!("expected MessageDelta");
        };
        assert_eq!(delta.stop_reason, Some(StopReason::Unknown));
        assert!(delta.stop_details.is_none());
        assert_eq!(usage.map(|u| u.output_tokens), Some(7));
    }

    #[test]
    fn stream_event_message_stop() {
        let event: StreamEvent = serde_json::from_str(r#"{"type":"message_stop"}"#).unwrap();
        assert!(matches!(event, StreamEvent::MessageStop));
    }

    // ── ContentBlockInfo ──

    #[test]
    fn content_block_info_text() {
        let info: ContentBlockInfo =
            serde_json::from_str(r#"{"type":"text","text":"Hello world"}"#).unwrap();
        assert!(matches!(info, ContentBlockInfo::Text { text } if text == "Hello world"));
    }

    #[test]
    fn content_block_info_tool_use() {
        let info: ContentBlockInfo =
            serde_json::from_str(r#"{"type":"tool_use","id":"toolu_01","name":"bash"}"#).unwrap();
        assert!(matches!(
            info,
            ContentBlockInfo::ToolUse { id, name } if id == "toolu_01" && name == "bash",
        ));
    }

    #[test]
    fn content_block_info_server_tool_use() {
        let info: ContentBlockInfo =
            serde_json::from_str(r#"{"type":"server_tool_use","id":"stu_01","name":"advisor"}"#)
                .unwrap();
        assert!(matches!(
            info,
            ContentBlockInfo::ServerToolUse { id, name } if id == "stu_01" && name == "advisor",
        ));
    }

    #[test]
    fn content_block_info_thinking() {
        let json = r#"{"type":"thinking","thinking":"Let me analyze this","signature":"sig_xyz"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        assert!(matches!(
            info,
            ContentBlockInfo::Thinking { thinking, signature }
                if thinking == "Let me analyze this" && signature == "sig_xyz",
        ));
    }

    #[test]
    fn content_block_info_redacted_thinking() {
        let info: ContentBlockInfo =
            serde_json::from_str(r#"{"type":"redacted_thinking","data":"base64data=="}"#).unwrap();
        assert!(matches!(
            info,
            ContentBlockInfo::RedactedThinking { data } if data == "base64data==",
        ));
    }

    #[test]
    fn content_block_info_server_tool_result_keeps_its_type_and_fields() {
        let json = r#"{"type":"advisor_tool_result","tool_use_id":"stu_1","content":{"type":"advisor_redacted_result","encrypted_content":"opaque"}}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        let ContentBlockInfo::ServerToolResult {
            r#type,
            tool_use_id,
            fields,
        } = info
        else {
            panic!("expected ServerToolResult, got {info:?}");
        };
        assert_eq!(
            (r#type.as_str(), tool_use_id.as_str()),
            ("advisor_tool_result", "stu_1"),
        );
        assert_eq!(
            serde_json::Value::Object(fields),
            json!({"content": {"type": "advisor_redacted_result", "encrypted_content": "opaque"}}),
        );
    }

    #[test]
    fn content_block_info_unknown_or_malformed_block() {
        for json in [
            r#"{"type":"some_future_block","data":"opaque"}"#,
            r#"{"type":"tool_use","id":"toolu_01"}"#,
        ] {
            let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
            assert!(matches!(info, ContentBlockInfo::Unknown {}), "{json}");
        }
    }

    // ── Delta ──

    #[test]
    fn delta_text() {
        let delta: Delta = serde_json::from_str(r#"{"type":"text_delta","text":"Hello"}"#).unwrap();
        assert!(matches!(delta, Delta::TextDelta { text } if text == "Hello"));
    }

    #[test]
    fn delta_input_json() {
        let delta: Delta =
            serde_json::from_str(r#"{"type":"input_json_delta","partial_json":"{\"key\":"}"#)
                .unwrap();
        assert!(matches!(
            delta,
            Delta::InputJsonDelta { partial_json } if partial_json == r#"{"key":"#,
        ));
    }

    #[test]
    fn delta_thinking() {
        let delta: Delta =
            serde_json::from_str(r#"{"type":"thinking_delta","thinking":"partial reasoning"}"#)
                .unwrap();
        assert!(matches!(
            delta,
            Delta::ThinkingDelta { thinking } if thinking == "partial reasoning",
        ));
    }

    #[test]
    fn delta_signature() {
        let delta: Delta =
            serde_json::from_str(r#"{"type":"signature_delta","signature":"sig_abc123"}"#).unwrap();
        assert!(matches!(
            delta,
            Delta::SignatureDelta { signature } if signature == "sig_abc123",
        ));
    }

    #[test]
    fn delta_unknown_type() {
        let json = r#"{"type":"some_future_delta","data":"opaque"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        assert!(matches!(delta, Delta::Unknown));
    }
}
