//! Folds one attempt's stream events into [`ResponseEvent`]s: content blocks, running usage,
//! progress notes, and the completed [`Response`].

use std::mem;

use tracing::{debug, warn};

use super::wire::{ApiError, ContentBlockInfo, Delta, StreamEvent};
use crate::client::{Response, ResponseEvent, StopDetails, StopReason};
use crate::message::{ContentBlock, Replay, WireKind, progress_note};
use crate::model::TokenUsage;

// ── ReplySpec ──

/// What a request asked its reply to carry.
#[derive(Debug, Clone)]
pub(super) struct ReplySpec {
    /// The model the request names, which the reply's reasoning blocks record as their producer.
    pub(super) model: String,
    /// Whether the request asked for thinking text. Text it did not ask for, which an upstream may
    /// still send, stays in the reply's blocks but is not streamed.
    pub(super) thinking_text: bool,
}

// ── ResponseAccumulator ──

/// One attempt's reply as it streams. A retried attempt starts a fresh one.
#[derive(Debug)]
pub(super) struct ResponseAccumulator {
    spec: ReplySpec,
    /// By stream index.
    blocks: Vec<BlockAccumulator>,
    usage: Option<TokenUsage>,
    stop_reason: Option<StopReason>,
    stop_details: Option<StopDetails>,
}

impl ResponseAccumulator {
    pub(super) fn new(spec: &ReplySpec) -> Self {
        Self {
            spec: spec.clone(),
            blocks: Vec::new(),
            usage: None,
            stop_reason: None,
            stop_details: None,
        }
    }

    /// Returns whether a content block has started, after which a retry would repeat what the
    /// consumer already showed.
    pub(super) fn content_started(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// Folds in one stream event, returning the event to forward, if any. `message_stop` yields the
    /// completed reply, and an in-band `error` comes back as `Err`.
    pub(super) fn apply(&mut self, event: StreamEvent) -> Result<Option<ResponseEvent>, ApiError> {
        let forward = match event {
            StreamEvent::MessageStart { message } => {
                self.usage = message
                    .usage
                    .map(|reported| observe(TokenUsage::default(), reported));
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
                    let usage = observe(self.usage.unwrap_or_default(), reported);
                    self.usage = Some(usage);
                    ResponseEvent::Usage(usage)
                })
            }
            StreamEvent::MessageStop => Some(ResponseEvent::Completed(self.finish())),
            StreamEvent::Error { error } => return Err(error),
            StreamEvent::ContentBlockStop | StreamEvent::Ping | StreamEvent::Unknown => None,
        };
        Ok(forward.filter(|event| {
            self.spec.thinking_text
                || !matches!(
                    event,
                    ResponseEvent::Thinking(_) | ResponseEvent::Progress(_)
                )
        }))
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
            BlockAccumulator::Thinking { thinking, .. } if !thinking.is_empty() => {
                Some(ResponseEvent::Thinking(thinking.clone()))
            }
            BlockAccumulator::ToolUse { .. } => {
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
            let (block, parse_error) = block.into_content_block(&self.spec.model);
            response.parse_errors.extend(parse_error);
            response.content.extend(block);
        }
        response
    }
}

/// Layers one usage report onto the snapshot so far. `message_start` reports the input and cache
/// counts with `output_tokens = 0`, and `message_delta` reports `output_tokens` with the input
/// counts zeroed, so zero means "not reported here".
fn observe(usage: TokenUsage, reported: TokenUsage) -> TokenUsage {
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

#[derive(Debug, PartialEq)]
enum BlockAccumulator {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        json_buf: Option<String>,
        /// Whether this is a `server_tool_use` call, which the API runs itself.
        server: bool,
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
            ContentBlockInfo::ToolUse { id, name, input } => Self::tool_use(id, name, input, false),
            ContentBlockInfo::ServerToolUse { id, name, input } => {
                Self::tool_use(id, name, input, true)
            }
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
                warn!(index, "skipping MCP connector result");
                Self::Skipped
            }
            ContentBlockInfo::Unknown { r#type: kind } => {
                warn!(index, %kind, "skipping unknown content block");
                Self::Skipped
            }
        }
    }

    fn tool_use(id: String, name: String, input: serde_json::Value, server: bool) -> Self {
        Self::ToolUse {
            id,
            name,
            input,
            json_buf: None,
            server,
        }
    }

    /// Appends one delta, returning the text or reasoning it streams.
    fn apply(&mut self, delta: Delta) -> Option<ResponseEvent> {
        match (self, delta) {
            (Self::Text(buf), Delta::TextDelta { text }) => {
                buf.push_str(&text);
                Some(ResponseEvent::Text(text))
            }
            (Self::ToolUse { json_buf, .. }, Delta::InputJsonDelta { partial_json }) => {
                json_buf.get_or_insert_default().push_str(&partial_json);
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
            Self::ToolUse {
                id,
                name,
                input,
                json_buf,
                server,
            } => {
                let (input, err) = json_buf.map_or((input, None), |json| parse_tool_json(&json));
                let parse_error = err.map(|e| (id.clone(), e));
                let block = if server {
                    ContentBlock::ServerToolUse { id, name, input }
                } else {
                    ContentBlock::ToolUse { id, name, input }
                };
                (Some(block), parse_error)
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

/// Treats an empty delta buffer as `{}`. Malformed JSON returns `{}` with its parse error.
fn parse_tool_json(json_buf: &str) -> (serde_json::Value, Option<String>) {
    let error = match serde_json::from_str(json_buf) {
        Ok(value) => return (value, None),
        Err(_) if json_buf.is_empty() => None,
        Err(e) => {
            warn!(error = %e, "malformed tool input JSON");
            Some(e.to_string())
        }
    };
    (serde_json::Value::Object(serde_json::Map::new()), error)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::iter;

    use serde_json::json;

    use super::*;

    const MODEL: &str = "claude-sonnet-5-5";

    fn accumulator(thinking_text: bool) -> ResponseAccumulator {
        ResponseAccumulator::new(&ReplySpec {
            model: MODEL.to_owned(),
            thinking_text,
        })
    }

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

    mod content_started {
        use super::*;

        #[test]
        fn once_a_block_starts() {
            let mut reply = accumulator(true);

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
    }

    mod apply {
        use super::*;

        #[test]
        fn keeps_initial_tool_input_until_a_delta_replaces_it() {
            for kind in ["tool_use", "server_tool_use"] {
                for (case, initial, deltas, expected, malformed) in [
                    ("empty_input", json!({}), vec![], json!({}), false),
                    (
                        "start_only",
                        json!({"file_path": "notes.txt"}),
                        vec![],
                        json!({"file_path": "notes.txt"}),
                        false,
                    ),
                    (
                        "fragmented",
                        json!({}),
                        vec![r#"{"file_path":"#, r#""notes.txt"}"#],
                        json!({"file_path": "notes.txt"}),
                        false,
                    ),
                    (
                        "replacement",
                        json!({"old": true}),
                        vec![r#"{"new":true}"#],
                        json!({"new": true}),
                        false,
                    ),
                    ("empty_deltas", json!({}), vec!["", ""], json!({}), false),
                    (
                        "empty_replacement",
                        json!({"old": true}),
                        vec![""],
                        json!({}),
                        false,
                    ),
                    (
                        "malformed",
                        json!({"old": true}),
                        vec!["{"],
                        json!({}),
                        true,
                    ),
                ] {
                    let mut reply = accumulator(true);
                    let mut events = vec![block_start(
                        0,
                        &json!({"type": kind, "id": "t1", "name": "read", "input": initial}),
                    )];
                    events.extend(deltas.into_iter().map(|partial_json| {
                        block_delta(
                            0,
                            &json!({"type": "input_json_delta", "partial_json": partial_json}),
                        )
                    }));
                    events.extend([
                        json!({"type": "content_block_stop", "index": 0}),
                        json!({"type": "message_stop"}),
                    ]);

                    let forwarded = fold(&mut reply, events);

                    let block = if kind == "tool_use" {
                        ContentBlock::ToolUse {
                            id: "t1".to_owned(),
                            name: "read".to_owned(),
                            input: expected,
                        }
                    } else {
                        ContentBlock::ServerToolUse {
                            id: "t1".to_owned(),
                            name: "read".to_owned(),
                            input: expected,
                        }
                    };
                    assert!(
                        matches!(
                            forwarded.as_slice(),
                            [ResponseEvent::Completed(response)]
                                if response.content == [block]
                                    && response.parse_errors.contains_key("t1") == malformed
                        ),
                        "{kind}: {case}: {forwarded:?}"
                    );
                }
            }
        }

        #[test]
        fn layers_usage_from_message_start_and_message_delta() {
            let mut reply = accumulator(true);

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
        fn streams_text_and_thinking_from_block_starts_and_deltas() {
            let mut reply = accumulator(true);

            let forwarded = fold(
                &mut reply,
                vec![
                    block_start(
                        0,
                        &json!({"type": "thinking", "thinking": "Let me ", "signature": "sig"}),
                    ),
                    block_delta(0, &json!({"type": "thinking_delta", "thinking": "think"})),
                    block_start(1, &json!({"type": "text", "text": "Hello "})),
                    block_delta(1, &json!({"type": "text_delta", "text": "there"})),
                    block_start(2, &json!({"type": "text", "text": ""})),
                    block_delta(2, &json!({"type": "text_delta", "text": "!"})),
                    block_start(
                        3,
                        &json!({"type": "thinking", "thinking": "", "signature": "sig2"}),
                    ),
                    block_delta(3, &json!({"type": "thinking_delta", "thinking": "Done"})),
                    json!({"type": "message_stop"}),
                ],
            );

            assert_eq!(
                forwarded,
                [
                    ResponseEvent::Thinking("Let me ".to_owned()),
                    ResponseEvent::Thinking("think".to_owned()),
                    ResponseEvent::Text("Hello ".to_owned()),
                    ResponseEvent::Text("there".to_owned()),
                    ResponseEvent::Text("!".to_owned()),
                    ResponseEvent::Thinking("Done".to_owned()),
                    ResponseEvent::Completed(Response {
                        content: vec![
                            ContentBlock::Thinking {
                                thinking: "Let me think".to_owned(),
                                replay: Replay::with_model(MODEL, "sig"),
                            },
                            ContentBlock::Text {
                                text: "Hello there".to_owned(),
                            },
                            ContentBlock::Text {
                                text: "!".to_owned(),
                            },
                            ContentBlock::Thinking {
                                thinking: "Done".to_owned(),
                                replay: Replay::with_model(MODEL, "sig2"),
                            },
                        ],
                        ..Response::default()
                    }),
                ],
            );
        }

        #[test]
        fn sends_each_progress_note_as_its_tool_use_starts() {
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
            let mut reply = accumulator(true);

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
        fn withholds_thinking_text_the_request_did_not_ask_for() {
            let mut reply = accumulator(false);

            let forwarded = fold(
                &mut reply,
                vec![
                    block_start(
                        0,
                        &json!({"type": "thinking", "thinking": "Plan.", "signature": "sig"}),
                    ),
                    block_delta(0, &json!({"type": "thinking_delta", "thinking": " Next."})),
                    block_start(1, &json!({"type": "tool_use", "id": "t1", "name": "echo"})),
                    block_delta(
                        1,
                        &json!({"type": "input_json_delta", "partial_json": "{}"}),
                    ),
                    json!({"type": "message_stop"}),
                ],
            );

            assert_eq!(
                forwarded,
                [ResponseEvent::Completed(Response {
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "Plan. Next.".to_owned(),
                            replay: Replay::with_model(MODEL, "sig"),
                        },
                        ContentBlock::ToolUse {
                            id: "t1".to_owned(),
                            name: "echo".to_owned(),
                            input: json!({}),
                        },
                    ],
                    ..Response::default()
                })],
            );
        }

        #[test]
        fn keeps_a_server_tool_result_whole_beside_its_call() {
            let results = json!([{"type": "web_search_result", "encrypted_content": "opaque"}]);
            let mut reply = accumulator(true);

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
                                fields: serde_json::Map::from_iter([(
                                    "content".to_owned(),
                                    results
                                )]),
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
        fn completes_the_reply_with_parse_errors_and_the_stop() {
            let mut reply = accumulator(true);

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

            let parse_error =
                serde_json::from_str::<serde_json::Value>(r#"{"content": "#).unwrap_err();
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
        fn skips_a_block_that_starts_out_of_order() {
            let mut reply = accumulator(true);

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
        fn in_band_error_fails_the_reply() {
            let mut reply = accumulator(true);

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
    }

    mod block_accumulator {
        use super::*;

        mod start {
            use super::*;

            #[test]
            fn keeps_initial_text_thinking_and_redacted_data() {
                let text = BlockAccumulator::start(
                    ContentBlockInfo::Text {
                        text: "hi".to_owned(),
                    },
                    0,
                );

                assert_eq!(text, BlockAccumulator::Text("hi".to_owned()));

                let thinking = BlockAccumulator::start(
                    ContentBlockInfo::Thinking {
                        thinking: "step 1".to_owned(),
                        signature: "sig_abc".to_owned(),
                    },
                    0,
                );

                assert_eq!(
                    thinking,
                    BlockAccumulator::Thinking {
                        thinking: "step 1".to_owned(),
                        signature: "sig_abc".to_owned(),
                    },
                );

                let redacted = BlockAccumulator::start(
                    ContentBlockInfo::RedactedThinking {
                        data: "opaque-blob".to_owned(),
                    },
                    0,
                );

                assert_eq!(
                    redacted,
                    BlockAccumulator::RedactedThinking {
                        data: "opaque-blob".to_owned()
                    },
                );
            }

            #[test]
            fn skips_unknown_blocks_and_mcp_connector_results() {
                for info in [
                    ContentBlockInfo::Unknown {
                        r#type: "some_future_block".to_owned(),
                    },
                    ContentBlockInfo::McpToolResult {},
                ] {
                    let block = BlockAccumulator::start(info, 0);

                    assert_eq!(block, BlockAccumulator::Skipped);
                }
            }
        }

        mod apply {
            use super::*;

            #[test]
            fn appends_thinking_and_replaces_the_signature() {
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
                assert_eq!(
                    block,
                    BlockAccumulator::Thinking {
                        thinking: "step 1, step 2".to_owned(),
                        signature: "sig_abc".to_owned(),
                    },
                );
            }

            #[test]
            fn ignores_a_delta_of_another_block_kind() {
                let mut block = BlockAccumulator::Text("hi".to_owned());

                let forwarded = block.apply(Delta::InputJsonDelta {
                    partial_json: "ignored".to_owned(),
                });

                assert_eq!(forwarded, None);
                assert_eq!(block, BlockAccumulator::Text("hi".to_owned()));
            }
        }

        mod into_content_block {
            use super::*;

            #[test]
            fn yields_text_verbatim_and_reasoning_for_its_model() {
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
        }
    }
}
