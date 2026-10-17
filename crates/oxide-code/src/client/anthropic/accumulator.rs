//! Folds one attempt's stream events into [`ResponseEvent`]s: content blocks, running usage,
//! progress notes, and the completed [`Response`].

use std::mem;

use tracing::{debug, warn};

use super::wire::{ApiError, ContentBlockInfo, Delta, StreamEvent, Usage};
use crate::client::{Response, ResponseEvent, StopDetails, StopReason, TokenUsage};
use crate::message::{ContentBlock, Replay, WireKind, progress_note};

// ── ResponseAccumulator ──

/// One attempt's reply as it streams. A retried attempt starts a fresh one.
#[derive(Debug)]
pub(super) struct ResponseAccumulator {
    /// The model the request asked for, the only one its reasoning blocks replay to.
    model: String,
    /// By stream index.
    blocks: Vec<BlockAccumulator>,
    usage: Option<TokenUsage>,
    stop_reason: Option<StopReason>,
    stop_details: Option<StopDetails>,
}

impl ResponseAccumulator {
    pub(super) fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            blocks: Vec::new(),
            usage: None,
            stop_reason: None,
            stop_details: None,
        }
    }

    /// Whether a content block has started, after which a retry would repeat what the consumer
    /// already showed.
    pub(super) fn content_started(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// Folds in one stream event, returning the event to forward, if any. `message_stop` yields
    /// the completed reply, and an in-band `error` comes back as `Err`.
    pub(super) fn apply(&mut self, event: StreamEvent) -> Result<Option<ResponseEvent>, ApiError> {
        let forward = match event {
            StreamEvent::MessageStart { message } => {
                self.usage = message
                    .usage
                    .map(|reported| observe(TokenUsage::default(), &reported));
                self.usage.map(ResponseEvent::Usage)
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => self.start_block(index, content_block),
            StreamEvent::ContentBlockDelta { index, delta } => self
                .blocks
                .get_mut(index)
                .and_then(|block| block.apply(delta)),
            StreamEvent::MessageDelta { delta, usage } => {
                self.stop_reason = delta.stop_reason;
                self.stop_details = delta.stop_details;
                usage.map(|reported| {
                    let usage = observe(self.usage.unwrap_or_default(), &reported);
                    self.usage = Some(usage);
                    ResponseEvent::Usage(usage)
                })
            }
            StreamEvent::MessageStop => Some(ResponseEvent::Completed(self.finish())),
            StreamEvent::Error { error } => return Err(error),
            StreamEvent::ContentBlockStop { .. } | StreamEvent::Ping | StreamEvent::Unknown => None,
        };
        Ok(forward)
    }

    /// Opens the block at `index`. A start out of index order is skipped, along with its deltas,
    /// since the index comes from the server and would otherwise size the block list.
    fn start_block(&mut self, index: usize, info: ContentBlockInfo) -> Option<ResponseEvent> {
        let next = self.blocks.len();
        if index != next {
            warn!(
                index,
                next, "skipping a content block that starts out of order"
            );
            return None;
        }
        let block = BlockAccumulator::start(info, index);
        let forward = match &block {
            BlockAccumulator::Text(text) if !text.is_empty() => {
                Some(ResponseEvent::Text(text.clone()))
            }
            BlockAccumulator::ToolUse { .. } | BlockAccumulator::ServerToolUse { .. } => {
                let thinking = self.blocks.iter().map(BlockAccumulator::thinking_text);
                progress_note(thinking).map(|note| ResponseEvent::Progress(note.to_owned()))
            }
            _ => None,
        };
        self.blocks.push(block);
        forward
    }

    fn finish(&mut self) -> Response {
        let mut response = Response {
            stop_reason: self.stop_reason,
            stop_details: self.stop_details.take(),
            ..Response::default()
        };
        for block in mem::take(&mut self.blocks) {
            let (block, parse_error) = block.into_content_block(&self.model);
            response.parse_errors.extend(parse_error);
            response.content.extend(block);
        }
        response
    }
}

/// Layers one usage report onto the snapshot so far. `message_start` reports the input and cache
/// counts with `output_tokens = 0`, and `message_delta` reports `output_tokens` with the input
/// counts zeroed, so zero means "not reported here".
fn observe(usage: TokenUsage, reported: &Usage) -> TokenUsage {
    let layer = |reported: u32, current: u32| if reported > 0 { reported } else { current };
    TokenUsage {
        input_tokens: layer(reported.input_tokens, usage.input_tokens),
        cache_creation_input_tokens: layer(
            reported.cache_creation_input_tokens,
            usage.cache_creation_input_tokens,
        ),
        cache_read_input_tokens: layer(
            reported.cache_read_input_tokens,
            usage.cache_read_input_tokens,
        ),
        output_tokens: layer(reported.output_tokens, usage.output_tokens),
    }
}

// ── BlockAccumulator ──

#[derive(Debug)]
enum BlockAccumulator {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
    ServerToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
    ServerToolResult {
        tool_use_id: String,
        kind: String,
        fields: serde_json::Map<String, serde_json::Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Absorbs deltas. Produces no [`ContentBlock`].
    Skipped,
}

impl BlockAccumulator {
    fn start(info: ContentBlockInfo, index: usize) -> Self {
        match info {
            ContentBlockInfo::Text { text } => Self::Text(text),
            ContentBlockInfo::ToolUse { id, name } => Self::ToolUse {
                id,
                name,
                json_buf: String::new(),
            },
            ContentBlockInfo::ServerToolUse { id, name } => Self::ServerToolUse {
                id,
                name,
                json_buf: String::new(),
            },
            ContentBlockInfo::ServerToolResult {
                r#type,
                tool_use_id,
                fields,
            } => Self::ServerToolResult {
                tool_use_id,
                kind: r#type,
                fields,
            },
            ContentBlockInfo::Thinking {
                thinking,
                signature,
            } => Self::Thinking {
                thinking,
                signature,
            },
            ContentBlockInfo::RedactedThinking { data } => Self::RedactedThinking { data },
            ContentBlockInfo::McpToolResult {} => {
                warn!("skipping MCP connector result at index {index}");
                Self::Skipped
            }
            ContentBlockInfo::Unknown { r#type: kind } => {
                warn!(
                    "skipping unknown or malformed content block of type {kind} at index {index}"
                );
                Self::Skipped
            }
        }
    }

    /// Appends one delta, returning the text or reasoning it streams.
    fn apply(&mut self, delta: Delta) -> Option<ResponseEvent> {
        match (self, delta) {
            (Self::Text(buf), Delta::TextDelta { text }) => {
                buf.push_str(&text);
                Some(ResponseEvent::Text(text))
            }
            (
                Self::ToolUse { json_buf, .. } | Self::ServerToolUse { json_buf, .. },
                Delta::InputJsonDelta { partial_json },
            ) => {
                json_buf.push_str(&partial_json);
                None
            }
            (
                Self::Thinking { thinking, .. },
                Delta::ThinkingDelta {
                    thinking: thinking_delta,
                },
            ) => {
                thinking.push_str(&thinking_delta);
                Some(ResponseEvent::Thinking(thinking_delta))
            }
            (
                Self::Thinking { signature, .. },
                Delta::SignatureDelta {
                    signature: sig_value,
                },
            ) => {
                *signature = sig_value;
                None
            }
            (block, delta) => {
                debug!(?block, ?delta, "ignoring unhandled delta");
                None
            }
        }
    }

    /// The block's thinking text in the form [`progress_note`] reads.
    fn thinking_text(&self) -> Option<&str> {
        match self {
            Self::Thinking { thinking, .. } => Some(thinking),
            Self::RedactedThinking { .. } => Some(""),
            _ => None,
        }
    }

    /// Surfaces any tool-input parse error alongside the produced block, whose reasoning replays
    /// only to `model`.
    fn into_content_block(self, model: &str) -> (Option<ContentBlock>, Option<(String, String)>) {
        let replay = |payload| Replay {
            wire: WireKind::Anthropic,
            model: model.to_owned(),
            payload,
        };
        match self {
            Self::Text(text) => (Some(ContentBlock::Text { text }), None),
            Self::ToolUse { id, name, json_buf } => {
                let (input, err) = parse_tool_json(&json_buf);
                let parse_error = err.map(|e| (id.clone(), e));
                (Some(ContentBlock::ToolUse { id, name, input }), parse_error)
            }
            Self::ServerToolUse { id, name, json_buf } => {
                let (input, err) = parse_tool_json(&json_buf);
                let parse_error = err.map(|e| (id.clone(), e));
                (
                    Some(ContentBlock::ServerToolUse { id, name, input }),
                    parse_error,
                )
            }
            Self::ServerToolResult {
                tool_use_id,
                kind,
                fields,
            } => (
                Some(ContentBlock::ServerToolResult {
                    tool_use_id,
                    kind,
                    fields,
                }),
                None,
            ),
            Self::Thinking {
                thinking,
                signature,
            } => (
                Some(ContentBlock::Thinking {
                    thinking,
                    replay: replay(signature),
                }),
                None,
            ),
            Self::RedactedThinking { data } => (
                Some(ContentBlock::RedactedThinking {
                    replay: replay(data),
                }),
                None,
            ),
            Self::Skipped => (None, None),
        }
    }
}

/// Returns an empty object and the parse error on failure. An empty buffer is a call that
/// streamed no input, which reads as `{}`.
fn parse_tool_json(json_buf: &str) -> (serde_json::Value, Option<String>) {
    if json_buf.is_empty() {
        return (serde_json::Value::Object(serde_json::Map::new()), None);
    }
    match serde_json::from_str(json_buf) {
        Ok(value) => (value, None),
        Err(e) => {
            warn!("malformed tool input JSON: {e}");
            (
                serde_json::Value::Object(serde_json::Map::new()),
                Some(e.to_string()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::iter;

    use serde_json::json;

    use super::*;

    // ── Fixtures ──

    const MODEL: &str = "claude-sonnet-5-5";

    fn event(value: serde_json::Value) -> StreamEvent {
        serde_json::from_value(value).expect("fixture is a stream event")
    }

    /// Folds `events` in order, returning every forwarded event.
    fn fold(reply: &mut ResponseAccumulator, events: Vec<serde_json::Value>) -> Vec<ResponseEvent> {
        events
            .into_iter()
            .filter_map(|value| reply.apply(event(value)).expect("no in-band error"))
            .collect()
    }

    fn block_start(index: usize, content_block: &serde_json::Value) -> serde_json::Value {
        json!({"type": "content_block_start", "index": index, "content_block": content_block})
    }

    fn block_delta(index: usize, delta: &serde_json::Value) -> serde_json::Value {
        json!({"type": "content_block_delta", "index": index, "delta": delta})
    }

    // ── ResponseAccumulator::content_started ──

    #[test]
    fn content_started_once_a_block_starts() {
        let mut reply = ResponseAccumulator::new(MODEL);

        fold(
            &mut reply,
            vec![
                json!({
                    "type": "message_start",
                    "message": {"id": "m", "model": "claude-sonnet-5-5"}
                }),
                block_delta(0, &json!({"type": "text_delta", "text": "orphan"})),
            ],
        );
        assert!(!reply.content_started());

        fold(
            &mut reply,
            vec![block_start(0, &json!({"type": "text", "text": ""}))],
        );
        assert!(reply.content_started());
    }

    // ── ResponseAccumulator::apply ──

    #[test]
    fn apply_layers_usage_from_message_start_and_message_delta() {
        let mut reply = ResponseAccumulator::new(MODEL);

        let forwarded = fold(
            &mut reply,
            vec![
                json!({
                    "type": "message_start",
                    "message": {
                        "id": "m",
                        "model": "claude-sonnet-5-5",
                        "usage": {
                            "input_tokens": 10,
                            "cache_creation_input_tokens": 20,
                            "cache_read_input_tokens": 30,
                            "output_tokens": 0
                        }
                    }
                }),
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 5}
                }),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            ],
        );

        let start = TokenUsage {
            input_tokens: 10,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30,
            output_tokens: 0,
        };
        assert_eq!(
            forwarded,
            [
                ResponseEvent::Usage(start),
                ResponseEvent::Usage(TokenUsage {
                    output_tokens: 5,
                    ..start
                }),
            ],
        );
    }

    #[test]
    fn apply_streams_text_from_block_starts_and_deltas() {
        let mut reply = ResponseAccumulator::new(MODEL);

        let forwarded = fold(
            &mut reply,
            vec![
                block_start(0, &json!({"type": "text", "text": "Hello "})),
                block_delta(0, &json!({"type": "text_delta", "text": "there"})),
                block_start(1, &json!({"type": "text", "text": ""})),
                block_delta(1, &json!({"type": "text_delta", "text": "!"})),
                json!({"type": "message_stop"}),
            ],
        );

        assert_eq!(
            forwarded,
            [
                ResponseEvent::Text("Hello ".to_owned()),
                ResponseEvent::Text("there".to_owned()),
                ResponseEvent::Text("!".to_owned()),
                ResponseEvent::Completed(Response {
                    content: vec![
                        ContentBlock::Text {
                            text: "Hello there".to_owned(),
                        },
                        ContentBlock::Text {
                            text: "!".to_owned(),
                        },
                    ],
                    ..Response::default()
                }),
            ],
        );
    }

    #[test]
    fn apply_sends_each_progress_note_as_its_tool_use_starts() {
        let thinking = |text: &str| {
            (
                json!({"type": "thinking", "thinking": "", "signature": "sig"}),
                Some(json!({"type": "thinking_delta", "thinking": text})),
            )
        };
        let text = |text: &str| {
            (
                json!({"type": "text", "text": ""}),
                Some(json!({"type": "text_delta", "text": text})),
            )
        };
        let tool = |kind: &str, id: &str| {
            (
                json!({"type": kind, "id": id, "name": "echo"}),
                Some(json!({"type": "input_json_delta", "partial_json": "{}"})),
            )
        };
        let blocks = [
            thinking(""),
            thinking("Searching."),
            tool("server_tool_use", "s1"),
            text("Found it."),
            tool("tool_use", "t1"),
            thinking("Now b."),
            (json!({"type": "redacted_thinking", "data": "opaque"}), None),
            tool("tool_use", "t2"),
            thinking("Summing up."),
            text("Checked."),
        ];
        let mut reply = ResponseAccumulator::new(MODEL);

        let forwarded = fold(
            &mut reply,
            blocks
                .into_iter()
                .enumerate()
                .flat_map(|(index, (start, delta))| {
                    iter::once(block_start(index, &start))
                        .chain(delta.map(|delta| block_delta(index, &delta)))
                })
                .chain([json!({"type": "message_stop"})])
                .collect(),
        );

        let notes: Vec<String> = forwarded
            .iter()
            .filter_map(|event| match event {
                ResponseEvent::Text(text) => Some(format!("text {text}")),
                ResponseEvent::Progress(note) => Some(format!("progress {note}")),
                _ => None,
            })
            .collect();
        // Text cuts `t1` off from any note, and a note before text introduces no tool call.
        assert_eq!(
            notes,
            [
                "progress Searching.",
                "text Found it.",
                "progress Now b.",
                "text Checked.",
            ],
        );
        let thinking_block = |text: &str| ContentBlock::Thinking {
            thinking: text.to_owned(),
            replay: Replay::with_model(MODEL, "sig"),
        };
        let text_block = |text: &str| ContentBlock::Text {
            text: text.to_owned(),
        };
        let tool_block = |id: &str| ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "echo".to_owned(),
            input: json!({}),
        };
        // Every block, the empty reasoning one included, stays in the reply.
        assert_eq!(
            forwarded.last(),
            Some(&ResponseEvent::Completed(Response {
                content: vec![
                    thinking_block(""),
                    thinking_block("Searching."),
                    ContentBlock::ServerToolUse {
                        id: "s1".to_owned(),
                        name: "echo".to_owned(),
                        input: json!({}),
                    },
                    text_block("Found it."),
                    tool_block("t1"),
                    thinking_block("Now b."),
                    ContentBlock::RedactedThinking {
                        replay: Replay::with_model(MODEL, "opaque"),
                    },
                    tool_block("t2"),
                    thinking_block("Summing up."),
                    text_block("Checked."),
                ],
                ..Response::default()
            })),
        );
    }

    #[test]
    fn apply_keeps_a_server_tool_result_whole_beside_its_call() {
        let results = json!([{"type": "web_search_result", "encrypted_content": "opaque"}]);
        let mut reply = ResponseAccumulator::new(MODEL);

        let forwarded = fold(
            &mut reply,
            vec![
                block_start(
                    0,
                    &json!({"type": "server_tool_use", "id": "s1", "name": "web_search"}),
                ),
                block_delta(
                    0,
                    &json!({"type": "input_json_delta", "partial_json": r#"{"query":"rust"}"#}),
                ),
                block_start(
                    1,
                    &json!({
                        "type": "web_search_tool_result",
                        "tool_use_id": "s1",
                        "content": results,
                    }),
                ),
                block_delta(1, &json!({"type": "text_delta", "text": "stray"})),
                block_start(2, &json!({"type": "text", "text": "Found it."})),
                json!({"type": "message_stop"}),
            ],
        );

        assert_eq!(
            forwarded,
            [
                ResponseEvent::Text("Found it.".to_owned()),
                ResponseEvent::Completed(Response {
                    content: vec![
                        ContentBlock::ServerToolUse {
                            id: "s1".to_owned(),
                            name: "web_search".to_owned(),
                            input: json!({"query": "rust"}),
                        },
                        ContentBlock::ServerToolResult {
                            tool_use_id: "s1".to_owned(),
                            kind: "web_search_tool_result".to_owned(),
                            fields: serde_json::Map::from_iter([("content".to_owned(), results)]),
                        },
                        ContentBlock::Text {
                            text: "Found it.".to_owned(),
                        },
                    ],
                    ..Response::default()
                }),
            ],
        );
    }

    #[test]
    fn apply_completes_the_reply_with_parse_errors_and_the_stop() {
        let mut reply = ResponseAccumulator::new(MODEL);

        let forwarded = fold(
            &mut reply,
            vec![
                block_start(0, &json!({"type": "text", "text": "Writing it now."})),
                block_start(1, &json!({"type": "tool_use", "id": "t1", "name": "write"})),
                block_delta(
                    1,
                    &json!({"type": "input_json_delta", "partial_json": r#"{"content": "#}),
                ),
                block_delta(5, &json!({"type": "text_delta", "text": "lost"})),
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "refusal", "stop_details": {"category": "cyber"}}
                }),
                json!({"type": "message_stop"}),
            ],
        );

        let parse_error = serde_json::from_str::<serde_json::Value>(r#"{"content": "#).unwrap_err();
        assert_eq!(
            forwarded,
            [
                ResponseEvent::Text("Writing it now.".to_owned()),
                ResponseEvent::Completed(Response {
                    content: vec![
                        ContentBlock::Text {
                            text: "Writing it now.".to_owned(),
                        },
                        ContentBlock::ToolUse {
                            id: "t1".to_owned(),
                            name: "write".to_owned(),
                            input: json!({}),
                        },
                    ],
                    parse_errors: HashMap::from([("t1".to_owned(), parse_error.to_string())]),
                    stop_reason: Some(StopReason::Refusal),
                    stop_details: Some(StopDetails {
                        category: Some("cyber".to_owned()),
                        explanation: None,
                    }),
                }),
            ],
        );
    }

    #[test]
    fn apply_skips_a_block_that_starts_out_of_order() {
        let mut reply = ResponseAccumulator::new(MODEL);

        let forwarded = fold(
            &mut reply,
            vec![
                block_start(usize::MAX, &json!({"type": "text", "text": "far"})),
                block_delta(usize::MAX, &json!({"type": "text_delta", "text": "lost"})),
                block_start(1, &json!({"type": "text", "text": "early"})),
            ],
        );
        assert_eq!(forwarded, []);
        assert!(!reply.content_started());

        let forwarded = fold(
            &mut reply,
            vec![
                block_start(0, &json!({"type": "text", "text": "Hi"})),
                json!({"type": "message_stop"}),
            ],
        );
        assert_eq!(
            forwarded,
            [
                ResponseEvent::Text("Hi".to_owned()),
                ResponseEvent::Completed(Response {
                    content: vec![ContentBlock::Text {
                        text: "Hi".to_owned(),
                    }],
                    ..Response::default()
                }),
            ],
        );
    }

    #[test]
    fn apply_in_band_error_fails_the_reply() {
        let mut reply = ResponseAccumulator::new(MODEL);

        let error = reply
            .apply(event(json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            })))
            .expect_err("an error event fails the reply");

        assert_eq!(
            (error.error_type.as_str(), error.message.as_str()),
            ("overloaded_error", "Overloaded"),
        );
        assert!(!reply.content_started());
    }

    // ── BlockAccumulator::start ──

    #[test]
    fn start_keeps_initial_text_thinking_and_redacted_data() {
        let text = BlockAccumulator::start(
            ContentBlockInfo::Text {
                text: "hi".to_owned(),
            },
            0,
        );
        assert!(matches!(text, BlockAccumulator::Text(t) if t == "hi"));

        let thinking = BlockAccumulator::start(
            ContentBlockInfo::Thinking {
                thinking: "step 1".to_owned(),
                signature: "sig_abc".to_owned(),
            },
            0,
        );
        assert!(matches!(
            thinking,
            BlockAccumulator::Thinking { thinking, signature }
                if thinking == "step 1" && signature == "sig_abc"
        ));

        let redacted = BlockAccumulator::start(
            ContentBlockInfo::RedactedThinking {
                data: "opaque-blob".to_owned(),
            },
            0,
        );
        assert!(
            matches!(redacted, BlockAccumulator::RedactedThinking { data } if data == "opaque-blob")
        );
    }

    #[test]
    fn start_opens_tool_uses_with_an_empty_input_buffer() {
        let tool = BlockAccumulator::start(
            ContentBlockInfo::ToolUse {
                id: "tool_1".to_owned(),
                name: "bash".to_owned(),
            },
            0,
        );
        assert!(matches!(
            tool,
            BlockAccumulator::ToolUse { id, name, json_buf }
                if id == "tool_1" && name == "bash" && json_buf.is_empty()
        ));

        let server = BlockAccumulator::start(
            ContentBlockInfo::ServerToolUse {
                id: "srv_1".to_owned(),
                name: "web_search".to_owned(),
            },
            0,
        );
        assert!(matches!(
            server,
            BlockAccumulator::ServerToolUse { id, name, json_buf }
                if id == "srv_1" && name == "web_search" && json_buf.is_empty()
        ));
    }

    #[test]
    fn start_skips_unknown_blocks_and_mcp_connector_results() {
        for info in [
            ContentBlockInfo::Unknown {
                r#type: json!("some_future_block"),
            },
            ContentBlockInfo::McpToolResult {},
        ] {
            let block = BlockAccumulator::start(info, 0);
            assert!(matches!(block, BlockAccumulator::Skipped), "{block:?}");
        }
    }

    // ── BlockAccumulator::apply ──

    #[test]
    fn apply_appends_tool_input_without_streaming_it() {
        for mut block in [
            BlockAccumulator::ToolUse {
                id: "tool_1".to_owned(),
                name: "bash".to_owned(),
                json_buf: r#"{"x"#.to_owned(),
            },
            BlockAccumulator::ServerToolUse {
                id: "srv_1".to_owned(),
                name: "web_search".to_owned(),
                json_buf: r#"{"x"#.to_owned(),
            },
        ] {
            let forwarded = block.apply(Delta::InputJsonDelta {
                partial_json: r":1}".to_owned(),
            });

            assert_eq!(forwarded, None);
            assert!(
                matches!(
                    &block,
                    BlockAccumulator::ToolUse { json_buf, .. }
                        | BlockAccumulator::ServerToolUse { json_buf, .. }
                        if json_buf == r#"{"x:1}"#
                ),
                "{block:?}",
            );
        }
    }

    #[test]
    fn apply_appends_thinking_and_replaces_the_signature() {
        let mut block = BlockAccumulator::Thinking {
            thinking: "step 1".to_owned(),
            signature: "stale".to_owned(),
        };

        let thinking = block.apply(Delta::ThinkingDelta {
            thinking: ", step 2".to_owned(),
        });
        let signature = block.apply(Delta::SignatureDelta {
            signature: "sig_abc".to_owned(),
        });

        assert_eq!(
            thinking,
            Some(ResponseEvent::Thinking(", step 2".to_owned()))
        );
        assert_eq!(signature, None);
        assert!(matches!(
            &block,
            BlockAccumulator::Thinking { thinking, signature }
                if thinking == "step 1, step 2" && signature == "sig_abc"
        ));
    }

    #[test]
    fn apply_ignores_a_delta_of_another_block_kind() {
        let mut block = BlockAccumulator::Text("hi".to_owned());

        let forwarded = block.apply(Delta::InputJsonDelta {
            partial_json: "ignored".to_owned(),
        });

        assert_eq!(forwarded, None);
        assert!(matches!(&block, BlockAccumulator::Text(buf) if buf == "hi"));
    }

    // ── BlockAccumulator::into_content_block ──

    #[test]
    fn into_content_block_yields_text_verbatim_and_reasoning_for_its_model() {
        assert_eq!(
            BlockAccumulator::Text("hi".to_owned()).into_content_block(MODEL),
            (
                Some(ContentBlock::Text {
                    text: "hi".to_owned()
                }),
                None
            ),
        );
        assert_eq!(
            BlockAccumulator::Thinking {
                thinking: "step 1".to_owned(),
                signature: "sig_abc".to_owned(),
            }
            .into_content_block(MODEL),
            (
                Some(ContentBlock::Thinking {
                    thinking: "step 1".to_owned(),
                    replay: Replay::with_model(MODEL, "sig_abc"),
                }),
                None,
            ),
        );
        assert_eq!(
            BlockAccumulator::RedactedThinking {
                data: "opaque-blob".to_owned(),
            }
            .into_content_block(MODEL),
            (
                Some(ContentBlock::RedactedThinking {
                    replay: Replay::with_model(MODEL, "opaque-blob"),
                }),
                None,
            ),
        );
        assert_eq!(
            BlockAccumulator::Skipped.into_content_block(MODEL),
            (None, None)
        );
    }

    #[test]
    fn into_content_block_parses_tool_input() {
        let (tool, err) = BlockAccumulator::ToolUse {
            id: "tool_1".to_owned(),
            name: "bash".to_owned(),
            json_buf: r#"{"command": "ls"}"#.to_owned(),
        }
        .into_content_block(MODEL);
        assert_eq!(
            tool,
            Some(ContentBlock::ToolUse {
                id: "tool_1".to_owned(),
                name: "bash".to_owned(),
                input: json!({"command": "ls"}),
            }),
        );
        assert_eq!(err, None);

        let (server, err) = BlockAccumulator::ServerToolUse {
            id: "srv_1".to_owned(),
            name: "web_search".to_owned(),
            json_buf: r#"{"query": "rust"}"#.to_owned(),
        }
        .into_content_block(MODEL);
        assert_eq!(
            server,
            Some(ContentBlock::ServerToolUse {
                id: "srv_1".to_owned(),
                name: "web_search".to_owned(),
                input: json!({"query": "rust"}),
            }),
        );
        assert_eq!(err, None);
    }

    #[test]
    fn into_content_block_malformed_tool_input_surfaces_a_parse_error() {
        for block in [
            BlockAccumulator::ToolUse {
                id: "tool_1".to_owned(),
                name: "bash".to_owned(),
                json_buf: "{unclosed".to_owned(),
            },
            BlockAccumulator::ServerToolUse {
                id: "tool_1".to_owned(),
                name: "web_search".to_owned(),
                json_buf: "{unclosed".to_owned(),
            },
        ] {
            let (block, err) = block.into_content_block(MODEL);

            assert!(
                matches!(
                    &block,
                    Some(
                        ContentBlock::ToolUse { input, .. }
                            | ContentBlock::ServerToolUse { input, .. }
                    ) if *input == json!({})
                ),
                "{block:?}",
            );
            let serde_error = serde_json::from_str::<serde_json::Value>("{unclosed").unwrap_err();
            assert_eq!(err, Some(("tool_1".to_owned(), serde_error.to_string())));
        }
    }

    // ── parse_tool_json ──

    #[test]
    fn parse_tool_json_empty_buffer_is_an_empty_object() {
        assert_eq!(parse_tool_json(""), (json!({}), None));
    }
}
