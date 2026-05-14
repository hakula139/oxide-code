//! Agent turn loop. Streams the model response, dispatches tool calls, records to the session,
//! and stops on text-only response or the optional per-turn round cap.

pub(crate) mod compact_boundary;
pub(crate) mod compaction;
pub(crate) mod event;

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::agent::event::{AgentEvent, AgentSink, UserAction};
use crate::client::anthropic::Client;
use crate::client::anthropic::wire::{ContentBlockInfo, Delta, StreamEvent, Usage};
use crate::config::AutoCompactionConfig;
use crate::file_tracker::FileTracker;
use crate::message::{ContentBlock, Message, Role, strip_trailing_thinking};
use crate::prompt::PromptParts;
use crate::session::handle::{RecordOutcome, SessionHandle};
use crate::tool::{ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

const MAX_AUTO_COMPACT_FAILURES: u8 = 3;

// ── Turn Abort ──

/// Why a turn ended without a clean assistant reply. `Cancelled` and `Quit` are user-driven
/// and distinct so the outer driver can keep running on cancel but tear down on quit. `Failed`
/// wraps any other error so the caller can render it via `{e:#}`.
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    input: u32,
    cache_creation_input: u32,
    cache_read_input: u32,
    output: u32,
}

impl TokenUsage {
    #[cfg(test)]
    pub(crate) const fn new(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            input: input_tokens,
            cache_creation_input: 0,
            cache_read_input: 0,
            output: output_tokens,
        }
    }

    pub(crate) const fn context_tokens(self) -> u32 {
        self.input
            .saturating_add(self.cache_creation_input)
            .saturating_add(self.cache_read_input)
    }

    pub(crate) const fn total_tokens(self) -> u32 {
        self.context_tokens().saturating_add(self.output)
    }

    pub(crate) const fn input_tokens(self) -> u32 {
        self.input
    }

    pub(crate) const fn cache_creation_input_tokens(self) -> u32 {
        self.cache_creation_input
    }

    pub(crate) const fn cache_read_input_tokens(self) -> u32 {
        self.cache_read_input
    }

    pub(crate) const fn output_tokens(self) -> u32 {
        self.output
    }

    fn add(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            cache_creation_input: self
                .cache_creation_input
                .saturating_add(other.cache_creation_input),
            cache_read_input: self.cache_read_input.saturating_add(other.cache_read_input),
            output: self.output.saturating_add(other.output),
        }
    }

    fn observe(&mut self, usage: &Usage) {
        // Anthropic's wire usage carries fresh totals only on the events that own them:
        // `MessageStart` reports input + cache fields with `output_tokens = 0`, and `MessageDelta`
        // reports `output_tokens` with the input fields zeroed. Treat zero as "not reported here"
        // so successive observations layer correctly into one snapshot.
        if usage.input_tokens > 0 {
            self.input = usage.input_tokens;
        }
        if usage.cache_creation_input_tokens > 0 {
            self.cache_creation_input = usage.cache_creation_input_tokens;
        }
        if usage.cache_read_input_tokens > 0 {
            self.cache_read_input = usage.cache_read_input_tokens;
        }
        if usage.output_tokens > 0 {
            self.output = usage.output_tokens;
        }
    }
}

/// Per-turn usage report emitted at the end of [`agent_turn`]. The two fields carry different
/// temporal meanings and resist being collapsed into one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TurnReport {
    /// Latest single round's usage. Drives auto-compaction threshold checks, where the trigger
    /// depends on the most recent prompt size rather than the historical sum.
    pub(crate) usage: Option<TokenUsage>,
    /// Sum of every round's usage in this turn. Drives session cost accumulation, since each
    /// round was billed independently.
    pub(crate) billable_usage: Option<TokenUsage>,
}

/// Result of one [`agent_turn`] invocation. The report carries usage observed before any abort
/// so the outer loop can still bill rounds the API already charged for, even when the user
/// cancels mid-stream or a later round fails.
#[derive(Debug)]
pub(crate) struct TurnOutcome {
    pub(crate) report: TurnReport,
    pub(crate) result: AbortResult<()>,
}

impl TurnOutcome {
    fn completed(report: TurnReport) -> Self {
        Self {
            report,
            result: Ok(()),
        }
    }

    fn aborted(report: TurnReport, abort: TurnAbort) -> Self {
        Self {
            report,
            result: Err(abort),
        }
    }

    /// Test helper: returns the report on success or panics with the abort. Mirrors
    /// `Result::unwrap`.
    #[cfg(test)]
    pub(crate) fn unwrap(self) -> TurnReport {
        match self.result {
            Ok(()) => self.report,
            Err(abort) => panic!("turn aborted: {abort:?}"),
        }
    }

    /// Test helper: returns the report on success or panics with the abort and `msg`.
    #[cfg(test)]
    pub(crate) fn expect(self, msg: &str) -> TurnReport {
        match self.result {
            Ok(()) => self.report,
            Err(abort) => panic!("{msg}: {abort:?}"),
        }
    }

    /// Test helper: returns the abort on failure or panics. Mirrors `Result::expect_err`.
    #[cfg(test)]
    pub(crate) fn expect_err(self, msg: &str) -> TurnAbort {
        match self.result {
            Ok(()) => panic!("{msg}: turn unexpectedly completed"),
            Err(abort) => abort,
        }
    }
}

pub(crate) struct AutoCompact<'a> {
    pub(crate) config: AutoCompactionConfig,
    pub(crate) failures: &'a mut u8,
    pub(crate) file_tracker: &'a FileTracker,
}

/// Drives one user prompt to a final assistant text reply. The loop returns as soon as a round
/// produces no tool calls. Mid-turn `SubmitPrompt` actions queue and splice in as user messages
/// at round boundaries. Long-running awaits race `user_rx` so `Cancel` / `Quit` abort promptly
/// without leaving partial round writes. `max_tool_rounds = None` runs unbounded. `Some(n)`
/// bails after `n` rounds without a final response.
#[expect(
    clippy::too_many_arguments,
    reason = "agent_turn owns the full per-turn dependency set; bundling them obscures lifetimes"
)]
pub(crate) async fn agent_turn(
    client: &dyn AgentClient,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    prompt: &PromptParts,
    sink: &dyn AgentSink,
    session: &SessionHandle,
    user_rx: &mut mpsc::Receiver<UserAction>,
    max_tool_rounds: Option<u32>,
) -> TurnOutcome {
    let tool_defs = tools.definitions();
    let mut pending_prompts: Vec<String> = Vec::new();
    let mut latest_usage = None;
    let mut billable_usage = None;
    let report = |latest, billable| TurnReport {
        usage: latest,
        billable_usage: billable,
    };

    for _ in 0..max_tool_rounds.unwrap_or(u32::MAX) {
        strip_trailing_thinking(messages);
        let stream = await_unless_aborted(
            stream_response(client, messages, &tool_defs, prompt, sink),
            user_rx,
            &mut pending_prompts,
        )
        .await;
        let StreamOutcome {
            blocks,
            parse_errors,
            usage,
        } = match stream {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                return TurnOutcome::aborted(
                    report(latest_usage, billable_usage),
                    TurnAbort::Failed(error),
                );
            }
            Err(abort) => return TurnOutcome::aborted(report(latest_usage, billable_usage), abort),
        };
        if let Some(usage) = usage {
            latest_usage = Some(usage);
            billable_usage =
                Some(billable_usage.map_or(usage, |total: TokenUsage| total.add(usage)));
        }

        let tool_uses = collect_tool_uses(&blocks);
        let assistant_msg = Message {
            role: Role::Assistant,
            content: blocks,
        };

        if tool_uses.is_empty() {
            // Queued prompts drain on the TUI side at idle.
            record_message(session, assistant_msg.clone(), sink).await;
            messages.push(assistant_msg);
            return TurnOutcome::completed(report(latest_usage, billable_usage));
        }

        let round = run_tool_round(
            tools,
            tool_uses,
            &parse_errors,
            sink,
            user_rx,
            &mut pending_prompts,
        )
        .await;
        let (results, sidecars) = match round {
            Ok(pair) => pair,
            Err(abort) => return TurnOutcome::aborted(report(latest_usage, billable_usage), abort),
        };
        let tool_result_msg = Message {
            role: Role::User,
            content: results,
        };

        commit_round_writes(session, sink, &assistant_msg, &tool_result_msg, sidecars).await;
        messages.push(assistant_msg);
        messages.push(tool_result_msg);
        record_drained_prompts(pending_prompts.drain(..), messages, session, sink).await;
    }

    // `None` resolves to `u32::MAX`, so this branch is reachable only when the caller set
    // `Some(n)` and the model ran `n` rounds without a final reply.
    let cap = max_tool_rounds.unwrap_or(u32::MAX);
    TurnOutcome::aborted(
        report(latest_usage, billable_usage),
        TurnAbort::Failed(anyhow!(
            "agent stopped after {cap} tool rounds without a final response \
             — this is a safety cap against runaway loops. Ask again with a narrower request."
        )),
    )
}

/// Decides whether to compact and drives it when the threshold and breaker allow. Returns
/// `Ok(true)` when the transcript was replaced. Returns `Ok(false)` when skipped (breaker
/// tripped, no usage, below threshold) or when summarization / boundary-write failed.
#[expect(
    clippy::too_many_arguments,
    reason = "auto-compaction needs the same live turn state as manual compaction"
)]
pub(crate) async fn auto_compact_if_needed(
    client: &dyn AgentClient,
    session: &SessionHandle,
    messages: &mut Vec<Message>,
    sink: &dyn AgentSink,
    user_rx: &mut mpsc::Receiver<UserAction>,
    pending: &mut Vec<String>,
    auto: Option<&mut AutoCompact<'_>>,
    usage: Option<TokenUsage>,
) -> AbortResult<bool> {
    let Some(auto) = auto else {
        return Ok(false);
    };
    let Some(usage) = usage else {
        return Ok(false);
    };
    if *auto.failures >= MAX_AUTO_COMPACT_FAILURES
        || !auto.config.should_trigger(usage.total_tokens())
    {
        return Ok(false);
    }

    sink.emit(AgentEvent::AutoCompactionStarted, "auto-compaction-started");
    let summary = match await_unless_aborted(
        compaction::compact_session(client, messages, None),
        user_rx,
        pending,
    )
    .await?
    {
        Ok(summary) => summary,
        Err(e) => {
            warn!("auto-compaction failed: {e:#}");
            record_auto_compact_failure(auto, sink, Some(format!("Auto-compaction failed: {e:#}")));
            return Ok(false);
        }
    };
    let compacted = compact_boundary::replace_session_with_summary(
        session,
        auto.file_tracker,
        messages,
        sink,
        summary,
        None,
        true,
    )
    .await;
    if compacted {
        *auto.failures = 0;
    } else {
        record_auto_compact_failure(auto, sink, None);
    }
    Ok(compacted)
}

/// Bumps the failure counter. If the bump trips the breaker, emits a one-time disablement
/// notice. `detail` becomes a user-visible error event. Pass `None` when an earlier sink call
/// already surfaced the failure (persist-fail goes through `session_write_error`).
fn record_auto_compact_failure(
    auto: &mut AutoCompact<'_>,
    sink: &dyn AgentSink,
    detail: Option<String>,
) {
    *auto.failures += 1;
    if let Some(detail) = detail {
        sink.emit(AgentEvent::Error(detail), "auto-compaction-failed");
    }
    if *auto.failures == MAX_AUTO_COMPACT_FAILURES {
        sink.emit(
            AgentEvent::Error(format!(
                "Auto-compaction disabled for this session after {MAX_AUTO_COMPACT_FAILURES} failures."
            )),
            "auto-compaction-disabled",
        );
    }
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
        sink.emit(
            AgentEvent::ToolCallStart {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            "tool-call-start",
        );

        let output =
            dispatch_tool_call(tools, &name, input, parse_errors.get(&id), user_rx, pending)
                .await?;

        sink.emit(
            AgentEvent::ToolCallEnd {
                id: id.clone(),
                content: output.content.clone(),
                is_error: output.is_error,
                metadata: output.metadata.clone(),
            },
            "tool-call-end",
        );

        sidecars.push((id.clone(), output.metadata));
        results.push(ContentBlock::ToolResult {
            tool_use_id: id,
            content: output.content,
            is_error: output.is_error,
        });
    }
    Ok((results, sidecars))
}

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

pub(crate) async fn record_drained_prompts(
    texts: impl IntoIterator<Item = String>,
    messages: &mut Vec<Message>,
    session: &SessionHandle,
    sink: &dyn AgentSink,
) {
    for text in texts {
        let queued_msg = Message::user(text.clone());
        record_message(session, queued_msg.clone(), sink).await;
        messages.push(queued_msg);
        sink.emit(AgentEvent::PromptDrained(text), "prompt-drained");
    }
}

/// Races `fut` against user actions. Cancel / quit produce a `TurnAbort`. Submits buffer into
/// `pending`.
/// `fut` must be cancel-safe.
pub(crate) async fn await_unless_aborted<F, T>(
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
                // Unreachable under current wiring. Log so regressions surface.
                Some(
                    action @ (UserAction::ConfirmExit
                    | UserAction::Clear
                    | UserAction::Resume { .. }
                    | UserAction::Compact { .. }
                    | UserAction::Rename { .. }
                    | UserAction::SwapConfig { .. }
                    | UserAction::PreviewTheme { .. }
                    | UserAction::SwapTheme { .. }),
                ) => warn!("dropped mid-turn action: {action:?}"),
            },
            output = &mut fut => return Ok(output),
        }
    }
}

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
    /// Absorbs deltas. Produces no [`ContentBlock`].
    Skipped,
}

impl BlockAccumulator {
    /// Surfaces any tool-input parse error alongside the produced block.
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

/// Returns empty object + error string on failure.
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
    usage: Option<TokenUsage>,
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
    let mut usage = TokenUsage::default();
    let mut saw_usage = false;

    while let Some(event) = rx.recv().await {
        let event = event.context("stream error")?;

        match event {
            StreamEvent::MessageStart { message } => {
                if let Some(observed) = message.usage {
                    usage.observe(&observed);
                    saw_usage = true;
                }
            }
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
            StreamEvent::MessageDelta {
                usage: Some(observed),
                ..
            } => {
                usage.observe(&observed);
                saw_usage = true;
            }
            _ => {}
        }
    }

    let mut outcome = StreamOutcome {
        usage: saw_usage.then_some(usage),
        ..StreamOutcome::default()
    };
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::client::anthropic::testing::test_client;
    use crate::client::anthropic::wire::{
        ApiError, ContentBlockInfo, MessageDeltaBody, MessageResponse, StreamEvent, Usage,
    };
    use crate::config::{Auth, AutoCompactionConfig, Effort};
    use crate::file_tracker::FileTracker;
    use crate::message::Role;
    use crate::model::ResolvedModelId;
    use crate::session::handle::{self, SessionHandle};
    use crate::session::store::test_store;
    use crate::tool::{Tool, ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

    // ── TurnAbort ──

    #[test]
    fn turn_abort_display_alternate_propagates_anyhow_cause_chain() {
        // {abort:#} must delegate to the inner anyhow Error's alternate form so the full cause
        // chain reaches the user. Plain {abort} surfaces only the outermost context.
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

    // ── Fixtures ──

    /// In-process fake that scripts a [`StreamEvent`] sequence per turn.
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

    struct FailingCompactClient;

    impl AgentClient for FailingCompactClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
            Err(anyhow!("summarizer unavailable"))
        }
    }

    struct CountingFailingClient {
        calls: AtomicUsize,
    }

    impl CountingFailingClient {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl AgentClient for CountingFailingClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("summarizer unavailable"))
        }
    }

    struct DelayedSummaryClient {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl AgentClient for DelayedSummaryClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
            let (tx, rx) = mpsc::channel(8);
            let started = self.started.clone();
            let release = self.release.clone();
            tokio::spawn(async move {
                started.notify_one();
                release.notified().await;
                for event in text_turn("auto summary") {
                    tx.send(Ok(event)).await.expect("test receiver alive");
                }
            });
            Ok(rx)
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
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
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
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
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

    fn text_turn_with_usage(text: &str, input_tokens: u32, output_tokens: u32) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message: MessageResponse {
                    id: "msg_1".into(),
                    model: "claude-sonnet-4-6".into(),
                    usage: Some(Usage {
                        input_tokens,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
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
            StreamEvent::MessageDelta {
                delta: MessageDeltaBody {
                    stop_reason: Some("end_turn".into()),
                },
                usage: Some(Usage {
                    input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens,
                }),
            },
            StreamEvent::MessageStop,
        ]
    }

    fn text_turn_with_cache_usage(
        text: &str,
        input_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        output_tokens: u32,
    ) -> Vec<StreamEvent> {
        vec![
            StreamEvent::MessageStart {
                message: MessageResponse {
                    id: "msg_1".into(),
                    model: "claude-sonnet-4-6".into(),
                    usage: Some(Usage {
                        input_tokens,
                        cache_creation_input_tokens,
                        cache_read_input_tokens,
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
            StreamEvent::MessageDelta {
                delta: MessageDeltaBody {
                    stop_reason: Some("end_turn".into()),
                },
                usage: Some(Usage {
                    input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens,
                }),
            },
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

    fn tool_use_turn_with_usage(
        id: &str,
        name: &str,
        input_json: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Vec<StreamEvent> {
        let mut events = tool_use_turn(id, name, input_json);
        events.insert(
            0,
            StreamEvent::MessageStart {
                message: MessageResponse {
                    id: "msg_1".into(),
                    model: "claude-sonnet-4-6".into(),
                    usage: Some(Usage {
                        input_tokens,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        output_tokens,
                    }),
                },
            },
        );
        events
    }

    /// Echoes its input. Exercises the tool-dispatch path without subprocess machinery.
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

    /// Signals on entry then blocks forever, so cancel tests can synchronize on the agent being
    /// parked inside the tool future before sending the interrupt.
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

    /// Inert receiver whose `recv()` stays pending. The sender is leaked so call sites don't
    /// need to bind it for the test's lifetime.
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

    /// Handle whose actor channel is closed. Every write returns actor-gone.
    fn dead_test_session() -> SessionHandle {
        crate::session::handle::testing::dead("dead-test-session")
    }

    // ── TokenUsage ──

    #[test]
    fn token_usage_context_and_total_include_cache_tokens() {
        let usage = TokenUsage {
            input: 10,
            cache_creation_input: 20,
            cache_read_input: 30,
            output: 5,
        };

        assert_eq!(usage.context_tokens(), 60);
        assert_eq!(usage.total_tokens(), 65);
    }

    // ── auto_compact_if_needed ──

    #[tokio::test]
    async fn auto_compact_if_needed_skips_without_auto_state_usage_or_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(Vec::new());
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut pending = Vec::new();
        let mut failures = 0;

        let absent = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            None,
            Some(TokenUsage {
                input: 20,
                cache_creation_input: 0,
                cache_read_input: 0,
                output: 1,
            }),
        )
        .await
        .unwrap();
        assert!(!absent);

        let missing_usage = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            None,
        )
        .await
        .unwrap();
        assert!(!missing_usage);

        let below_threshold = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(100),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            Some(TokenUsage {
                input: 20,
                cache_creation_input: 0,
                cache_read_input: 0,
                output: 1,
            }),
        )
        .await
        .unwrap();
        assert!(!below_threshold);
        assert_eq!(messages.len(), 4);
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn auto_compact_if_needed_counts_summarizer_failure_without_replacing_messages() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut pending = Vec::new();
        let mut failures = 0;

        let compacted = auto_compact_if_needed(
            &FailingCompactClient,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            Some(TokenUsage {
                input: 20,
                cache_creation_input: 0,
                cache_read_input: 0,
                output: 1,
            }),
        )
        .await
        .unwrap();

        assert!(!compacted);
        assert_eq!(failures, 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hi"));
        assert!(
            sink.events().iter().any(|event| {
                matches!(event, AgentEvent::Error(message) if message.contains("Auto-compaction failed"))
            }),
            "summarizer failure must surface a user-visible AgentEvent::Error",
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_emits_disabled_notice_on_third_failure() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut pending = Vec::new();
        let mut failures = MAX_AUTO_COMPACT_FAILURES - 1;

        let compacted = auto_compact_if_needed(
            &FailingCompactClient,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            Some(TokenUsage {
                input: 20,
                cache_creation_input: 0,
                cache_read_input: 0,
                output: 1,
            }),
        )
        .await
        .unwrap();

        assert!(!compacted);
        assert_eq!(failures, MAX_AUTO_COMPACT_FAILURES);
        assert!(
            sink.events().iter().any(|event| {
                matches!(event, AgentEvent::Error(message) if message.contains("Auto-compaction disabled"))
            }),
            "the breaker-tripping failure must surface a one-time disablement notice",
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_counts_persist_failure_without_replacing_messages() {
        let session = dead_test_session();
        let client = FakeClient::new(vec![text_turn("auto summary")]);
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut pending = Vec::new();
        let mut failures = 0;

        let compacted = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            Some(TokenUsage {
                input: 20,
                cache_creation_input: 0,
                cache_read_input: 0,
                output: 1,
            }),
        )
        .await
        .unwrap();

        assert!(!compacted);
        assert_eq!(failures, 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hi"));
        assert!(
            sink.events()
                .iter()
                .any(|event| matches!(event, AgentEvent::Error(message) if message.contains("Session write failed")))
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_stops_after_failure_limit() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = CountingFailingClient::new();
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut pending = Vec::new();
        let mut failures = MAX_AUTO_COMPACT_FAILURES - 1;
        let usage = Some(TokenUsage {
            input: 50_000,
            cache_creation_input: 0,
            cache_read_input: 0,
            output: 1,
        });

        let first = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(50_000),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            usage,
        )
        .await
        .unwrap();
        let second = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut inert_user_rx(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(50_000),
                },
                failures: &mut failures,
                file_tracker: &tracker,
            }),
            usage,
        )
        .await
        .unwrap();

        assert!(!first);
        assert!(!second);
        assert_eq!(failures, MAX_AUTO_COMPACT_FAILURES);
        assert_eq!(client.calls(), 1);
    }

    #[tokio::test]
    async fn auto_compact_if_needed_queues_submit_while_summarizing() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let client = DelayedSummaryClient {
            started: started.clone(),
            release: release.clone(),
        };
        let sink = CapturingSink::new();
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut pending = Vec::new();
        let mut failures = 0;
        let (tx, mut rx) = mpsc::channel::<UserAction>(1);
        let mut auto = AutoCompact {
            config: AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(10),
            },
            failures: &mut failures,
            file_tracker: &tracker,
        };

        let compact = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &sink,
            &mut rx,
            &mut pending,
            Some(&mut auto),
            Some(TokenUsage {
                input: 20,
                cache_creation_input: 0,
                cache_read_input: 0,
                output: 1,
            }),
        );
        let queue_prompt = async {
            started.notified().await;
            tx.send(UserAction::SubmitPrompt("queued after summary".into()))
                .await
                .unwrap();
            tokio::task::yield_now().await;
            release.notify_one();
        };
        let (compacted, ()) = tokio::join!(compact, queue_prompt);
        let compacted = compacted.unwrap();

        assert!(compacted);
        assert_eq!(pending, vec!["queued after summary"]);
        assert_eq!(*auto.failures, 0);
        assert_eq!(
            sink.events()
                .iter()
                .filter(|event| matches!(event, AgentEvent::PromptDrained(_)))
                .count(),
            0
        );
        assert!(sink.events().iter().any(|event| matches!(
            event,
            AgentEvent::SessionCompacted {
                automatic: true,
                ..
            }
        )));
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("auto summary"))
        );
    }

    // ── agent_turn ──

    #[tokio::test]
    async fn agent_turn_dead_session_surfaces_write_failure_on_first_call() {
        // Write errors must not abort the turn. One Error event surfaces and the turn returns Ok.
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
            None,
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
        // First 2 cmds (assistant + tool_result messages) ack healthily. The 3rd (metadata batch)
        // is dropped without ack so the batch's failure handler fires the Error event.
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
            None,
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
    async fn agent_turn_text_only_response_records_and_completes() {
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
            None,
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
            None,
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
    async fn agent_turn_reports_latest_stream_usage() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn_with_usage("Hello!", 100, 7)]);
        let tools = ToolRegistry::new(Vec::new());
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        let report = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_user_rx(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.usage.map(TokenUsage::total_tokens), Some(107));
        assert_eq!(
            report.billable_usage.map(TokenUsage::total_tokens),
            Some(107)
        );
    }

    #[tokio::test]
    async fn agent_turn_reports_cache_usage_for_context_and_cost() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn_with_cache_usage("Hello!", 10, 20, 30, 5)]);
        let tools = ToolRegistry::new(Vec::new());
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        let report = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_user_rx(),
            None,
        )
        .await
        .unwrap();

        let usage = report.usage.expect("stream reports usage");
        assert_eq!(usage.input_tokens(), 10);
        assert_eq!(usage.cache_creation_input_tokens(), 20);
        assert_eq!(usage.cache_read_input_tokens(), 30);
        assert_eq!(usage.output_tokens(), 5);
        assert_eq!(usage.context_tokens(), 60);
        assert_eq!(usage.total_tokens(), 65);
        assert_eq!(report.billable_usage, Some(usage));
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
            None,
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
    async fn agent_turn_does_not_auto_compact_between_tool_result_and_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn_with_usage("tool_1", "echo", r#"{"v":1}"#, 9, 2),
            text_turn_with_usage("Done", 1, 2),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![
            Message::user("run echo"),
            Message::assistant("earlier"),
            Message::user("continue"),
        ];

        let report = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_user_rx(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(report.usage.map(TokenUsage::total_tokens), Some(3));
        assert_eq!(
            report.billable_usage.map(TokenUsage::total_tokens),
            Some(14)
        );
        assert_eq!(
            sink.events()
                .iter()
                .filter(|event| matches!(event, AgentEvent::SessionCompacted { .. }))
                .count(),
            0
        );
        assert!(matches!(&messages[5].content[0], ContentBlock::Text { text } if text == "Done"));
    }

    #[tokio::test]
    async fn agent_turn_drains_mid_round_submit_into_messages_at_round_boundary() {
        // Pre-loaded SubmitPrompt is consumed during the round. At the boundary the agent splices
        // the queued text as a trailing user message and emits PromptDrained.
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
        // Hold the sender so `recv()` stays pending after the queued submit.
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
            None,
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
        // Two SubmitPrompts during one await must land as separate User messages in dispatch order
        // with one PromptDrained event each.
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
            None,
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
        // Biased select picks the queued Cancel before the stream future, so the session stays at
        // its pre-turn tail and the abort is Cancelled.
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
            None,
        )
        .await
        .expect_err("cancel must surface as Err(Cancelled)");

        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        assert_eq!(messages.len(), 1, "no assistant message recorded");
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_quit_during_stream_is_quit_abort() {
        // Quit is the teardown signal. It must stay distinct from Cancel for the outer driver.
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
            None,
        )
        .await
        .expect_err("quit must surface as Err(Quit)");

        assert!(matches!(abort, TurnAbort::Quit), "got {abort:?}");
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_sender_drop_during_turn_collapses_to_quit_abort() {
        // Closed channel resolves `recv()` to None. Treated as Quit so the outer loop exits.
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
            None,
        )
        .await
        .expect_err("dead channel must surface as Err(Quit)");

        assert!(matches!(abort, TurnAbort::Quit), "got {abort:?}");
    }

    #[tokio::test]
    async fn agent_turn_absorbs_stragglers_without_killing_turn() {
        // Every catch-all variant in `await_unless_aborted` must let the turn complete. Returning
        // Cancelled for any of these would kill the turn from a buffered no-op.
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

            let outcome = agent_turn(
                &client,
                &tools,
                &mut messages,
                &empty_prompt(),
                &sink,
                &session,
                &mut rx,
                None,
            )
            .await;
            assert!(
                outcome.result.is_ok(),
                "turn must complete despite {action:?}"
            );

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
        // Synchronizes on `GateTool`'s started signal so Cancel races the tool future, not the
        // prior stream phase.
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
                None,
            ),
            async {
                started.notified().await;
                tx.send(UserAction::Cancel).await.unwrap();
            },
        );

        let abort = turn_result.expect_err("cancel must surface as Err(Cancelled)");
        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        // Cancel here happens before the round's messages are appended. Iteration-atomic so the
        // next turn sees the pre-turn tail.
        assert_eq!(messages.len(), 1, "{messages:#?}");
        // ToolCallStart fires before `dispatch_tool_call` parks. The matching End must not fire
        // because the tool future was dropped.
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
    async fn agent_turn_cancel_during_tool_round_preserves_completed_round_usage() {
        // Round 1's stream observes usage before the tool gates. Cancel arrives during the tool
        // execution, so the abort must still carry billable_usage from the completed round.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![tool_use_turn_with_usage(
            "tool_1", "gate", r"{}", 7, 3,
        )]);
        let started = Arc::new(Notify::new());
        let tools = ToolRegistry::new(vec![Box::new(GateTool {
            started: started.clone(),
        })]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("kick off")];
        let (tx, mut rx) = mpsc::channel::<UserAction>(1);
        let prompt = empty_prompt();

        let (outcome, ()) = tokio::join!(
            agent_turn(
                &client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut rx,
                None,
            ),
            async {
                started.notified().await;
                tx.send(UserAction::Cancel).await.unwrap();
            },
        );

        assert!(
            matches!(outcome.result, Err(TurnAbort::Cancelled)),
            "got {:?}",
            outcome.result,
        );
        assert_eq!(
            outcome.report.billable_usage.map(TokenUsage::total_tokens),
            Some(10),
            "round 1 usage must reach the caller despite the abort: {:?}",
            outcome.report,
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
            None,
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
        // Truncated `input_json_delta` must not run the tool with empty input. The agent
        // synthesizes an `is_error: true` result naming the parse failure for self-correction.
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
            None,
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

        // Tool not dispatched: result is the synthetic parse-error, not EchoTool's `{}` echo.
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

        // Sink also reflects the failure via ToolCallEnd { is_error: true }.
        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallEnd { id, is_error: true, content, .. }
                if id == "tool_1" && content.contains("tool input JSON failed to parse"),
        )));
    }

    #[tokio::test]
    async fn agent_turn_with_some_cap_bails_with_safety_cap_message() {
        const CAP: u32 = 3;
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let turns: Vec<Vec<StreamEvent>> = (0..CAP)
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
            Some(CAP),
        )
        .await
        .expect_err("cap must trip");
        let msg = format!("{err:#}");
        assert!(msg.contains(&CAP.to_string()), "cap in error: {msg}");
        assert!(msg.contains("safety cap"), "explains intent: {msg}");
    }

    #[tokio::test]
    async fn agent_turn_with_none_cap_runs_unbounded_until_text_only_reply() {
        // The cap is None, so the loop must not bail on its own; it terminates only when the
        // model produces a text-only round. Pin that an arbitrary multi-round chain still
        // completes without tripping the safety cap.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let mut turns: Vec<Vec<StreamEvent>> = (0..50)
            .map(|i| tool_use_turn(&format!("tool_{i}"), "echo", r"{}"))
            .collect();
        turns.push(text_turn("done"));
        let client = FakeClient::new(turns);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("loop a while then stop")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_user_rx(),
            None,
        )
        .await
        .expect("unbounded loop must reach the text-only round");
        let last = messages.last().expect("assistant reply recorded");
        assert!(matches!(last.role, Role::Assistant), "{last:?}");
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
            None,
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
        // The API rejects a trailing thinking block on follow-up rounds.
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
            None,
        )
        .await
        .unwrap();

        let stripped = &messages[1];
        assert_eq!(stripped.content.len(), 1);
        assert!(matches!(&stripped.content[0], ContentBlock::Text { .. }));
    }

    /// Covers the real `<Client as AgentClient>` path. `FakeClient` tests above stub the trait.
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
            None,
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
