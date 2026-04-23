//! Agent turn loop.
//!
//! Drives one user → assistant round: streams the model response,
//! dispatches any tool calls it emits, records each turn to the
//! session, and stops when the model returns text only or the safety
//! cap [`MAX_TOOL_ROUNDS`] trips.

pub(crate) mod event;

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use crate::agent::event::{AgentEvent, AgentSink};
use crate::client::anthropic::{Client, ContentBlockInfo, Delta, StreamEvent};
use crate::message::{ContentBlock, Message, Role, strip_trailing_thinking};
use crate::prompt::PromptParts;
use crate::session::manager::SessionManager;
use crate::session::writer::record_session_message;
use crate::tool::{ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

const MAX_TOOL_ROUNDS: usize = 25;

// ── Agent Client ──

/// Streaming surface the agent loop needs from a model client. Narrower
/// than [`Client`][crate::client::anthropic::Client] (which also owns
/// non-streaming `complete`, headers, auth) so in-process fakes can
/// drive [`agent_turn`] with scripted [`StreamEvent`]s in tests.
pub(crate) trait AgentClient: Send + Sync {
    fn stream_message(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>>;
}

impl AgentClient for Client {
    fn stream_message(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        Client::stream_message(self, messages, system_sections, user_context, tools)
    }
}

// ── Agent Turn ──

/// Drives one user → assistant turn, executing any tool calls the model
/// emits and looping until the model produces a text-only response or the
/// safety cap [`MAX_TOOL_ROUNDS`] is exceeded. Records each assistant /
/// tool-result message to `session` as it completes.
pub(crate) async fn agent_turn(
    client: &dyn AgentClient,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    prompt: &PromptParts,
    sink: &dyn AgentSink,
    session: &Mutex<SessionManager>,
) -> Result<()> {
    let tool_defs = tools.definitions();

    for _ in 0..MAX_TOOL_ROUNDS {
        strip_trailing_thinking(messages);
        let blocks = stream_response(client, messages, &tool_defs, prompt, sink).await?;

        let tool_uses: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();

        let assistant_msg = Message {
            role: Role::Assistant,
            content: blocks,
        };
        record_session_message(session, &assistant_msg, Some(sink)).await;
        messages.push(assistant_msg);

        if tool_uses.is_empty() {
            return Ok(());
        }

        let mut results = Vec::new();
        let mut sidecars: Vec<(String, ToolMetadata)> = Vec::new();
        for (id, name, input) in tool_uses {
            _ = sink.send(AgentEvent::ToolCallStart {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });

            let output = match tools.get(&name) {
                Some(t) => t.run(input).await,
                None => ToolOutput {
                    content: format!("Unknown tool: {name}"),
                    is_error: true,
                    metadata: ToolMetadata::default(),
                },
            };

            _ = sink.send(AgentEvent::ToolCallEnd {
                id: id.clone(),
                content: output.content.clone(),
                is_error: output.is_error,
                metadata: output.metadata.clone(),
            });

            sidecars.push((id.clone(), output.metadata));
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: output.content,
                is_error: output.is_error,
            });
        }

        let tool_result_msg = Message {
            role: Role::User,
            content: results,
        };
        record_session_message(session, &tool_result_msg, Some(sink)).await;
        // Sidecar metadata is written immediately after the message
        // so a mid-turn crash can still recover the display info for
        // results that did land. Each entry is independent — a single
        // failure doesn't abort the batch.
        {
            let mut s = session.lock().await;
            for (id, metadata) in &sidecars {
                let r = s.record_tool_result_metadata(id, metadata);
                crate::session::writer::log_session_err(r, &mut s, Some(sink));
            }
        }
        messages.push(tool_result_msg);
    }

    bail!(
        "agent stopped after {MAX_TOOL_ROUNDS} tool rounds without a final response \
         — this is a safety cap against runaway loops. Ask again with a narrower request."
    )
}

// ── Stream Processing ──

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
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Placeholder for unrecognized content block types. Absorbs deltas silently
    /// and produces no [`ContentBlock`] at the end.
    Skipped,
}

impl BlockAccumulator {
    fn into_content_block(self) -> Option<ContentBlock> {
        match self {
            Self::Text(text) => Some(ContentBlock::Text { text }),
            Self::ToolUse { id, name, json_buf } => Some(ContentBlock::ToolUse {
                id,
                name,
                input: parse_tool_json(&json_buf),
            }),
            Self::ServerToolUse { id, name, json_buf } => Some(ContentBlock::ServerToolUse {
                id,
                name,
                input: parse_tool_json(&json_buf),
            }),
            Self::Thinking {
                thinking,
                signature,
            } => Some(ContentBlock::Thinking {
                thinking,
                signature,
            }),
            Self::RedactedThinking { data } => Some(ContentBlock::RedactedThinking { data }),
            Self::Skipped => None,
        }
    }
}

fn parse_tool_json(json_buf: &str) -> serde_json::Value {
    serde_json::from_str(json_buf).unwrap_or_else(|e| {
        warn!("malformed tool input JSON: {e}");
        serde_json::Value::Object(serde_json::Map::new())
    })
}

async fn stream_response(
    client: &dyn AgentClient,
    messages: &[Message],
    tools: &[ToolDefinition],
    prompt: &PromptParts,
    sink: &dyn AgentSink,
) -> Result<Vec<ContentBlock>> {
    let section_refs: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
    let mut rx = client.stream_message(
        messages,
        &section_refs,
        prompt.user_context.as_deref(),
        tools,
    )?;

    let mut blocks: Vec<Option<BlockAccumulator>> = Vec::new();

    while let Some(event) = rx.recv().await {
        let event = event.context("stream error")?;

        match event {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if blocks.len() <= index {
                    blocks.resize_with(index + 1, || None);
                }
                let acc = init_accumulator(content_block, index);
                // Send initial text to display if non-empty (the API
                // typically sends empty initial text, but be safe).
                if let BlockAccumulator::Text(text) = &acc
                    && !text.is_empty()
                {
                    // Display-only; authoritative content stays in `acc`.
                    _ = sink.send(AgentEvent::StreamToken(text.clone()));
                }
                blocks[index] = Some(acc);
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(Some(block)) = blocks.get_mut(index) {
                    apply_delta(block, delta, sink);
                }
            }
            StreamEvent::Error { error } => {
                bail!("API error ({}): {}", error.error_type, error.message);
            }
            _ => {}
        }
    }

    Ok(blocks
        .into_iter()
        .flatten()
        .filter_map(BlockAccumulator::into_content_block)
        .collect())
}

fn init_accumulator(content_block: ContentBlockInfo, index: usize) -> BlockAccumulator {
    match content_block {
        ContentBlockInfo::Text { text } => BlockAccumulator::Text(text),
        ContentBlockInfo::ToolUse { id, name } => BlockAccumulator::ToolUse {
            id,
            name,
            json_buf: String::new(),
        },
        ContentBlockInfo::ServerToolUse { id, name } => BlockAccumulator::ServerToolUse {
            id,
            name,
            json_buf: String::new(),
        },
        ContentBlockInfo::Thinking {
            thinking,
            signature,
        } => BlockAccumulator::Thinking {
            thinking,
            signature,
        },
        ContentBlockInfo::RedactedThinking { data } => BlockAccumulator::RedactedThinking { data },
        ContentBlockInfo::Unknown => {
            warn!("skipping unknown content block at index {index}");
            BlockAccumulator::Skipped
        }
    }
}

fn apply_delta(block: &mut BlockAccumulator, delta: Delta, sink: &dyn AgentSink) {
    match (block, delta) {
        (BlockAccumulator::Text(buf), Delta::TextDelta { text }) => {
            buf.push_str(&text);
            // Display-only; authoritative content stays in `buf`.
            _ = sink.send(AgentEvent::StreamToken(text));
        }
        (
            BlockAccumulator::ToolUse { json_buf, .. }
            | BlockAccumulator::ServerToolUse { json_buf, .. },
            Delta::InputJsonDelta { partial_json },
        ) => {
            json_buf.push_str(&partial_json);
        }
        (
            BlockAccumulator::Thinking { thinking, .. },
            Delta::ThinkingDelta {
                thinking: thinking_delta,
            },
        ) => {
            thinking.push_str(&thinking_delta);
            _ = sink.send(AgentEvent::ThinkingToken(thinking_delta));
        }
        (
            BlockAccumulator::Thinking { signature, .. },
            Delta::SignatureDelta {
                signature: sig_value,
            },
        ) => {
            *signature = sig_value;
        }
        (block, delta) => {
            debug!(?block, ?delta, "ignoring unhandled delta");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::client::anthropic::{
        ApiError, ContentBlockInfo, MessageResponse, StreamEvent, Usage, test_client,
    };
    use crate::config::Auth;
    use crate::message::Role;
    use crate::session::manager::SessionManager;
    use crate::session::store::test_store;
    use crate::tool::{Tool, ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

    // ── agent_turn ──

    /// In-process fake that hands the agent loop a scripted sequence of
    /// [`StreamEvent`]s per turn.
    struct FakeClient {
        turns: StdMutex<VecDeque<Vec<StreamEvent>>>,
    }

    impl FakeClient {
        fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                turns: StdMutex::new(turns.into()),
            }
        }
    }

    impl AgentClient for FakeClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
            let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
            let (tx, rx) = mpsc::channel(events.len().max(1));
            for event in events {
                tx.try_send(Ok(event)).expect("channel capacity");
            }
            Ok(rx)
        }
    }

    fn text_turn(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message: MessageResponse {
                    id: "msg_1".into(),
                    model: "claude-sonnet-4-6".into(),
                    usage: Some(Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    }),
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockInfo::Text {
                    text: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::TextDelta { text: text.into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageStop,
        ]
    }

    fn tool_use_turn(id: &str, name: &str, input_json: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockInfo::ToolUse {
                    id: id.into(),
                    name: name.into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::InputJsonDelta {
                    partial_json: input_json.into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageStop,
        ]
    }

    /// Tool that echoes its input. Exercises the agent's tool-dispatch
    /// and result-plumbing path without any subprocess machinery.
    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> &'static str {
            "echo the input"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn run(
            &self,
            input: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            Box::pin(async move {
                ToolOutput {
                    content: input.to_string(),
                    is_error: false,
                    metadata: ToolMetadata {
                        title: Some("echoed".into()),
                        ..Default::default()
                    },
                }
            })
        }
    }

    fn empty_prompt() -> PromptParts {
        PromptParts {
            system_sections: vec![],
            user_context: None,
        }
    }

    fn test_session(dir: &std::path::Path) -> Mutex<SessionManager> {
        let store = test_store(dir);
        Mutex::new(SessionManager::start(&store, "claude-sonnet-4-6"))
    }

    #[tokio::test]
    async fn agent_turn_text_only_response_records_assistant_message_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("Hello there!")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(
            matches!(&messages[1].content[0], ContentBlock::Text { text } if text == "Hello there!"),
        );
        let streamed: Vec<String> = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::StreamToken(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, ["Hello there!"]);
    }

    #[tokio::test]
    async fn agent_turn_single_tool_call_dispatches_and_completes_on_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "echo", r#"{"v":42}"#),
            text_turn("Done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("run echo")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .unwrap();

        // Message ordering: user → assistant(tool_use) → user(tool_result) → assistant(text).
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::ToolUse { name, .. } if name == "echo",
        ));
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = &messages[2].content[0]
        else {
            panic!("expected ToolResult, got {:?}", messages[2].content[0]);
        };
        assert_eq!(tool_use_id, "tool_1");
        assert_eq!(content, r#"{"v":42}"#);
        assert!(!is_error);
        assert!(matches!(
            &messages[3].content[0],
            ContentBlock::Text { text } if text == "Done",
        ));

        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallStart { id, name, .. } if id == "tool_1" && name == "echo",
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallEnd { id, metadata, is_error: false, .. }
                if id == "tool_1" && metadata.title.as_deref() == Some("echoed"),
        )));
    }

    #[tokio::test]
    async fn agent_turn_unknown_tool_name_emits_error_result_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "nonexistent", r"{}"),
            text_turn("fallback"),
        ]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .unwrap();

        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &messages[2].content[0]
        else {
            panic!("expected ToolResult");
        };
        assert!(is_error, "unknown tool marks tool_result as error");
        assert!(
            content.contains("Unknown tool: nonexistent"),
            "error content: {content}",
        );
    }

    #[tokio::test]
    async fn agent_turn_max_tool_rounds_bails_with_safety_cap_message() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let turns: Vec<Vec<StreamEvent>> = (0..MAX_TOOL_ROUNDS)
            .map(|i| tool_use_turn(&format!("tool_{i}"), "echo", r"{}"))
            .collect();
        let client = FakeClient::new(turns);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("loop forever")];

        let err = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .expect_err("cap must trip");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&MAX_TOOL_ROUNDS.to_string()),
            "cap in error: {msg}"
        );
        assert!(msg.contains("safety cap"), "explains intent: {msg}");
    }

    #[tokio::test]
    async fn agent_turn_mid_stream_error_event_surfaces_as_bail() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![vec![StreamEvent::Error {
            error: ApiError {
                error_type: "overloaded_error".into(),
                message: "Servers overloaded".into(),
            },
        }]]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        let err = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .expect_err("api error must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("overloaded_error"), "type in error: {msg}");
        assert!(
            msg.contains("Servers overloaded"),
            "message in error: {msg}"
        );
    }

    #[tokio::test]
    async fn agent_turn_strips_trailing_thinking_before_next_round() {
        // A trailing thinking block is legal on the first round but
        // rejected by the API on the second — agent_turn must strip it
        // before the follow-up turn.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("done")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "intermediate".into(),
                    },
                    ContentBlock::Thinking {
                        thinking: "reasoning".into(),
                        signature: "sig".into(),
                    },
                ],
            },
        ];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .unwrap();

        let stripped = &messages[1];
        assert_eq!(stripped.content.len(), 1);
        assert!(matches!(&stripped.content[0], ContentBlock::Text { .. }));
    }

    /// Covers `<Client as AgentClient>::stream_message` on the real
    /// production path; the `FakeClient` tests above stub the trait.
    #[tokio::test]
    async fn agent_turn_drives_real_client_over_wiremock() {
        let server = MockServer::start().await;
        let body = indoc::indoc! {r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-6","usage":{"input_tokens":5,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_stop
data: {"type":"message_stop"}

"#};
        Mock::given(method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let client = test_client(
            server.uri(),
            Auth::ApiKey("sk".to_owned()),
            "claude-sonnet-4-6",
        );

        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[1].content[0], ContentBlock::Text { text } if text == "hello"),);
    }
    // ── BlockAccumulator::into_content_block ──

    #[test]
    fn into_content_block_text_yields_text_block() {
        let block = BlockAccumulator::Text("hi".to_owned()).into_content_block();
        assert!(matches!(block, Some(ContentBlock::Text { text }) if text == "hi"));
    }

    #[test]
    fn into_content_block_tool_use_yields_tool_use_block() {
        let block = BlockAccumulator::ToolUse {
            id: "tool_1".to_owned(),
            name: "bash".to_owned(),
            json_buf: r#"{"command": "ls"}"#.to_owned(),
        }
        .into_content_block();
        let Some(ContentBlock::ToolUse { id, name, input }) = block else {
            panic!("expected ToolUse, got {block:?}");
        };
        assert_eq!(id, "tool_1");
        assert_eq!(name, "bash");
        assert_eq!(input, json!({"command": "ls"}));
    }

    #[test]
    fn into_content_block_server_tool_use_yields_server_tool_use_block() {
        let block = BlockAccumulator::ServerToolUse {
            id: "srv_1".to_owned(),
            name: "web_search".to_owned(),
            json_buf: r#"{"query": "rust"}"#.to_owned(),
        }
        .into_content_block();
        let Some(ContentBlock::ServerToolUse { id, name, input }) = block else {
            panic!("expected ServerToolUse, got {block:?}");
        };
        assert_eq!(id, "srv_1");
        assert_eq!(name, "web_search");
        assert_eq!(input, json!({"query": "rust"}));
    }

    #[test]
    fn into_content_block_thinking_preserves_signature() {
        let block = BlockAccumulator::Thinking {
            thinking: "step 1".to_owned(),
            signature: "sig_abc".to_owned(),
        }
        .into_content_block();
        let Some(ContentBlock::Thinking {
            thinking,
            signature,
        }) = block
        else {
            panic!("expected Thinking, got {block:?}");
        };
        assert_eq!(thinking, "step 1");
        assert_eq!(signature, "sig_abc");
    }

    #[test]
    fn into_content_block_redacted_thinking_preserves_data() {
        let block = BlockAccumulator::RedactedThinking {
            data: "opaque-blob".to_owned(),
        }
        .into_content_block();
        assert!(
            matches!(block, Some(ContentBlock::RedactedThinking { data }) if data == "opaque-blob")
        );
    }

    #[test]
    fn into_content_block_skipped_yields_none() {
        assert!(BlockAccumulator::Skipped.into_content_block().is_none());
    }

    // ── parse_tool_json ──

    #[test]
    fn parse_tool_json_valid_object() {
        let value = parse_tool_json(r#"{"command": "ls", "n": 3}"#);
        assert_eq!(value, json!({"command": "ls", "n": 3}));
    }

    #[test]
    fn parse_tool_json_malformed() {
        let value = parse_tool_json("{unclosed");
        assert_eq!(value, json!({}));
    }

    // ── init_accumulator ──

    #[test]
    fn init_accumulator_text_starts_with_initial_text() {
        let acc = init_accumulator(
            ContentBlockInfo::Text {
                text: "hi".to_owned(),
            },
            0,
        );
        assert!(matches!(acc, BlockAccumulator::Text(t) if t == "hi"));
    }

    #[test]
    fn init_accumulator_tool_use_starts_with_empty_buf() {
        let acc = init_accumulator(
            ContentBlockInfo::ToolUse {
                id: "tool_1".to_owned(),
                name: "bash".to_owned(),
            },
            0,
        );
        let BlockAccumulator::ToolUse { id, name, json_buf } = acc else {
            panic!("expected ToolUse, got {acc:?}");
        };
        assert_eq!(id, "tool_1");
        assert_eq!(name, "bash");
        assert!(json_buf.is_empty());
    }

    #[test]
    fn init_accumulator_server_tool_use_starts_with_empty_buf() {
        let acc = init_accumulator(
            ContentBlockInfo::ServerToolUse {
                id: "srv_1".to_owned(),
                name: "web_search".to_owned(),
            },
            0,
        );
        let BlockAccumulator::ServerToolUse { id, name, json_buf } = acc else {
            panic!("expected ServerToolUse, got {acc:?}");
        };
        assert_eq!(id, "srv_1");
        assert_eq!(name, "web_search");
        assert!(json_buf.is_empty());
    }

    #[test]
    fn init_accumulator_thinking_preserves_fields() {
        let acc = init_accumulator(
            ContentBlockInfo::Thinking {
                thinking: "step 1".to_owned(),
                signature: "sig_abc".to_owned(),
            },
            0,
        );
        let BlockAccumulator::Thinking {
            thinking,
            signature,
        } = acc
        else {
            panic!("expected Thinking, got {acc:?}");
        };
        assert_eq!(thinking, "step 1");
        assert_eq!(signature, "sig_abc");
    }

    #[test]
    fn init_accumulator_redacted_thinking_preserves_data() {
        let acc = init_accumulator(
            ContentBlockInfo::RedactedThinking {
                data: "opaque-blob".to_owned(),
            },
            0,
        );
        assert!(
            matches!(acc, BlockAccumulator::RedactedThinking { data } if data == "opaque-blob")
        );
    }

    #[test]
    fn init_accumulator_unknown_yields_skipped() {
        let acc = init_accumulator(ContentBlockInfo::Unknown, 0);
        assert!(matches!(acc, BlockAccumulator::Skipped));
    }

    // ── apply_delta ──

    #[test]
    fn apply_delta_text_appends_and_emits_stream_token() {
        let sink = CapturingSink::new();
        let mut block = BlockAccumulator::Text("ha".to_owned());
        apply_delta(
            &mut block,
            Delta::TextDelta {
                text: "llo".to_owned(),
            },
            &sink,
        );
        let BlockAccumulator::Text(buf) = &block else {
            panic!("expected Text, got {block:?}");
        };
        assert_eq!(buf, "hallo");
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::StreamToken(t) if t == "llo"));
    }

    #[test]
    fn apply_delta_tool_use_appends_to_json_buf() {
        let sink = CapturingSink::new();
        let mut block = BlockAccumulator::ToolUse {
            id: "tool_1".to_owned(),
            name: "bash".to_owned(),
            json_buf: r#"{"x"#.to_owned(),
        };
        apply_delta(
            &mut block,
            Delta::InputJsonDelta {
                partial_json: r":1}".to_owned(),
            },
            &sink,
        );
        let BlockAccumulator::ToolUse { json_buf, .. } = &block else {
            panic!("expected ToolUse, got {block:?}");
        };
        assert_eq!(json_buf, r#"{"x:1}"#);
        assert!(sink.events().is_empty());
    }

    #[test]
    fn apply_delta_server_tool_use_appends_to_json_buf() {
        let sink = CapturingSink::new();
        let mut block = BlockAccumulator::ServerToolUse {
            id: "srv_1".to_owned(),
            name: "web_search".to_owned(),
            json_buf: r#"{"q"#.to_owned(),
        };
        apply_delta(
            &mut block,
            Delta::InputJsonDelta {
                partial_json: r#":"rust"}"#.to_owned(),
            },
            &sink,
        );
        let BlockAccumulator::ServerToolUse { json_buf, .. } = &block else {
            panic!("expected ServerToolUse, got {block:?}");
        };
        assert_eq!(json_buf, r#"{"q:"rust"}"#);
        assert!(sink.events().is_empty());
    }

    #[test]
    fn apply_delta_thinking_appends_and_emits_thinking_token() {
        let sink = CapturingSink::new();
        let mut block = BlockAccumulator::Thinking {
            thinking: "step 1".to_owned(),
            signature: String::new(),
        };
        apply_delta(
            &mut block,
            Delta::ThinkingDelta {
                thinking: ", step 2".to_owned(),
            },
            &sink,
        );
        let BlockAccumulator::Thinking { thinking, .. } = &block else {
            panic!("expected Thinking, got {block:?}");
        };
        assert_eq!(thinking, "step 1, step 2");
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::ThinkingToken(t) if t == ", step 2"));
    }

    #[test]
    fn apply_delta_signature_updates_signature_field() {
        let sink = CapturingSink::new();
        let mut block = BlockAccumulator::Thinking {
            thinking: "step 1".to_owned(),
            signature: String::new(),
        };
        apply_delta(
            &mut block,
            Delta::SignatureDelta {
                signature: "sig_abc".to_owned(),
            },
            &sink,
        );
        let BlockAccumulator::Thinking {
            thinking,
            signature,
        } = &block
        else {
            panic!("expected Thinking, got {block:?}");
        };
        assert_eq!(thinking, "step 1");
        assert_eq!(signature, "sig_abc");
        assert!(sink.events().is_empty());
    }

    #[test]
    fn apply_delta_mismatched_pair_is_a_noop() {
        let sink = CapturingSink::new();
        let mut block = BlockAccumulator::Text("hi".to_owned());
        apply_delta(
            &mut block,
            Delta::InputJsonDelta {
                partial_json: "ignored".to_owned(),
            },
            &sink,
        );
        let BlockAccumulator::Text(buf) = &block else {
            panic!("expected Text, got {block:?}");
        };
        assert_eq!(buf, "hi");
        assert!(sink.events().is_empty());
    }
}
