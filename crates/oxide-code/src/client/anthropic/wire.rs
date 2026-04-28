//! Anthropic Messages API wire types — pure data; helpers that build
//! or interpret these types live in sibling modules.

use serde::{Deserialize, Serialize};

use crate::config::{Effort, ThinkingConfig};
use crate::message::Message;
use crate::tool::ToolDefinition;

// ── Request types ──

#[derive(Serialize)]
pub(super) struct CreateMessageRequest<'a> {
    pub(super) model: &'a str,
    pub(super) max_tokens: u32,
    pub(super) stream: bool,
    pub(super) metadata: RequestMetadata,
    /// Serialized before `messages` so the billing header's `cch=00000`
    /// placeholder appears first in the JSON, making
    /// [`super::billing::inject_cch`]'s single-occurrence replacement
    /// safe even when tool results contain the literal placeholder
    /// string.
    pub(super) system: Vec<SystemBlock<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) thinking: Option<&'a ThinkingConfig>,
    /// Carries both the `format` (JSON-schema-constrained output for
    /// one-shot calls) and `effort` (agentic-path intelligence tier)
    /// knobs. Wrapped in `Option` so an empty `OutputConfig` never
    /// ships — callers build one via [`OutputConfig::new`] and pass
    /// `None` when neither sub-field is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) output_config: Option<OutputConfig<'a>>,
    /// `context_management.edits` — the client-side context-editing
    /// directive that partners the `context-management-2025-06-27`
    /// beta header. Populated on the streaming path for any model
    /// with [`Capabilities::context_management`][crate::model::Capabilities::context_management]
    /// set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) context_management: Option<ContextManagement>,
    pub(super) messages: &'a [Message],
}

/// Shared wrapper for the `output_config` body field. Either field
/// may be absent; when both are, [`Self::new`] returns `None` so the
/// builder never ships an empty object.
#[derive(Serialize)]
pub(super) struct OutputConfig<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a OutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<Effort>,
}

impl<'a> OutputConfig<'a> {
    /// Returns `None` when every field is empty so callers can avoid
    /// shipping a bare `{}`. `Some(_)` otherwise.
    pub(super) fn new(format: Option<&'a OutputFormat>, effort: Option<Effort>) -> Option<Self> {
        (format.is_some() || effort.is_some()).then_some(Self { format, effort })
    }
}

/// `context_management.edits` body field. oxide-code mirrors
/// claude-code 2.1.119's observed wire shape — a single
/// `clear_thinking_20251015` edit with `keep = "all"` on every
/// agentic request that also ships the matching beta header.
#[derive(Serialize)]
pub(super) struct ContextManagement {
    edits: [ContextEdit; 1],
}

impl ContextManagement {
    /// Wire shape claude-code 2.1.119 sends on every 4.6+ request.
    /// Single place to edit when Anthropic ships newer edit types or
    /// we need to diverge from the default.
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

/// JSON-schema-constrained completion format. Constructed via
/// [`OutputFormat::json_schema`]; callers typically build one per
/// request shape (e.g., `{"title": string}`) and pass it by reference
/// to [`super::Client::complete`].
#[derive(Debug, Serialize)]
pub(crate) struct OutputFormat {
    r#type: &'static str,
    schema: serde_json::Value,
}

impl OutputFormat {
    /// Builds a `json_schema` output format from a precomputed schema
    /// value. The schema must already match Anthropic's expectations
    /// (`type: "object"`, `additionalProperties: false`, explicit
    /// `required` array) — we don't validate here.
    pub(crate) fn json_schema(schema: serde_json::Value) -> Self {
        Self {
            r#type: "json_schema",
            schema,
        }
    }
}

/// Top-level `metadata` object on every outbound request.
///
/// `user_id` is a stringified JSON object with the canonical claude-code
/// shape `{device_id, account_uuid, session_id}`; field order is part of
/// the wire fingerprint. The API receives it as a flat string, not a
/// nested object.
#[derive(Serialize)]
pub(super) struct RequestMetadata {
    pub(super) user_id: String,
}

/// A text block in the system prompt array. The Anthropic API accepts `system`
/// as either a string or an array of these blocks. Using the array form lets
/// the identity prefix occupy its own block, which is required for OAuth
/// validation on non-Haiku models.
#[derive(Serialize)]
pub(super) struct SystemBlock<'a> {
    pub(super) r#type: &'static str,
    pub(super) text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_control: Option<CacheControl>,
}

/// Prompt caching control. The `scope` field determines the cache sharing
/// level: `"global"` for static content identical across sessions (1P only),
/// `None` for the default org-scoped ephemeral cache (universally accepted).
/// The `ttl` field overrides the server default (5 m as of 2026-03) —
/// oxide-code defaults to `"1h"`, opt-out via `prompt_cache_ttl = "5m"`.
///
/// `scope: "global"` must be a true prefix of all preceding request content
/// — the server rejects a global-scoped block preceded by a non-global
/// block (including tool definitions, which render before `system`). See
/// [`super::betas::is_first_party_base_url`] for where the gating decision
/// is made.
#[derive(Serialize)]
pub(super) struct CacheControl {
    pub(super) r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ttl: Option<&'static str>,
}

// ── SSE response types ──

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields are populated by serde and used in downstream matching"
    )
)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamEvent {
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
    /// Catch-all for unrecognized event types.
    /// Silently skipped during stream processing.
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
pub(crate) struct MessageResponse {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlockInfo {
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
    /// Catch-all for unrecognized block types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[expect(
    clippy::enum_variant_names,
    reason = "variant names mirror Anthropic API delta type values (text_delta, input_json_delta, etc.)"
)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    /// Full signature value (overwrites, not appended).
    SignatureDelta {
        signature: String,
    },
    /// Catch-all for unrecognized delta types.
    /// Silently skipped during stream processing.
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
pub(crate) struct MessageDeltaBody {
    pub(crate) stop_reason: Option<String>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) input_tokens: u32,
    #[serde(default)]
    pub(crate) output_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiError {
    #[serde(rename = "type")]
    pub(crate) error_type: String,
    pub(crate) message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
                delta: MessageDeltaBody { stop_reason: Some(ref reason) },
                usage: Some(Usage { input_tokens: 0, output_tokens: 42 }),
            } if reason == "end_turn",
        ));
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
    fn content_block_info_unknown_type() {
        let json = r#"{"type":"some_future_block","data":"opaque"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        assert!(matches!(info, ContentBlockInfo::Unknown));
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
