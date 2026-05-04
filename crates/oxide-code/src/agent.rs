//! Agent turn loop.
//!
//! Drives one user → assistant round: streams the model response,
//! dispatches any tool calls it emits, records each turn to the
//! session, and stops when the model returns text only or the safety
//! cap [`MAX_TOOL_ROUNDS`] trips.

pub(crate) mod event;

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::agent::event::{AgentEvent, AgentSink, UserAction};
use crate::client::anthropic::Client;
use crate::client::anthropic::wire::{ContentBlockInfo, Delta, StreamEvent};
use crate::message::{ContentBlock, Message, Role, strip_trailing_thinking};
use crate::prompt::PromptParts;
use crate::session::handle::{RecordOutcome, SessionHandle};
use crate::tool::{ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

const MAX_TOOL_ROUNDS: usize = 25;

// ── Turn Abort ──

/// Reasons a turn ends before normal completion.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnAbort {
    #[error("turn cancelled")]
    Cancelled,
    #[error("turn quit")]
    Quit,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

type AbortResult<T> = std::result::Result<T, TurnAbort>;

// ── Agent Client ──

/// Narrow streaming trait so in-process fakes can drive [`agent_turn`].
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

/// Drives one user→assistant turn until text-only response or safety cap.
/// Long-running awaits race `user_rx` for cancel / quit / mid-turn submit.
pub(crate) async fn agent_turn(
    client: &dyn AgentClient,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    prompt: &PromptParts,
    sink: &dyn AgentSink,
    session: &SessionHandle,
    user_rx: &mut mpsc::Receiver<UserAction>,
) -> AbortResult<()> {
    let tool_defs = tools.definitions();
    let mut pending_prompts: Vec<String> = Vec::new();

    for _ in 0..MAX_TOOL_ROUNDS {
        strip_trailing_thinking(messages);
        let StreamOutcome {
            blocks,
            parse_errors,
        } = await_unless_aborted(
            stream_response(client, messages, &tool_defs, prompt, sink),
            user_rx,
            &mut pending_prompts,
        )
        .await??;

        let tool_uses = collect_tool_uses(&blocks);
        let assistant_msg = Message {
            role: Role::Assistant,
            content: blocks,
        };

        if tool_uses.is_empty() {
            // Queued prompts drain on the TUI side at idle.
            record_message(session, assistant_msg.clone(), sink).await;
            messages.push(assistant_msg);
            return Ok(());
        }

        let (results, sidecars) = run_tool_round(
            tools,
            tool_uses,
            &parse_errors,
            sink,
            user_rx,
            &mut pending_prompts,
        )
        .await?;
        let tool_result_msg = Message {
            role: Role::User,
            content: results,
        };

        commit_round_writes(session, sink, &assistant_msg, &tool_result_msg, sidecars).await;
        messages.push(assistant_msg);
        messages.push(tool_result_msg);
        record_drained_prompts(pending_prompts.drain(..), messages, session, sink).await;
    }

    Err(TurnAbort::Failed(anyhow!(
        "agent stopped after {MAX_TOOL_ROUNDS} tool rounds without a final response \
         — this is a safety cap against runaway loops. Ask again with a narrower request."
    )))
}

fn collect_tool_uses(blocks: &[ContentBlock]) -> Vec<(String, String, serde_json::Value)> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Dispatches tool calls, racing each against `user_rx` for abort signals.
async fn run_tool_round(
    tools: &ToolRegistry,
    tool_uses: Vec<(String, String, serde_json::Value)>,
    parse_errors: &HashMap<String, String>,
    sink: &dyn AgentSink,
    user_rx: &mut mpsc::Receiver<UserAction>,
    pending: &mut Vec<String>,
) -> AbortResult<(Vec<ContentBlock>, Vec<(String, ToolMetadata)>)> {
    let mut results = Vec::with_capacity(tool_uses.len());
    let mut sidecars: Vec<(String, ToolMetadata)> = Vec::with_capacity(tool_uses.len());
    for (id, name, input) in tool_uses {
        _ = sink.send(AgentEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        });

        let output =
            dispatch_tool_call(tools, &name, input, parse_errors.get(&id), user_rx, pending)
                .await?;

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
    Ok((results, sidecars))
}

/// Persists the round's messages + metadata in one coalesced flush.
async fn commit_round_writes(
    session: &SessionHandle,
    sink: &dyn AgentSink,
    assistant_msg: &Message,
    tool_result_msg: &Message,
    sidecars: Vec<(String, ToolMetadata)>,
) {
    let assistant_fut = session.record_message(assistant_msg.clone());
    let tool_result_fut = session.record_message(tool_result_msg.clone());
    let metadata_fut = session.record_tool_metadata_batch(sidecars);
    let (assistant_outcome, tool_result_outcome, metadata_outcome) =
        tokio::join!(assistant_fut, tool_result_fut, metadata_fut);
    sink.session_write_error(assistant_outcome.failure.as_deref());
    sink.session_write_error(tool_result_outcome.failure.as_deref());
    sink.session_write_error(metadata_outcome.failure.as_deref());
}

/// Dispatches one tool call; short-circuits to error result on malformed input.
async fn dispatch_tool_call(
    tools: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    parse_error: Option<&String>,
    user_rx: &mut mpsc::Receiver<UserAction>,
    pending: &mut Vec<String>,
) -> AbortResult<ToolOutput> {
    if let Some(err) = parse_error {
        return Ok(ToolOutput {
            content: format!("tool input JSON failed to parse: {err}; retry with a valid object"),
            is_error: true,
            metadata: ToolMetadata::default(),
        });
    }
    await_unless_aborted(tools.run(name, input), user_rx, pending).await
}

/// Splices queued prompts into the conversation and emits `PromptDrained` per item, in order.
async fn record_drained_prompts(
    texts: impl IntoIterator<Item = String>,
    messages: &mut Vec<Message>,
    session: &SessionHandle,
    sink: &dyn AgentSink,
) {
    for text in texts {
        let queued_msg = Message::user(text.clone());
        record_message(session, queued_msg.clone(), sink).await;
        messages.push(queued_msg);
        _ = sink.send(AgentEvent::PromptDrained(text));
    }
}

/// Races `fut` against user actions. Cancel / quit → `TurnAbort`; submits buffer into `pending`.
/// `fut` must be cancel-safe.
async fn await_unless_aborted<F, T>(
    fut: F,
    user_rx: &mut mpsc::Receiver<UserAction>,
    pending: &mut Vec<String>,
) -> AbortResult<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        tokio::select! {
            // Biased: user actions observed before same-poll-ready futures.
            biased;
            action = user_rx.recv() => match action {
                Some(UserAction::Cancel) => return Err(TurnAbort::Cancelled),
                Some(UserAction::Quit) | None => return Err(TurnAbort::Quit),
                Some(UserAction::SubmitPrompt(text)) => pending.push(text),
                // Unreachable under current wiring; log so regressions surface.
                Some(
                    action @ (UserAction::ConfirmExit
                    | UserAction::Clear
                    | UserAction::SwapConfig { .. }),
                ) => warn!("dropped mid-turn action: {action:?}"),
            },
            output = &mut fut => return Ok(output),
        }
    }
}

/// Surfaces the first I/O failure on `sink`; drops the AI-title seed
/// (only the fresh-start trigger in `main` consumes it).
async fn record_message(session: &SessionHandle, msg: Message, sink: &dyn AgentSink) {
    let outcome: RecordOutcome = session.record_message(msg).await;
    sink.session_write_error(outcome.failure.as_deref());
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
    /// Converts to a [`ContentBlock`], surfacing any tool-input parse error.
    fn into_content_block(self) -> (Option<ContentBlock>, Option<(String, String)>) {
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
            Self::Thinking {
                thinking,
                signature,
            } => (
                Some(ContentBlock::Thinking {
                    thinking,
                    signature,
                }),
                None,
            ),
            Self::RedactedThinking { data } => {
                (Some(ContentBlock::RedactedThinking { data }), None)
            }
            Self::Skipped => (None, None),
        }
    }
}

/// Parses tool JSON; returns empty object + error string on failure.
fn parse_tool_json(json_buf: &str) -> (serde_json::Value, Option<String>) {
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

#[derive(Debug, Default)]
struct StreamOutcome {
    blocks: Vec<ContentBlock>,
    parse_errors: HashMap<String, String>,
}

async fn stream_response(
    client: &dyn AgentClient,
    messages: &[Message],
    tools: &[ToolDefinition],
    prompt: &PromptParts,
    sink: &dyn AgentSink,
) -> Result<StreamOutcome> {
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
                if let BlockAccumulator::Text(text) = &acc
                    && !text.is_empty()
                {
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

    let mut outcome = StreamOutcome::default();
    for acc in blocks.into_iter().flatten() {
        let (block, parse_error) = acc.into_content_block();
        outcome.parse_errors.extend(parse_error);
        outcome.blocks.extend(block);
    }
    Ok(outcome)
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
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use serde_json::json;
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::client::anthropic::testing::test_client;
    use crate::client::anthropic::wire::{
        ApiError, ContentBlockInfo, MessageResponse, StreamEvent, Usage,
    };
    use crate::config::{Auth, Effort};
    use crate::message::Role;
    use crate::model::ResolvedModelId;
    use crate::session::handle::{self, SessionHandle};
    use crate::session::store::test_store;
    use crate::tool::{Tool, ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

    // ── TurnAbort ──

    #[test]
    fn turn_abort_display_alternate_propagates_anyhow_cause_chain() {
        // {abort:#} must delegate to the inner anyhow Error's alternate
        // form so the full cause chain reaches the user (e.g., when a
        // bare-REPL or headless run prints "Error: {e:#}"); plain {abort}
        // surfaces only the outermost context (anyhow's default Display).
        let inner = anyhow!("HTTP 503 from upstream");
        let chained = inner.context("stream error");
        let abort = TurnAbort::Failed(chained);

        let plain = format!("{abort}");
        let alternate = format!("{abort:#}");

        assert_eq!(plain, "stream error");
        assert!(
            alternate.contains("stream error") && alternate.contains("HTTP 503 from upstream"),
            "alternate must include both layers: {alternate:?}",
        );
    }

    #[test]
    fn turn_abort_display_cancelled_and_quit_use_static_labels() {
        assert_eq!(format!("{}", TurnAbort::Cancelled), "turn cancelled");
        assert_eq!(format!("{}", TurnAbort::Quit), "turn quit");
    }

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

    fn text_turn_with_initial_text(text: &str) -> Vec<StreamEvent> {
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
                content_block: ContentBlockInfo::Text { text: text.into() },
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

    /// Tool that signals when invoked then blocks forever. Lets cancel
    /// tests reliably wait until the agent is parked inside the tool
    /// future before sending the interrupt — without it `tokio::join!`
    /// races the cancel against the prior stream phase.
    struct GateTool {
        started: Arc<Notify>,
    }

    impl Tool for GateTool {
        fn name(&self) -> &'static str {
            "gate"
        }

        fn description(&self) -> &'static str {
            "blocks until the turn is cancelled"
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn run(
            &self,
            _input: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            let started = self.started.clone();
            Box::pin(async move {
                started.notify_one();
                std::future::pending::<ToolOutput>().await
            })
        }
    }

    /// Inert `UserAction` receiver for `agent_turn` tests that don't
    /// drive cancel / quit / submit signals. The sender is leaked so
    /// `recv()` stays pending for the test's lifetime; a tracked-leak
    /// alternative (returning the pair) costs every call site a `let`
    /// binding for no test-correctness benefit.
    fn inert_user_rx() -> mpsc::Receiver<UserAction> {
        let (tx, rx) = crate::agent::event::inert_user_action_channel();
        std::mem::forget(tx);
        rx
    }

    fn empty_prompt() -> PromptParts {
        PromptParts {
            system_sections: vec![],
            user_context: None,
        }
    }

    fn test_session(dir: &std::path::Path) -> SessionHandle {
        let store = test_store(dir);
        handle::start(&store, "claude-sonnet-4-6")
    }

    /// Handle whose actor channel is already closed; every write
    /// returns the actor-gone failure.
    fn dead_test_session() -> SessionHandle {
        crate::session::handle::testing::dead("dead-test-session")
    }

    #[tokio::test]
    async fn agent_turn_dead_session_surfaces_write_failure_on_first_call() {
        // Write errors must not abort the turn — agent_turn returns Ok
        // and emits exactly one Error event for the user.
        let session = dead_test_session();
        let client = FakeClient::new(vec![text_turn("Hello!")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![crate::message::Message::user("hi")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_user_rx(),
        )
        .await
        .unwrap();

        let events = sink.events();
        let error_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Error(m) if m.contains("Session write failed")))
            .collect();
        assert_eq!(
            error_events.len(),
            1,
            "exactly one write-failure Error event (sticky once-flag): {events:?}",
        );
    }

    #[tokio::test]
    async fn agent_turn_metadata_batch_failure_after_healthy_messages_surfaces_error() {
        // The assistant + tool-result messages succeed; the sidecar
        // batch is the first failing cmd, so the batch's failure
        // handler (not the message handler) is what fires the Error
        // event. Programmable handle: ack the first 2 cmds healthily
        // then drop every cmd without acking — the 3rd cmd
        // (ToolMetadata) hits dispatch_outcome's rx-await fallback.
        let session = crate::session::handle::testing::acks_then_drops("metadata-batch-fails", 2);
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "echo", r#"{"v":1}"#),
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
            &mut inert_user_rx(),
        )
        .await
        .unwrap();

        let events = sink.events();
        let error_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Error(m) if m.contains("Session write failed")))
            .collect();
        assert_eq!(
            error_events.len(),
            1,
            "exactly one write-failure Error event (sticky once-flag): {events:?}",
        );
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
            &mut inert_user_rx(),
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
    async fn agent_turn_initial_text_block_streams_without_delta() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn_with_initial_text("Hello immediately")]);
        let tools = ToolRegistry::new(Vec::new());
        let sink = CapturingSink::new();
        let mut user_rx = inert_user_rx();
        let mut messages = vec![Message::user("hi")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut user_rx,
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 2);
        assert!(
            matches!(&messages[1].content[0], ContentBlock::Text { text } if text == "Hello immediately"),
        );
        let streamed: Vec<String> = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::StreamToken(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, ["Hello immediately"]);
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
            &mut inert_user_rx(),
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
    async fn agent_turn_drains_mid_round_submit_into_messages_at_round_boundary() {
        // Round 1 emits a tool_use; we pre-load the user channel with a
        // SubmitPrompt so `await_unless_aborted` consumes it during the
        // round (either while the SSE stream produces frames or while
        // the tool runs). At the round boundary the agent must splice
        // the queued text into `messages` as a trailing user message
        // and emit a `PromptDrained` event, then proceed to round 2
        // which is text-only.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "echo", r#"{"v":1}"#),
            text_turn("done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("kick off")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(4);
        // Hold the sender until the test ends so `recv()` after the
        // queued submit stays pending instead of resolving to `None`.
        tx.send(UserAction::SubmitPrompt("follow up".into()))
            .await
            .unwrap();

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
        )
        .await
        .expect("turn must complete");

        // user → assistant(tool_use) → user(tool_result) → user("follow up") → assistant("done").
        assert_eq!(
            messages.len(),
            5,
            "expected 5 messages including the drained prompt: {messages:#?}",
        );
        assert_eq!(messages[3].role, Role::User);
        assert!(
            matches!(
                &messages[3].content[0],
                ContentBlock::Text { text } if text == "follow up",
            ),
            "drained prompt must land between tool_result and round 2: {:?}",
            messages[3],
        );

        let drained_count = sink
            .events()
            .iter()
            .filter(|e| matches!(e, AgentEvent::PromptDrained(t) if t == "follow up"))
            .count();
        assert_eq!(
            drained_count, 1,
            "exactly one PromptDrained event for the queued prompt",
        );
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_drains_multiple_mid_round_submits_in_order() {
        // Two SubmitPrompts arrive during the same tool's await; both must
        // land as separate trailing User messages in dispatch order, and
        // the agent must emit one PromptDrained event per item.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "echo", r#"{"v":1}"#),
            text_turn("done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("kick off")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(4);
        tx.send(UserAction::SubmitPrompt("first".into()))
            .await
            .unwrap();
        tx.send(UserAction::SubmitPrompt("second".into()))
            .await
            .unwrap();

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
        )
        .await
        .expect("turn must complete");

        // user → assistant(tool_use) → user(tool_result) → user("first")
        // → user("second") → assistant("done").
        assert_eq!(messages.len(), 6, "{messages:#?}");
        assert!(matches!(
            &messages[3].content[0],
            ContentBlock::Text { text } if text == "first",
        ));
        assert!(matches!(
            &messages[4].content[0],
            ContentBlock::Text { text } if text == "second",
        ));

        let drained: Vec<_> = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::PromptDrained(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(drained, vec!["first".to_owned(), "second".to_owned()]);
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_cancel_during_stream_is_cancelled_abort() {
        // Biased select picks the queued Cancel before the synchronous
        // stream future, so the turn never produces an assistant
        // message. The session must stay at its pre-turn tail and the
        // abort must be Cancelled (not a `Failed` error).
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("never reached")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(1);
        tx.try_send(UserAction::Cancel).unwrap();

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
        )
        .await
        .expect_err("cancel must surface as Err(Cancelled)");

        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        assert_eq!(messages.len(), 1, "no assistant message recorded");
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_quit_during_stream_is_quit_abort() {
        // `Quit` is the explicit teardown signal; the agent loop relies
        // on it to break out of its outer driver. Pre-queueing it must
        // surface as Err(Quit) so callers don't conflate it with cancel.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("never reached")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(1);
        tx.try_send(UserAction::Quit).unwrap();

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
        )
        .await
        .expect_err("quit must surface as Err(Quit)");

        assert!(matches!(abort, TurnAbort::Quit), "got {abort:?}");
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_sender_drop_during_turn_collapses_to_quit_abort() {
        // When every `UserAction` sender drops, `recv()` resolves to
        // `None`. The agent treats it as Quit so the outer loop can
        // exit cleanly instead of looping on a dead channel.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("never reached")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(1);
        drop(tx);

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
        )
        .await
        .expect_err("dead channel must surface as Err(Quit)");

        assert!(matches!(abort, TurnAbort::Quit), "got {abort:?}");
    }

    #[tokio::test]
    async fn agent_turn_absorbs_stragglers_without_killing_turn() {
        // Every variant in the catch-all arm of `await_unless_aborted`
        // must let an in-flight turn complete. A future change that
        // returns `TurnAbort::Cancelled` for any of these (because they
        // "feel" mutating) would kill the turn from a buffered no-op.
        for action in [
            UserAction::ConfirmExit,
            UserAction::Clear,
            UserAction::SwapConfig {
                model: Some(ResolvedModelId::new("claude-opus-4-7".to_owned())),
                effort: None,
            },
            UserAction::SwapConfig {
                model: None,
                effort: Some(Effort::High),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let session = test_session(dir.path());
            let client = FakeClient::new(vec![text_turn("Hello!")]);
            let tools = ToolRegistry::new(vec![]);
            let sink = CapturingSink::new();
            let mut messages = vec![Message::user("hi")];
            let (tx, mut rx) = mpsc::channel::<UserAction>(1);
            tx.try_send(action.clone()).unwrap();

            agent_turn(
                &client,
                &tools,
                &mut messages,
                &empty_prompt(),
                &sink,
                &session,
                &mut rx,
            )
            .await
            .unwrap_or_else(|_| panic!("turn must complete despite {action:?}"));

            assert_eq!(
                messages.len(),
                2,
                "assistant message recorded for {action:?}"
            );
            drop(tx);
        }
    }

    #[tokio::test]
    async fn agent_turn_cancel_during_tool_round_is_cancelled_outcome() {
        // Drives the tool-round path of `await_unless_aborted`: the
        // stream completes synchronously, then `dispatch_tool_call`
        // parks on `GateTool`'s pending future. Sending Cancel after
        // the gate fires the started signal guarantees the cancel
        // races the tool future, not the prior stream phase.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![tool_use_turn("tool_1", "gate", r"{}")]);
        let started = Arc::new(Notify::new());
        let tools = ToolRegistry::new(vec![Box::new(GateTool {
            started: started.clone(),
        })]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("kick off")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(1);
        let prompt = empty_prompt();

        let (turn_result, ()) = tokio::join!(
            agent_turn(
                &client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut rx,
            ),
            async {
                started.notified().await;
                tx.send(UserAction::Cancel).await.unwrap();
            },
        );

        let abort = turn_result.expect_err("cancel must surface as Err(Cancelled)");
        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        // Tool-round cancel happens before the assistant tool_use and
        // tool_result messages are appended, so the conversation must
        // stay at the pre-turn tail. Iteration-atomic: the next turn
        // sees the same shape it would after a clean abort.
        assert_eq!(messages.len(), 1, "{messages:#?}");
        // The agent did emit ToolCallStart for the gated call (the
        // start event fires before `dispatch_tool_call` parks). The
        // matching End event must NOT fire because the tool's future
        // was dropped — assert on that to pin the cancel boundary.
        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallStart { id, .. } if id == "tool_1")),
            "ToolCallStart fired before cancel: {events:?}",
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCallEnd { id, .. } if id == "tool_1")),
            "ToolCallEnd must not fire after cancel: {events:?}",
        );
        drop(tx);
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
            &mut inert_user_rx(),
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
    async fn agent_turn_malformed_tool_input_short_circuits_to_parse_error_result() {
        // The model sometimes emits truncated / invalid JSON in
        // `input_json_delta`. The tool MUST NOT run with empty input —
        // instead the agent synthesizes an `is_error: true` tool result
        // that names the parse failure so the model can self-correct.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "echo", "{unclosed"),
            text_turn("recovered"),
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
            &mut inert_user_rx(),
        )
        .await
        .unwrap();

        // ToolUse round-trips with the empty-object fallback so the
        // assistant message stays well-formed.
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::ToolUse { id, input, .. }
                if id == "tool_1" && *input == json!({}),
        ));

        // Tool was NOT dispatched: result content is the synthetic
        // parse-error message, not EchoTool's `{}` echo.
        assert!(matches!(
            &messages[2].content[0],
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: true,
            } if tool_use_id == "tool_1"
                && content.contains("tool input JSON failed to parse")
                && content.contains("retry with a valid object"),
        ));

        // Sink saw a ToolCallEnd with is_error so the UI also reflects
        // the failure (instead of rendering an unparsed input label).
        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallEnd { id, is_error: true, content, .. }
                if id == "tool_1" && content.contains("tool input JSON failed to parse"),
        )));
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
            &mut inert_user_rx(),
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
            &mut inert_user_rx(),
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
            &mut inert_user_rx(),
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
            &mut inert_user_rx(),
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[1].content[0], ContentBlock::Text { text } if text == "hello"),);
    }

    // ── BlockAccumulator::into_content_block ──

    #[test]
    fn into_content_block_text_yields_text_block() {
        let (block, err) = BlockAccumulator::Text("hi".to_owned()).into_content_block();
        assert!(matches!(block, Some(ContentBlock::Text { text }) if text == "hi"));
        assert!(err.is_none());
    }

    #[test]
    fn into_content_block_tool_use_yields_tool_use_block() {
        let (block, err) = BlockAccumulator::ToolUse {
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
        assert!(err.is_none());
    }

    #[test]
    fn into_content_block_tool_use_malformed_json_surfaces_parse_error() {
        let (block, err) = BlockAccumulator::ToolUse {
            id: "tool_1".to_owned(),
            name: "bash".to_owned(),
            json_buf: "{unclosed".to_owned(),
        }
        .into_content_block();
        assert!(matches!(
            &block,
            Some(ContentBlock::ToolUse { id, input, .. })
                if id == "tool_1" && *input == json!({}),
        ));
        let (err_id, err_msg) = err.expect("parse error surfaced");
        assert_eq!(err_id, "tool_1");
        assert!(!err_msg.is_empty(), "non-empty serde_json error: {err_msg}");
    }

    #[test]
    fn into_content_block_server_tool_use_malformed_json_surfaces_parse_error() {
        let (block, err) = BlockAccumulator::ServerToolUse {
            id: "srv_1".to_owned(),
            name: "web_search".to_owned(),
            json_buf: "{unclosed".to_owned(),
        }
        .into_content_block();
        assert!(matches!(
            &block,
            Some(ContentBlock::ServerToolUse { id, input, .. })
                if id == "srv_1" && *input == json!({}),
        ));
        let (err_id, err_msg) = err.expect("parse error surfaced");
        assert_eq!(err_id, "srv_1");
        assert!(!err_msg.is_empty(), "non-empty serde_json error: {err_msg}");
    }

    #[test]
    fn into_content_block_server_tool_use_yields_server_tool_use_block() {
        let (block, err) = BlockAccumulator::ServerToolUse {
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
        assert!(err.is_none());
    }

    #[test]
    fn into_content_block_thinking_preserves_signature() {
        let (block, err) = BlockAccumulator::Thinking {
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
        assert!(err.is_none());
    }

    #[test]
    fn into_content_block_redacted_thinking_preserves_data() {
        let (block, err) = BlockAccumulator::RedactedThinking {
            data: "opaque-blob".to_owned(),
        }
        .into_content_block();
        assert!(
            matches!(block, Some(ContentBlock::RedactedThinking { data }) if data == "opaque-blob")
        );
        assert!(err.is_none());
    }

    #[test]
    fn into_content_block_skipped_yields_none() {
        let (block, err) = BlockAccumulator::Skipped.into_content_block();
        assert!(block.is_none());
        assert!(err.is_none());
    }

    // ── parse_tool_json ──

    #[test]
    fn parse_tool_json_valid_object() {
        let (value, err) = parse_tool_json(r#"{"command": "ls", "n": 3}"#);
        assert_eq!(value, json!({"command": "ls", "n": 3}));
        assert!(err.is_none());
    }

    #[test]
    fn parse_tool_json_malformed_produces_empty_object_and_error() {
        let (value, err) = parse_tool_json("{unclosed");
        assert_eq!(value, json!({}));
        let err = err.expect("parse error surfaced");
        assert!(!err.is_empty(), "non-empty serde_json error: {err}");
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
