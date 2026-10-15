//! Agent turn loop. Streams the model response, dispatches tool calls, records to the session,
//! and stops on text-only response or the optional per-turn round cap.

pub(crate) mod compact_boundary;
pub(crate) mod compaction;

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::client::anthropic::Client;
use crate::client::anthropic::wire::{
    ContentBlockInfo, Delta, StopDetails, StopReason, StreamEvent, Usage,
};
use crate::config::AutoCompactionConfig;
use crate::file_tracker::{self, FileTracker};
use crate::message::{ContentBlock, Message, Role, progress_note};
use crate::prompt::PromptParts;
use crate::protocol::{Decision, Event, EventSink, Op, ToolCall, ToolResult};
use crate::session::handle::{RecordOutcome, SessionHandle};
use crate::tool::{ToolDefinition, ToolMetadata, ToolOutput, ToolRegistry};

const MAX_AUTO_COMPACT_FAILURES: u8 = 3;

// ── Turn Abort ──

/// Why a turn ended without a clean assistant reply. `Cancelled` and `Shutdown` are user-driven
/// and distinct, so the engine keeps serving after a cancel but stops on shutdown. `Failed` wraps
/// any other error so the caller can render it via `{e:#}`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TurnAbort {
    #[error("turn cancelled")]
    Cancelled,
    #[error("engine shut down")]
    Shutdown,
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

    /// Streams a request no later request extends, so caching its transcript tail would only add
    /// a write premium. Defaults to [`Self::stream_message`] for clients that do no caching.
    fn stream_one_shot(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        self.stream_message(messages, system_sections, user_context, tools)
    }
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

    fn stream_one_shot(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        Client::stream_one_shot(self, messages, system_sections, user_context, tools)
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

/// Usage the API reported for one turn's requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TurnReport {
    /// Latest single round's usage, used by the auto-compaction threshold check, which compares
    /// against the most recent prompt size rather than the historical sum.
    pub(crate) usage: Option<TokenUsage>,
    /// Every request's usage in order, including one that failed or was cancelled partway. Some
    /// models pick their price tier from a single request's prompt size, so cost is per entry.
    pub(crate) billable_rounds: Vec<TokenUsage>,
}

impl TurnReport {
    fn record(&mut self, round: Option<TokenUsage>) {
        let Some(round) = round else { return };
        self.usage = Some(round);
        self.billable_rounds.push(round);
    }
}

/// Result of one [`agent_turn`] invocation. The report counts every request whose stream
/// reported usage, including one the user cancelled or that failed partway.
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

    /// Mirrors `Result::unwrap`: returns the report on success or panics with the abort.
    #[cfg(test)]
    pub(crate) fn unwrap(self) -> TurnReport {
        match self.result {
            Ok(()) => self.report,
            Err(abort) => panic!("turn aborted: {abort:?}"),
        }
    }

    /// Mirrors `Result::expect`: returns the report on success or panics with the abort and `msg`.
    #[cfg(test)]
    pub(crate) fn expect(self, msg: &str) -> TurnReport {
        match self.result {
            Ok(()) => self.report,
            Err(abort) => panic!("{msg}: {abort:?}"),
        }
    }

    /// Mirrors `Result::expect_err`: returns the abort on failure or panics.
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
    /// The conversation's request prefix, which the summarizer reuses to read its prompt cache.
    pub(crate) prompt: &'a PromptParts,
    pub(crate) tools: &'a [ToolDefinition],
    /// Each summary request's usage, recorded even when the summary fails or the user cancels it.
    pub(crate) spent: &'a mut Vec<TokenUsage>,
}

/// Inputs the permission gate needs at each tool call: the resolved policy, the working directory
/// to resolve path targets against, and whether the session can prompt a human. In a non-interactive
/// session an `ask` resolves to deny, since no modal can surface.
pub(crate) struct GateContext<'a> {
    pub(crate) policy: &'a crate::permission::Policy,
    pub(crate) cwd: &'a std::path::Path,
    pub(crate) interactive: bool,
}

/// Drives one user prompt to a final assistant text reply. The loop returns as soon as a round
/// produces no tool calls, and a `pause_turn` stop re-requests without a new user message.
/// [`Op::Queue`]s splice in as user messages at round boundaries. Long-running awaits race `ops`
/// so `Interrupt` / `Shutdown` abort promptly. An abort mid-stream commits nothing, while an abort
/// mid-round commits the round with the unfinished calls marked interrupted.
/// `max_tool_rounds = None` runs unbounded, and `Some(n)` bails after `n` rounds without a reply.
#[expect(
    clippy::too_many_arguments,
    reason = "agent_turn owns the full per-turn dependency set; bundling them obscures lifetimes"
)]
pub(crate) async fn agent_turn(
    client: &dyn AgentClient,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    prompt: &PromptParts,
    sink: &dyn EventSink,
    session: &SessionHandle,
    ops: &mut mpsc::Receiver<Op>,
    max_tool_rounds: Option<u32>,
    gate: &GateContext<'_>,
) -> TurnOutcome {
    let tool_defs = tools.definitions();
    let mut pending_prompts: Vec<String> = Vec::new();
    let mut report = TurnReport::default();

    for _ in 0..max_tool_rounds.unwrap_or(u32::MAX) {
        let mut round_usage = None;
        let stream = await_unless_aborted(
            stream_response(client, messages, &tool_defs, prompt, sink, &mut round_usage),
            ops,
            &mut pending_prompts,
        )
        .await;
        report.record(round_usage);
        let StreamOutcome {
            blocks,
            parse_errors,
            stop_reason,
            stop_details,
        } = match stream {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return TurnOutcome::aborted(report, TurnAbort::Failed(error)),
            Err(abort) => return TurnOutcome::aborted(report, abort),
        };

        let blocks = match settle_stop_reason(
            stop_reason,
            stop_details.as_ref(),
            blocks,
            session,
            sink,
            messages,
        )
        .await
        {
            StopAction::Proceed(blocks) => blocks,
            StopAction::Resume => continue,
            StopAction::Finish(result) => return TurnOutcome { report, result },
        };

        let tool_uses = collect_tool_uses(&blocks);
        if tool_uses.is_empty() {
            // Queued prompts drain on the TUI side at idle.
            commit_assistant_message(session, sink, messages, blocks).await;
            notify_if_interrupted(messages, sink);
            return TurnOutcome::completed(report);
        }
        let assistant_msg = Message {
            role: Role::Assistant,
            content: blocks,
        };

        let round = run_tool_round(
            tools,
            tool_uses,
            &parse_errors,
            sink,
            ops,
            &mut pending_prompts,
            gate,
        )
        .await;
        // Calls that finished before an abort already took effect, so the model must see them.
        commit_round(
            session,
            sink,
            messages,
            assistant_msg,
            round.results,
            round.sidecars,
        )
        .await;
        if let Some(abort) = round.abort {
            return TurnOutcome::aborted(report, abort);
        }
        record_drained_prompts(pending_prompts.drain(..), messages, session, sink).await;
    }

    // `None` resolves to `u32::MAX`, so this branch is reachable only when the caller set
    // `Some(n)` and the model ran `n` rounds without a final reply.
    let cap = max_tool_rounds.unwrap_or(u32::MAX);
    TurnOutcome::aborted(
        report,
        TurnAbort::Failed(anyhow!(
            "agent stopped after {cap} tool rounds without a final response. This safety cap \
             guards against runaway loops, so ask again with a narrower request."
        )),
    )
}

/// How the round loop continues after [`settle_stop_reason`].
enum StopAction {
    /// The stop needs no special handling. Dispatch any tool calls in these blocks.
    Proceed(Vec<ContentBlock>),
    /// `pause_turn`: the reply is committed and the next request resumes it.
    Resume,
    /// The turn ends here.
    Finish(AbortResult<()>),
}

/// Handles the stop reasons that end or pause a round before any tool runs.
async fn settle_stop_reason(
    stop_reason: Option<StopReason>,
    stop_details: Option<&StopDetails>,
    blocks: Vec<ContentBlock>,
    session: &SessionHandle,
    sink: &dyn EventSink,
    messages: &mut Vec<Message>,
) -> StopAction {
    match stop_reason {
        // The API asks clients to drop refused output without running it.
        Some(StopReason::Refusal) => {
            sink.emit(Event::Error(refusal_message(stop_details)), "refusal");
            StopAction::Finish(Ok(()))
        }
        // Running a cut-off call, or asking the model to retry it, would repeat the same
        // oversized output, so the round stops and keeps only the finished prose.
        Some(reason @ (StopReason::MaxTokens | StopReason::ModelContextWindowExceeded)) => {
            let error = truncation_error(reason, &blocks);
            let prose = blocks
                .into_iter()
                .filter(|b| !matches!(b, ContentBlock::ToolUse { .. }))
                .collect();
            commit_assistant_message(session, sink, messages, prose).await;
            StopAction::Finish(Err(TurnAbort::Failed(error)))
        }
        Some(StopReason::PauseTurn) => {
            commit_assistant_message(session, sink, messages, blocks).await;
            StopAction::Resume
        }
        _ => StopAction::Proceed(blocks),
    }
}

/// Records and appends a final assistant reply. An empty reply is skipped because the API rejects
/// an assistant message with no content on every later request.
async fn commit_assistant_message(
    session: &SessionHandle,
    sink: &dyn EventSink,
    messages: &mut Vec<Message>,
    content: Vec<ContentBlock>,
) {
    if content.is_empty() {
        return;
    }
    let message = Message {
        role: Role::Assistant,
        content,
    };
    record_message(session, message.clone(), sink).await;
    messages.push(message);
}

/// User-facing notice for a `refusal` stop, naming the policy category and explanation when the
/// API supplied them.
fn refusal_message(details: Option<&StopDetails>) -> String {
    let category = details
        .and_then(|d| d.category.as_deref())
        .map(|c| format!(" (category: {c})"))
        .unwrap_or_default();
    let explanation = details.and_then(|d| d.explanation.as_deref()).map_or_else(
        || ".".to_owned(),
        |e| format!(": {}.", e.trim().trim_end_matches('.')),
    );
    format!(
        "The model declined this request{category}{explanation} Rephrase it, or run /clear to \
         start over."
    )
}

/// Error for a reply cut off by the output cap or the context window, naming the tool call it cut.
fn truncation_error(reason: StopReason, blocks: &[ContentBlock]) -> anyhow::Error {
    let (cause, remedy) = if reason == StopReason::ModelContextWindowExceeded {
        (
            "The conversation filled the model's context window",
            "Run /compact, then continue.",
        )
    } else {
        (
            "The response hit the max_tokens output limit",
            "Ask for smaller steps, or raise `max_tokens`.",
        )
    };
    let effect = match blocks.last() {
        Some(ContentBlock::ToolUse { name, .. }) => {
            format!("while writing the `{name}` call, so no tools ran")
        }
        _ if blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. })) =>
        {
            "before it finished, so no tools ran".to_owned()
        }
        _ => "before it finished".to_owned(),
    };
    anyhow!("{cause} {effect}. {remedy}")
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
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
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

    sink.emit(Event::AutoCompactionStarted, "auto-compaction-started");
    let summary = match await_unless_aborted(
        compaction::compact_session(client, auto.prompt, auto.tools, messages, None, auto.spent),
        ops,
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
    sink: &dyn EventSink,
    detail: Option<String>,
) {
    *auto.failures += 1;
    if let Some(detail) = detail {
        sink.emit(Event::Error(detail), "auto-compaction-failed");
    }
    if *auto.failures == MAX_AUTO_COMPACT_FAILURES {
        sink.emit(
            Event::Error(format!(
                "Auto-compaction disabled for this session after {MAX_AUTO_COMPACT_FAILURES} failures."
            )),
            "auto-compaction-disabled",
        );
    }
}

/// Tells the user to send a message when `messages` ends on a response that stopped early, which
/// only a new prompt resumes.
pub(crate) fn notify_if_interrupted(messages: &[Message], sink: &dyn EventSink) {
    if let Some(last) = messages.last()
        && last.role == Role::Assistant
        && ends_on_interrupted_work(&last.content)
    {
        sink.emit(
            Event::Error(
                "The response was interrupted before it finished. Send a message to continue."
                    .to_owned(),
            ),
            "interrupted-work",
        );
    }
}

/// Text of the progress block that stands in for unfinished work when a response stops early.
pub(crate) const INTERRUPTED_WORK_SENTINEL: &str =
    "This part of the response was interrupted before it finished.";

fn ends_on_interrupted_work(blocks: &[ContentBlock]) -> bool {
    matches!(
        blocks.last(),
        Some(ContentBlock::Thinking { thinking, .. }) if thinking == INTERRUPTED_WORK_SENTINEL
    )
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

/// One tool round's results, with a result for every call even when a user abort stopped dispatch.
struct ToolRound {
    results: Vec<ContentBlock>,
    sidecars: Vec<(String, ToolMetadata)>,
    abort: Option<TurnAbort>,
}

impl ToolRound {
    fn push(&mut self, id: String, output: ToolOutput) {
        self.sidecars.push((id.clone(), output.metadata));
        self.results.push(ContentBlock::ToolResult {
            tool_use_id: id,
            content: output.content,
            is_error: output.is_error,
        });
    }
}

async fn run_tool_round(
    tools: &ToolRegistry,
    tool_uses: Vec<(String, String, serde_json::Value)>,
    parse_errors: &HashMap<String, String>,
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
    gate: &GateContext<'_>,
) -> ToolRound {
    let mut round = ToolRound {
        results: Vec::with_capacity(tool_uses.len()),
        sidecars: Vec::with_capacity(tool_uses.len()),
        abort: None,
    };
    let mut calls = tool_uses.into_iter();
    for (id, name, input) in calls.by_ref() {
        sink.emit(
            Event::ToolCallStart(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                icon: tools.icon(&name).to_owned(),
                label: tools.label(&name, &input),
            }),
            "tool-call-start",
        );

        let (output, abort) = dispatch_tool_call(
            tools,
            &id,
            &name,
            input.clone(),
            parse_errors.get(&id),
            sink,
            ops,
            pending,
            gate,
        )
        .await;
        sink.emit(
            Event::ToolCallEnd(ToolResult {
                id: id.clone(),
                content: output.content.clone(),
                is_error: output.is_error,
                title: output.metadata.title.clone(),
                view: tools.result_view(
                    &name,
                    &input,
                    &output.content,
                    &output.metadata,
                    output.is_error,
                ),
            }),
            "tool-call-end",
        );
        round.push(id, output);
        if abort.is_some() {
            round.abort = abort;
            break;
        }
    }
    for (id, ..) in calls {
        round.push(id, ToolOutput::error(NOT_RUN));
    }
    round
}

/// Result of a call the user interrupted before its tool started.
const NOT_RUN: &str = "Not run, because the user interrupted the turn.";

/// Persists one tool round, then appends it to the live transcript.
async fn commit_round(
    session: &SessionHandle,
    sink: &dyn EventSink,
    messages: &mut Vec<Message>,
    assistant_msg: Message,
    results: Vec<ContentBlock>,
    sidecars: Vec<(String, ToolMetadata)>,
) {
    let tool_result_msg = Message {
        role: Role::User,
        content: results,
    };
    let assistant_fut = session.record_message(assistant_msg.clone());
    let tool_result_fut = session.record_message(tool_result_msg.clone());
    let metadata_fut = session.record_tool_metadata_batch(sidecars);
    let (assistant_outcome, tool_result_outcome, metadata_outcome) =
        tokio::join!(assistant_fut, tool_result_fut, metadata_fut);
    sink.session_write_error(assistant_outcome.failure.as_deref());
    sink.session_write_error(tool_result_outcome.failure.as_deref());
    sink.session_write_error(metadata_outcome.failure.as_deref());
    messages.push(assistant_msg);
    messages.push(tool_result_msg);
}

/// Runs one call through the permission gate and its tool. A user abort still yields a result for
/// the call, which says whether the tool had started.
#[expect(
    clippy::too_many_arguments,
    reason = "dispatch threads the per-call gate inputs (id, sink, gate) alongside the run state"
)]
async fn dispatch_tool_call(
    tools: &ToolRegistry,
    id: &str,
    name: &str,
    mut input: serde_json::Value,
    parse_error: Option<&String>,
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
    gate: &GateContext<'_>,
) -> (ToolOutput, Option<TurnAbort>) {
    if let Some(err) = parse_error {
        return (
            ToolOutput::error(format!(
                "Tool input JSON failed to parse: {err}. Retry with a valid JSON object."
            )),
            None,
        );
    }

    match check_permission(tools, id, name, &mut input, sink, ops, pending, gate).await {
        Ok(None) => {}
        Ok(Some(denial)) => return (denial, None),
        Err(abort) => return (ToolOutput::error(NOT_RUN), Some(abort)),
    }

    match await_unless_aborted(
        file_tracker::attribute_to(id, tools.run(name, input)),
        ops,
        pending,
    )
    .await
    {
        Ok(output) => (output, None),
        Err(abort) => {
            let output = ToolOutput::error(
                "The user interrupted this call before it finished, so it may have run partially.",
            );
            (output, Some(abort))
        }
    }
}

/// Runs the permission gate for one call, first pinning a path tool's `input` to the path the gate
/// resolves so the preview and the run act on what was checked. Returns `Ok(None)` to proceed,
/// `Ok(Some(output))` with a synthetic denial, or an abort on an interrupt or shutdown during an
/// outstanding approval. An `ask` emits [`Event::ApprovalRequested`] and blocks on the matching
/// [`Op::Resolve`], but denies at once when the session is non-interactive or no modal can receive
/// the request.
#[expect(
    clippy::too_many_arguments,
    reason = "the gate threads the same per-call inputs dispatch already carries"
)]
async fn check_permission(
    tools: &ToolRegistry,
    id: &str,
    name: &str,
    input: &mut serde_json::Value,
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
    gate: &GateContext<'_>,
) -> AbortResult<Option<ToolOutput>> {
    let Some(tool) = tools.get(name) else {
        // Unknown tool: let `tools.run` produce its own "unknown tool" error rather than denying.
        return Ok(None);
    };

    let gate_target = tool.gate_target(input, gate.cwd);
    tool.pin_target(input, &gate_target);
    let decision = gate
        .policy
        .decide(tool.permission_class(), &gate_target.as_target());

    match decision {
        crate::permission::Decision::Allow => Ok(None),
        crate::permission::Decision::Deny => Ok(Some(denied_output(name))),
        crate::permission::Decision::Ask if !gate.interactive => Ok(Some(denied_output(name))),
        crate::permission::Decision::Ask => {
            // The approval request is a control-plane event: if it can't be delivered, no modal will
            // exist to answer it and `await_approval` would block forever. Fail closed on a send
            // error rather than stranding the turn.
            if sink
                .send(Event::ApprovalRequested {
                    id: id.to_owned(),
                    preview: tool.approval_preview(input),
                })
                .is_err()
            {
                return Ok(Some(denied_output(name)));
            }
            match await_approval(id, ops, pending).await? {
                Decision::Approve => Ok(None),
                Decision::Deny => Ok(Some(denied_output(name))),
            }
        }
    }
}

/// The synthetic tool result returned for a denied call. Marked `is_error` so the model treats it
/// as a failed call and can try another approach.
fn denied_output(name: &str) -> ToolOutput {
    ToolOutput::error(format!(
        "The {name} call was denied by the permission policy. Do not retry it. \
         Choose a different approach or ask the user to adjust permissions."
    ))
}

/// Blocks on the `ops` channel for the [`Op::Resolve`] matching `id`, the same channel the turn
/// loop races elsewhere so no second channel is introduced. An interrupt or shutdown aborts the
/// turn, an [`Op::Queue`] buffers into `pending`, and a decision for a different id is ignored (a
/// stale reply for an already-resolved call). Cancel-safe by drop: it holds no state.
async fn await_approval(
    id: &str,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
) -> AbortResult<Decision> {
    loop {
        match ops.recv().await {
            Some(Op::Interrupt) => return Err(TurnAbort::Cancelled),
            Some(Op::Shutdown) | None => return Err(TurnAbort::Shutdown),
            Some(Op::Queue(text)) => pending.push(text),
            Some(Op::Resolve {
                id: target,
                decision,
            }) if target == id => {
                return Ok(decision);
            }
            // A decision for a different id is a stale reply, and any other op is unreachable
            // while an approval is outstanding. Log and keep waiting so the gate can't be skipped.
            Some(other) => warn!("ignored op while awaiting approval {id}: {other:?}"),
        }
    }
}

pub(crate) async fn record_drained_prompts(
    texts: impl IntoIterator<Item = String>,
    messages: &mut Vec<Message>,
    session: &SessionHandle,
    sink: &dyn EventSink,
) {
    for text in texts {
        let queued_msg = Message::user(text.clone());
        record_message(session, queued_msg.clone(), sink).await;
        messages.push(queued_msg);
        sink.emit(Event::PromptDrained(text), "prompt-drained");
    }
}

/// Races `fut` against incoming ops. An interrupt or shutdown produces a `TurnAbort`, and a queued
/// prompt buffers into `pending`. `fut` must be cancel-safe.
pub(crate) async fn await_unless_aborted<F, T>(
    fut: F,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
) -> AbortResult<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        tokio::select! {
            // Biased: ops observed before same-poll-ready futures.
            biased;
            op = ops.recv() => match op {
                Some(Op::Interrupt) => return Err(TurnAbort::Cancelled),
                Some(Op::Shutdown) | None => return Err(TurnAbort::Shutdown),
                Some(Op::Queue(text)) => pending.push(text),
                // Unreachable under current wiring. Log so regressions surface. A `Resolve`
                // here is a stale reply for an already-resolved call.
                Some(
                    op @ (Op::Prompt(_)
                    | Op::Clear
                    | Op::Archive
                    | Op::Resume { .. }
                    | Op::Compact { .. }
                    | Op::Rename { .. }
                    | Op::SetModel { .. }
                    | Op::Resolve { .. }),
                ) => warn!("dropped mid-turn op: {op:?}"),
            },
            output = &mut fut => return Ok(output),
        }
    }
}

async fn record_message(session: &SessionHandle, msg: Message, sink: &dyn EventSink) {
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

    /// The block's thinking text in the form [`progress_note`] reads.
    fn thinking_text(&self) -> Option<&str> {
        match self {
            Self::Thinking { thinking, .. } => Some(thinking),
            Self::RedactedThinking { .. } => Some(""),
            _ => None,
        }
    }
}

/// Returns empty object + error string on failure. An empty buffer is a call that streamed no
/// input, which reads as `{}`.
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

#[derive(Debug, Default)]
struct StreamOutcome {
    blocks: Vec<ContentBlock>,
    parse_errors: HashMap<String, String>,
    /// `None` when the stream omitted it, in which case the content alone decides the next step.
    stop_reason: Option<StopReason>,
    stop_details: Option<StopDetails>,
}

/// Streams one conversation request and collects its reply with [`collect_response`].
async fn stream_response(
    client: &dyn AgentClient,
    messages: &[Message],
    tools: &[ToolDefinition],
    prompt: &PromptParts,
    sink: &dyn EventSink,
    usage: &mut Option<TokenUsage>,
) -> Result<StreamOutcome> {
    let section_refs: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
    let rx = client.stream_message(messages, &section_refs, Some(&prompt.user_context), tools)?;
    collect_response(rx, sink, usage).await
}

/// Accumulates one response stream into its reply. `usage` tracks what the API has reported so
/// far, so it stays accurate when the stream fails or the caller drops the future.
async fn collect_response(
    mut rx: mpsc::Receiver<Result<StreamEvent>>,
    sink: &dyn EventSink,
    usage: &mut Option<TokenUsage>,
) -> Result<StreamOutcome> {
    let mut blocks: Vec<Option<BlockAccumulator>> = Vec::new();
    let mut stop_reason = None;
    let mut stop_details = None;
    let mut finished = false;

    while let Some(event) = rx.recv().await {
        let event = event.context("stream error")?;

        match event {
            StreamEvent::MessageStart { message } => {
                // A retried request starts over with its own `message_start`, so the usage of the
                // failed attempt is discarded.
                *usage = None;
                if let Some(observed) = message.usage {
                    usage.get_or_insert_default().observe(&observed);
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
                match &acc {
                    BlockAccumulator::Text(text) if !text.is_empty() => {
                        _ = sink.send(Event::StreamToken(text.clone()));
                    }
                    BlockAccumulator::ToolUse { .. } | BlockAccumulator::ServerToolUse { .. } => {
                        send_progress_note(&blocks[..index], sink);
                    }
                    _ => {}
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
            StreamEvent::Retrying { notice } => {
                sink.emit(Event::Retrying(notice.to_string()), "retry-notice");
            }
            StreamEvent::MessageDelta {
                delta,
                usage: observed,
            } => {
                if let Some(observed) = observed {
                    usage.get_or_insert_default().observe(&observed);
                }
                stop_reason = delta.stop_reason;
                stop_details = delta.stop_details;
            }
            StreamEvent::MessageStop => {
                finished = true;
                break;
            }
            _ => {}
        }
    }
    // A stream cut before `message_stop` may hold a partial block, so it never becomes a reply.
    if !finished {
        bail!("response stream ended before message_stop");
    }

    let mut outcome = StreamOutcome {
        stop_reason,
        stop_details,
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

/// Sends the progress note for a tool use whose block starts after `preceding`.
fn send_progress_note(preceding: &[Option<BlockAccumulator>], sink: &dyn EventSink) {
    let thinking = preceding
        .iter()
        .map(|block| block.as_ref().and_then(BlockAccumulator::thinking_text));
    if let Some(note) = progress_note(thinking) {
        _ = sink.send(Event::ProgressUpdate(note.to_owned()));
    }
}

fn apply_delta(block: &mut BlockAccumulator, delta: Delta, sink: &dyn EventSink) {
    match (block, delta) {
        (BlockAccumulator::Text(buf), Delta::TextDelta { text }) => {
            buf.push_str(&text);
            _ = sink.send(Event::StreamToken(text));
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
            _ = sink.send(Event::ThinkingToken(thinking_delta));
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
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::testing::test_client;
    use crate::client::anthropic::wire::{
        ApiError, ContentBlockInfo, MessageDeltaBody, MessageResponse, StopDetails, StopReason,
        StreamEvent, Usage,
    };
    use crate::config::auth::{Auth, AuthSource};
    use crate::config::{AutoCompactionConfig, Effort};
    use crate::file_tracker::FileTracker;
    use crate::message::Role;
    use crate::model::ResolvedModelId;
    use crate::permission::GateTarget;
    use crate::protocol::CapturingSink;
    use crate::session::handle::{self, SessionHandle};
    use crate::session::store::testing::test_store;
    use crate::tool::{
        DEFAULT_TOOL_ICON, ReadExcerptLine, Tool, ToolDefinition, ToolMetadata, ToolOutput,
        ToolRegistry, ToolResultView,
    };

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
    fn turn_abort_display_cancelled_and_shutdown_use_static_labels() {
        assert_eq!(format!("{}", TurnAbort::Cancelled), "turn cancelled");
        assert_eq!(format!("{}", TurnAbort::Shutdown), "engine shut down");
    }

    // ── Fixtures ──

    /// In-process fake that scripts a [`StreamEvent`] sequence per turn and records each request's
    /// transcript.
    struct FakeClient {
        turns: StdMutex<VecDeque<Vec<StreamEvent>>>,
        requests: StdMutex<Vec<Vec<Message>>>,
    }

    impl FakeClient {
        fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                turns: StdMutex::new(turns.into()),
                requests: StdMutex::default(),
            }
        }

        fn requests(&self) -> Vec<Vec<Message>> {
            self.requests.lock().unwrap().clone()
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
            messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
            self.requests.lock().unwrap().push(messages.to_vec());
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
                    model: "claude-sonnet-5-5".into(),
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
                    model: "claude-sonnet-5-5".into(),
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
                    model: "claude-sonnet-5-5".into(),
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
                    stop_reason: Some(StopReason::EndTurn),
                    stop_details: None,
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
                    model: "claude-sonnet-5-5".into(),
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
                    stop_reason: Some(StopReason::EndTurn),
                    stop_details: None,
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
                    model: "claude-sonnet-5-5".into(),
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

    enum Block<'a> {
        Thinking(&'a str),
        RedactedThinking,
        Text(&'a str),
        Echo(&'a str),
        ServerTool(&'a str),
    }

    /// One response streaming `blocks` in order. `Echo` and `ServerTool` blocks are `echo` and
    /// `web_search` calls by id.
    fn blocks_turn(blocks: &[Block<'_>]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            let (content_block, delta) = match block {
                Block::Thinking(text) => (
                    ContentBlockInfo::Thinking {
                        thinking: String::new(),
                        signature: "sig".into(),
                    },
                    Some(Delta::ThinkingDelta {
                        thinking: (*text).into(),
                    }),
                ),
                Block::RedactedThinking => (
                    ContentBlockInfo::RedactedThinking {
                        data: "opaque".into(),
                    },
                    None,
                ),
                Block::Text(text) => (
                    ContentBlockInfo::Text {
                        text: String::new(),
                    },
                    Some(Delta::TextDelta {
                        text: (*text).into(),
                    }),
                ),
                Block::Echo(id) => (
                    ContentBlockInfo::ToolUse {
                        id: (*id).into(),
                        name: "echo".into(),
                    },
                    Some(Delta::InputJsonDelta {
                        partial_json: "{}".into(),
                    }),
                ),
                Block::ServerTool(id) => (
                    ContentBlockInfo::ServerToolUse {
                        id: (*id).into(),
                        name: "web_search".into(),
                    },
                    Some(Delta::InputJsonDelta {
                        partial_json: "{}".into(),
                    }),
                ),
            };
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block,
            });
            events.extend(delta.map(|delta| StreamEvent::ContentBlockDelta { index, delta }));
            events.push(StreamEvent::ContentBlockStop { index });
        }
        events.push(StreamEvent::MessageStop);
        events
    }

    /// One turn holding a `tool_use` block per `(id, name, input_json)`.
    fn tool_uses_turn(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
        let mut events: Vec<StreamEvent> = calls
            .iter()
            .enumerate()
            .flat_map(|(index, (id, name, input_json))| {
                [
                    StreamEvent::ContentBlockStart {
                        index,
                        content_block: ContentBlockInfo::ToolUse {
                            id: (*id).into(),
                            name: (*name).into(),
                        },
                    },
                    StreamEvent::ContentBlockDelta {
                        index,
                        delta: Delta::InputJsonDelta {
                            partial_json: (*input_json).into(),
                        },
                    },
                    StreamEvent::ContentBlockStop { index },
                ]
            })
            .collect();
        events.push(StreamEvent::MessageStop);
        events
    }

    /// Adds a `message_delta` carrying `stop_reason` just before the turn's `message_stop`.
    fn with_stop(
        mut events: Vec<StreamEvent>,
        stop_reason: StopReason,
        stop_details: Option<StopDetails>,
    ) -> Vec<StreamEvent> {
        let stop = events.pop();
        assert!(matches!(stop, Some(StreamEvent::MessageStop)), "{stop:?}");
        events.push(StreamEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some(stop_reason),
                stop_details,
            },
            usage: None,
        });
        events.push(StreamEvent::MessageStop);
        events
    }

    /// Text block followed by a `tool_use` block, as one turn.
    fn text_then_tool_use_turn(
        text: &str,
        id: &str,
        name: &str,
        input_json: &str,
    ) -> Vec<StreamEvent> {
        let mut events = text_turn(text);
        events.pop();
        for event in tool_use_turn(id, name, input_json) {
            events.push(match event {
                StreamEvent::ContentBlockStart { content_block, .. } => {
                    StreamEvent::ContentBlockStart {
                        index: 1,
                        content_block,
                    }
                }
                StreamEvent::ContentBlockDelta { delta, .. } => {
                    StreamEvent::ContentBlockDelta { index: 1, delta }
                }
                StreamEvent::ContentBlockStop { .. } => StreamEvent::ContentBlockStop { index: 1 },
                other => other,
            });
        }
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

        fn permission_class(&self) -> crate::permission::PermissionClass {
            crate::permission::PermissionClass::Bash
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

    /// Path tool whose result view shows the input and metadata it was resolved from.
    struct PathViewTool;

    impl Tool for PathViewTool {
        fn name(&self) -> &'static str {
            "path_view"
        }

        fn description(&self) -> &'static str {
            "view a path"
        }

        fn permission_class(&self) -> crate::permission::PermissionClass {
            crate::permission::PermissionClass::Read
        }

        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn path_field(&self) -> Option<&'static str> {
            Some("file_path")
        }

        fn run(
            &self,
            input: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
            Box::pin(async move {
                if input.get("fail").is_some() {
                    return ToolOutput::error("cannot view notes");
                }
                ToolOutput {
                    content: input["file_path"].as_str().unwrap_or_default().to_owned(),
                    is_error: false,
                    metadata: ToolMetadata {
                        title: Some("Viewed notes".into()),
                        ..Default::default()
                    },
                }
            })
        }

        fn result_view(
            &self,
            input: &serde_json::Value,
            _content: &str,
            metadata: &ToolMetadata,
        ) -> Option<ToolResultView> {
            Some(ToolResultView::ReadExcerpt {
                path: input["file_path"].as_str().unwrap_or_default().to_owned(),
                lines: vec![ReadExcerptLine {
                    number: 1,
                    text: metadata.title.clone().unwrap_or_default(),
                }],
                total_lines: 1,
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

        fn permission_class(&self) -> crate::permission::PermissionClass {
            crate::permission::PermissionClass::Bash
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
    fn inert_ops() -> mpsc::Receiver<Op> {
        let (tx, rx) = crate::protocol::inert_op_channel();
        std::mem::forget(tx);
        rx
    }

    /// The tool calls and results among `events`, each in emission order.
    fn tool_events(events: &[Event]) -> (Vec<ToolCall>, Vec<ToolResult>) {
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for event in events {
            match event {
                Event::ToolCallStart(call) => calls.push(call.clone()),
                Event::ToolCallEnd(result) => results.push(result.clone()),
                _ => {}
            }
        }
        (calls, results)
    }

    fn empty_prompt() -> PromptParts {
        PromptParts::default()
    }

    fn round_totals(report: &TurnReport) -> Vec<u32> {
        report
            .billable_rounds
            .iter()
            .map(|usage| usage.total_tokens())
            .collect()
    }

    /// Gate that bypasses every rule (`yolo`), so the dispatch tests exercise the turn loop without
    /// the permission layer interceding. Permission-specific behavior is covered in `permission`.
    fn yolo_gate() -> GateContext<'static> {
        static POLICY: std::sync::LazyLock<crate::permission::Policy> =
            std::sync::LazyLock::new(|| {
                crate::permission::Policy::new(crate::permission::Mode::Yolo, vec![], vec![])
            });
        GateContext {
            policy: &POLICY,
            cwd: std::path::Path::new("."),
            interactive: false,
        }
    }

    /// Gate over allow / deny rule sets, leaking the policy like [`yolo_gate`]. `echo` is a
    /// `bash`-class tool with no gate target, so only a class-wide rule matches it.
    fn gate_with(
        mode: crate::permission::Mode,
        allow: &[&str],
        deny: &[&str],
        interactive: bool,
    ) -> GateContext<'static> {
        let owned = |rules: &[&str]| rules.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let policy = Box::leak(Box::new(
            crate::permission::Policy::resolve(
                mode,
                &owned(allow),
                &owned(deny),
                std::path::Path::new("."),
            )
            .expect("rules parse"),
        ));
        GateContext {
            policy,
            cwd: std::path::Path::new("."),
            interactive,
        }
    }

    /// Answers every approval request with `decision` over the agent's own `ops`, the way the
    /// TUI's approval modal does, while capturing every event.
    struct ReplyingSink {
        events: CapturingSink,
        op_tx: mpsc::Sender<Op>,
        decision: Decision,
    }

    impl EventSink for ReplyingSink {
        fn send(&self, event: Event) -> Result<()> {
            if let Event::ApprovalRequested { id, .. } = &event {
                self.op_tx
                    .try_send(Op::Resolve {
                        id: id.clone(),
                        decision: self.decision,
                    })
                    .expect("user channel has room");
            }
            self.events.send(event)
        }
    }

    fn test_session(dir: &std::path::Path) -> SessionHandle {
        let store = test_store(dir);
        handle::start(&store, "claude-sonnet-5-5")
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
            &mut inert_ops(),
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(100),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
                matches!(event, Event::Error(message) if message.contains("Auto-compaction failed"))
            }),
            "summarizer failure must surface a user-visible Event::Error",
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
                matches!(event, Event::Error(message) if message.contains("Auto-compaction disabled"))
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
                .any(|event| matches!(event, Event::Error(message) if message.contains("Session write failed")))
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(50_000),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
            &mut inert_ops(),
            &mut pending,
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(50_000),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        let mut auto = AutoCompact {
            config: AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(10),
            },
            failures: &mut failures,
            file_tracker: &tracker,
            prompt: &empty_prompt(),
            tools: &[],
            spent: &mut Vec::new(),
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
            tx.send(Op::Queue("queued after summary".into()))
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
                .filter(|event| matches!(event, Event::PromptDrained(_)))
                .count(),
            0
        );
        assert!(sink.events().iter().any(|event| matches!(
            event,
            Event::SessionCompacted {
                automatic: true,
                ..
            }
        )));
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("auto summary"))
        );
    }

    #[tokio::test]
    async fn auto_compact_if_needed_retries_an_overflowing_summary_without_the_oldest_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            with_stop(
                text_turn("partial"),
                StopReason::ModelContextWindowExceeded,
                None,
            ),
            text_turn("auto summary"),
        ]);
        let tracker = FileTracker::default();
        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        let mut failures = 0;

        let compacted = auto_compact_if_needed(
            &client,
            &session,
            &mut messages,
            &CapturingSink::new(),
            &mut inert_ops(),
            &mut Vec::new(),
            Some(&mut AutoCompact {
                config: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                },
                failures: &mut failures,
                file_tracker: &tracker,
                prompt: &empty_prompt(),
                tools: &[],
                spent: &mut Vec::new(),
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

        assert!(compacted);
        let sent_texts: Vec<Vec<String>> = client
            .requests()
            .iter()
            .map(|request| {
                request
                    .iter()
                    .flat_map(|m| &m.content)
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        let [first, retry] = &sent_texts[..] else {
            panic!("expected the overflow and one retry: {sent_texts:?}");
        };
        assert_eq!(first[..4], ["hi", "there", "next", "done"]);
        assert!(
            !retry
                .iter()
                .any(|text| ["hi", "there", "next"].contains(&text.as_str())),
            "the retry keeps only the newest round: {retry:?}",
        );
        assert!(retry.iter().any(|text| text == "done"), "{retry:?}");
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        let events = sink.events();
        let error_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Error(m) if m.contains("Session write failed")))
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        let events = sink.events();
        let error_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, Event::Error(m) if m.contains("Session write failed")))
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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
                Event::StreamToken(t) => Some(t),
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
        let mut ops = inert_ops();
        let mut messages = vec![Message::user("hi")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut ops,
            None,
            &yolo_gate(),
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
                Event::StreamToken(t) => Some(t),
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        assert_eq!(report.usage.map(TokenUsage::total_tokens), Some(107));
        assert_eq!(round_totals(&report), [107]);
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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
        assert_eq!(report.billable_rounds, [usage]);
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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

        let (calls, results) = tool_events(&sink.events());
        assert_eq!(
            calls,
            [ToolCall {
                id: "tool_1".to_owned(),
                name: "echo".to_owned(),
                input: serde_json::json!({"v": 42}),
                icon: DEFAULT_TOOL_ICON.to_owned(),
                label: "Echo".to_owned(),
            }]
        );
        assert_eq!(
            results,
            [ToolResult {
                id: "tool_1".to_owned(),
                content: r#"{"v":42}"#.to_owned(),
                is_error: false,
                title: Some("echoed".to_owned()),
                view: ToolResultView::Text {
                    content: r#"{"v":42}"#.to_owned(),
                },
            }]
        );
    }

    #[tokio::test]
    async fn agent_turn_resolves_result_views_from_the_unpinned_input_and_tool_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "path_view", r#"{"file_path":"notes.txt"}"#),
            tool_use_turn(
                "tool_2",
                "path_view",
                r#"{"file_path":"notes.txt","fail":true}"#,
            ),
            text_turn("Done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(PathViewTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("view notes")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        let mut pinned = json!({"file_path": "notes.txt"});
        PathViewTool.pin_target(
            &mut pinned,
            &GateTarget::for_path("notes.txt", Path::new(".")),
        );
        let pinned = pinned["file_path"].as_str().unwrap();
        assert_ne!(
            pinned, "notes.txt",
            "the gate must pin the path the tool runs on"
        );
        let (_, results) = tool_events(&sink.events());
        assert_eq!(
            results,
            [
                ToolResult {
                    id: "tool_1".to_owned(),
                    content: pinned.to_owned(),
                    is_error: false,
                    title: Some("Viewed notes".to_owned()),
                    view: ToolResultView::ReadExcerpt {
                        path: "notes.txt".to_owned(),
                        lines: vec![ReadExcerptLine {
                            number: 1,
                            text: "Viewed notes".to_owned(),
                        }],
                        total_lines: 1,
                    },
                },
                ToolResult {
                    id: "tool_2".to_owned(),
                    content: "cannot view notes".to_owned(),
                    is_error: true,
                    title: None,
                    view: ToolResultView::Text {
                        content: "cannot view notes".to_owned(),
                    },
                },
            ]
        );
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        assert_eq!(report.usage.map(TokenUsage::total_tokens), Some(3));
        assert_eq!(round_totals(&report), [11, 3]);
        assert_eq!(
            sink.events()
                .iter()
                .filter(|event| matches!(event, Event::SessionCompacted { .. }))
                .count(),
            0
        );
        assert!(matches!(&messages[5].content[0], ContentBlock::Text { text } if text == "Done"));
    }

    #[tokio::test]
    async fn agent_turn_drains_mid_round_submit_into_messages_at_round_boundary() {
        // Pre-loaded `Op::Queue` is consumed during the round. At the boundary the agent splices
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
        let (tx, mut rx) = mpsc::channel::<Op>(4);
        // Hold the sender so `recv()` stays pending after the queued submit.
        tx.send(Op::Queue("follow up".into())).await.unwrap();

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            None,
            &yolo_gate(),
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
            .filter(|e| matches!(e, Event::PromptDrained(t) if t == "follow up"))
            .count();
        assert_eq!(
            drained_count, 1,
            "exactly one PromptDrained event for the queued prompt",
        );
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_drains_multiple_mid_round_submits_in_order() {
        // Two `Op::Queue`s during one await must land as separate User messages in dispatch order
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
        let (tx, mut rx) = mpsc::channel::<Op>(4);
        tx.send(Op::Queue("first".into())).await.unwrap();
        tx.send(Op::Queue("second".into())).await.unwrap();

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            None,
            &yolo_gate(),
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
                Event::PromptDrained(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(drained, vec!["first".to_owned(), "second".to_owned()]);
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_interrupt_during_stream_is_cancelled_abort() {
        // Biased select picks the queued Cancel before the stream future, so the session stays at
        // its pre-turn tail and the abort is Cancelled.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("never reached")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::Interrupt).unwrap();

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            None,
            &yolo_gate(),
        )
        .await
        .expect_err("cancel must surface as Err(Cancelled)");

        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        assert_eq!(messages.len(), 1, "no assistant message recorded");
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_shutdown_during_stream_is_shutdown_abort() {
        // Shutdown is the teardown signal. It must stay distinct from an interrupt for the engine.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("never reached")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::Shutdown).unwrap();

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            None,
            &yolo_gate(),
        )
        .await
        .expect_err("shutdown must surface as Err(Shutdown)");

        assert!(matches!(abort, TurnAbort::Shutdown), "got {abort:?}");
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_sender_drop_during_turn_collapses_to_shutdown_abort() {
        // Closed channel resolves `recv()` to None. Treated as Shutdown so the engine stops.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![text_turn("never reached")]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
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
            &yolo_gate(),
        )
        .await
        .expect_err("dead channel must surface as Err(Shutdown)");

        assert!(matches!(abort, TurnAbort::Shutdown), "got {abort:?}");
    }

    #[tokio::test]
    async fn agent_turn_absorbs_stragglers_without_killing_turn() {
        // Every catch-all variant in `await_unless_aborted` must let the turn complete. Returning
        // Cancelled for any of these would kill the turn from a buffered no-op.
        for op in [
            Op::Prompt("late".to_owned()),
            Op::Resolve {
                id: "stale".to_owned(),
                decision: Decision::Approve,
            },
            Op::SetModel {
                model: Some(ResolvedModelId::new("claude-opus-5-5".to_owned())),
                effort: Some(Effort::High),
            },
            Op::Compact { instructions: None },
            Op::Clear,
            Op::Archive,
            Op::Resume {
                path: "/repo/other.jsonl".into(),
            },
            Op::Rename {
                title: "Late".to_owned(),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let session = test_session(dir.path());
            let client = FakeClient::new(vec![text_turn("Hello!")]);
            let tools = ToolRegistry::new(vec![]);
            let sink = CapturingSink::new();
            let mut messages = vec![Message::user("hi")];
            let (tx, mut rx) = mpsc::channel::<Op>(1);
            tx.try_send(op.clone()).unwrap();

            let outcome = agent_turn(
                &client,
                &tools,
                &mut messages,
                &empty_prompt(),
                &sink,
                &session,
                &mut rx,
                None,
                &yolo_gate(),
            )
            .await;
            assert!(outcome.result.is_ok(), "turn must complete despite {op:?}");

            assert_eq!(messages.len(), 2, "assistant message recorded for {op:?}");
            drop(tx);
        }
    }

    #[tokio::test]
    async fn agent_turn_interrupt_during_tool_round_commits_finished_and_interrupted_results() {
        // Synchronizes on `GateTool`'s started signal so Cancel races the tool future, not the
        // prior stream phase.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![tool_uses_turn(&[
            ("tool_1", "echo", r#"{"v":1}"#),
            ("tool_2", "gate", "{}"),
            ("tool_3", "echo", "{}"),
        ])]);
        let started = Arc::new(Notify::new());
        let tools = ToolRegistry::new(vec![
            Box::new(EchoTool),
            Box::new(GateTool {
                started: started.clone(),
            }),
        ]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("kick off")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        let prompt = empty_prompt();
        let gate = yolo_gate();

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
                &gate,
            ),
            async {
                started.notified().await;
                tx.send(Op::Interrupt).await.unwrap();
            },
        );

        let abort = turn_result.expect_err("cancel must surface as Err(Cancelled)");
        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        let [_, assistant, results] = &messages[..] else {
            panic!("the round commits as one assistant / user pair: {messages:#?}");
        };
        assert_eq!(assistant.content.len(), 3, "{assistant:?}");
        let results: Vec<(&str, &str, bool)> = results
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => (tool_use_id.as_str(), content.as_str(), *is_error),
                other => panic!("expected ToolResult, got {other:?}"),
            })
            .collect();
        assert_eq!(
            results,
            [
                ("tool_1", r#"{"v":1}"#, false),
                (
                    "tool_2",
                    "The user interrupted this call before it finished, so it may have run \
                     partially.",
                    true,
                ),
                (
                    "tool_3",
                    "Not run, because the user interrupted the turn.",
                    true
                ),
            ],
        );
        let events = sink.events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::ToolCallStart(call) if call.id == "tool_3")),
            "a call after the interrupt never starts: {events:?}",
        );
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_interrupt_during_tool_round_preserves_completed_round_usage() {
        // Round 1's stream observes usage before the tool gates. Cancel arrives during the tool
        // execution, so the abort must still carry billable_rounds from the completed round.
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
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        let prompt = empty_prompt();
        let gate = yolo_gate();

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
                &gate,
            ),
            async {
                started.notified().await;
                tx.send(Op::Interrupt).await.unwrap();
            },
        );

        assert!(
            matches!(outcome.result, Err(TurnAbort::Cancelled)),
            "got {:?}",
            outcome.result,
        );
        assert_eq!(
            round_totals(&outcome.report),
            [10],
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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
                && content.contains("Tool input JSON failed to parse")
                && content.contains("Retry with a valid JSON object"),
        ));

        // Sink also reflects the failure via ToolCallEnd { is_error: true }.
        let events = sink.events();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd(ToolResult { id, is_error: true, content, .. })
                if id == "tool_1" && content.contains("Tool input JSON failed to parse"),
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
            &mut inert_ops(),
            Some(CAP),
            &yolo_gate(),
        )
        .await
        .expect_err("cap must trip");
        let msg = format!("{err:#}");
        assert!(msg.contains(&CAP.to_string()), "cap in error: {msg}");
        assert!(msg.contains("safety cap"), "explains intent: {msg}");
    }

    #[tokio::test]
    async fn agent_turn_safety_cap_bail_preserves_completed_round_usage() {
        // Each capped round reports usage. The cap-trip abort must still fold every round into
        // billable_rounds so the caller bills what the API charged for.
        const CAP: u32 = 2;
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let turns: Vec<Vec<StreamEvent>> = (0..CAP)
            .map(|i| tool_use_turn_with_usage(&format!("tool_{i}"), "echo", r"{}", 5, 2))
            .collect();
        let client = FakeClient::new(turns);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("loop forever")];

        let outcome = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            Some(CAP),
            &yolo_gate(),
        )
        .await;

        assert!(
            matches!(outcome.result, Err(TurnAbort::Failed(_))),
            "got {:?}",
            outcome.result,
        );
        assert_eq!(
            round_totals(&outcome.report),
            vec![7; CAP as usize],
            "every capped round must reach billable_rounds: {:?}",
            outcome.report,
        );
    }

    #[tokio::test]
    async fn agent_turn_with_none_cap_runs_unbounded_until_text_only_reply() {
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
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
    async fn agent_turn_failed_after_completed_round_preserves_billable_rounds() {
        // Round 1 completes with reported usage. Round 2 emits a stream Error so the turn
        // bails with TurnAbort::Failed. The completed round's usage must still ride out on
        // billable_rounds so the caller bills what the API charged for.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn_with_usage("tool_1", "echo", r"{}", 9, 4),
            vec![StreamEvent::Error {
                error: ApiError {
                    error_type: "overloaded_error".into(),
                    message: "Servers overloaded".into(),
                },
            }],
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        let outcome = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await;

        assert!(
            matches!(outcome.result, Err(TurnAbort::Failed(_))),
            "got {:?}",
            outcome.result,
        );
        assert_eq!(
            round_totals(&outcome.report),
            [13],
            "round 1 usage must reach the caller despite the bail: {:?}",
            outcome.report,
        );
    }

    #[tokio::test]
    async fn agent_turn_emits_each_progress_update_as_its_tool_use_starts_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            blocks_turn(&[
                Block::Thinking(""),
                Block::Thinking("Searching."),
                Block::ServerTool("s1"),
                Block::Text("Found it."),
                Block::Echo("t1"),
                Block::Thinking("Now b."),
                Block::RedactedThinking,
                Block::Echo("t2"),
                Block::Thinking("Summing up."),
                Block::Text("Checked."),
            ]),
            text_turn("Done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("run echo twice")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        let order: Vec<String> = sink
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StreamToken(text) => Some(format!("token {text}")),
                Event::ProgressUpdate(text) => Some(format!("progress {text}")),
                Event::ToolCallStart(ToolCall { id, .. }) => Some(format!("start {id}")),
                Event::ToolCallEnd(ToolResult { id, .. }) => Some(format!("end {id}")),
                _ => None,
            })
            .collect();
        // Text cuts `t1` off from any note, and a note before text introduces no tool call.
        assert_eq!(
            order,
            [
                "progress Searching.",
                "token Found it.",
                "progress Now b.",
                "token Checked.",
                "start t1",
                "end t1",
                "start t2",
                "end t2",
                "token Done",
            ],
        );
        // Every block, the empty reasoning one included, stays in history for the next round.
        assert_eq!(messages[1].content.len(), 10);
    }

    #[tokio::test]
    async fn agent_turn_keeps_interrupted_work_note_when_tool_results_follow() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            blocks_turn(&[
                Block::Echo("t1"),
                Block::Thinking(INTERRUPTED_WORK_SENTINEL),
            ]),
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 4, "round 2 ran on the tool results");
        assert!(ends_on_interrupted_work(&messages[1].content));
        let events = sink.events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Error(_) | Event::ProgressUpdate(_))),
            "the loop resumes on its own: {events:?}",
        );
    }

    #[tokio::test]
    async fn agent_turn_ending_on_interrupted_work_tells_user_to_continue() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![blocks_turn(&[Block::Thinking(
            INTERRUPTED_WORK_SENTINEL,
        )])]);
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        assert!(sink.events().iter().any(|e| matches!(
            e,
            Event::Error(m) if m.contains("interrupted before it finished")
        )));
        assert_eq!(messages[1].content.len(), 1);
        assert!(ends_on_interrupted_work(&messages[1].content));
    }

    #[tokio::test]
    async fn agent_turn_pause_turn_resends_the_paused_reply_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            with_stop(text_turn("searching"), StopReason::PauseTurn, None),
            text_turn("found it"),
        ]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("look it up")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        let requests = client.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            matches!(
                &requests[1][..],
                [user, paused]
                    if user.role == Role::User
                        && paused.role == Role::Assistant
                        && matches!(&paused.content[..], [ContentBlock::Text { text }] if text == "searching"),
            ),
            "resume must end on the paused reply: {:?}",
            requests[1],
        );
        assert!(
            matches!(&messages[2].content[..], [ContentBlock::Text { text }] if text == "found it"),
            "{messages:?}",
        );
    }

    #[tokio::test]
    async fn agent_turn_max_tokens_keeps_prose_and_runs_no_cut_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![with_stop(
            text_then_tool_use_turn("Writing it now.", "tool_1", "echo", r#"{"content": "#),
            StopReason::MaxTokens,
            None,
        )]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("write the file")];

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .expect_err("a truncated reply fails the turn");

        assert_eq!(
            abort.to_string(),
            "The response hit the max_tokens output limit while writing the `echo` call, so no \
             tools ran. Ask for smaller steps, or raise `max_tokens`.",
        );
        assert!(
            matches!(&messages[1..], [reply] if matches!(&reply.content[..], [ContentBlock::Text { text }] if text == "Writing it now.")),
            "only the finished prose is kept: {messages:?}",
        );
        assert_eq!(client.requests().len(), 1, "the cut call is never retried");
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::ToolCallStart(_))),
        );
    }

    #[tokio::test]
    async fn agent_turn_empty_reply_completes_without_committing_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![with_stop(
            vec![StreamEvent::MessageStop],
            StopReason::EndTurn,
            None,
        )]);
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
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    #[tokio::test]
    async fn agent_turn_stream_cut_before_message_stop_fails_without_committing() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let mut events = text_turn("half a sent");
        events.pop();
        let client = FakeClient::new(vec![events]);
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        let abort = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await
        .expect_err("a cut stream fails the turn");

        assert!(
            format!("{abort:#}").contains("ended before message_stop"),
            "{abort:#}",
        );
        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    const HELLO_SSE: &str = indoc::indoc! {r#"
        event: message_start
        data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"output_tokens":0}}}

        event: content_block_start
        data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

        event: content_block_delta
        data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}

        event: content_block_stop
        data: {"type":"content_block_stop","index":0}

        event: message_stop
        data: {"type":"message_stop"}

    "#};

    fn sse(body: &'static str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }

    /// Drives one turn through the real `<Client as AgentClient>` path. The server answers with
    /// `responses` in order, one request each, and repeats the last one.
    async fn real_client_turn(
        responses: Vec<ResponseTemplate>,
        tools: &ToolRegistry,
    ) -> (TurnOutcome, Vec<Message>, CapturingSink) {
        let server = MockServer::start().await;
        let last = responses.len() - 1;
        for (i, response) in responses.into_iter().enumerate() {
            let mock = Mock::given(method("POST"))
                .and(wm_path("/v1/messages"))
                .respond_with(response);
            let mock = if i == last {
                mock
            } else {
                mock.up_to_n_times(1)
            };
            mock.mount(&server).await;
        }
        let client = test_client(
            server.uri(),
            Auth::new(AuthSource::ApiKeyEnv, "sk"),
            "claude-sonnet-5-5",
        );
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("hi")];

        let outcome = agent_turn(
            &client,
            tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut inert_ops(),
            None,
            &yolo_gate(),
        )
        .await;
        (outcome, messages, sink)
    }

    #[tokio::test]
    async fn agent_turn_drives_real_client_over_wiremock() {
        let (outcome, messages, _) =
            real_client_turn(vec![sse(HELLO_SSE)], &ToolRegistry::new(vec![])).await;

        outcome.unwrap();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[1].content[0], ContentBlock::Text { text } if text == "hello"),);
    }

    #[tokio::test]
    async fn agent_turn_real_client_announces_each_retry_before_the_reply() {
        let (outcome, messages, sink) = real_client_turn(
            vec![
                ResponseTemplate::new(529),
                ResponseTemplate::new(503),
                sse(HELLO_SSE),
            ],
            &ToolRegistry::new(vec![]),
        )
        .await;

        outcome.expect("the third attempt succeeds");
        assert_eq!(messages.len(), 2, "{messages:?}");
        let events = sink.events();
        let notices: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Retrying(message) => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [
                "HTTP 529. Retrying in 1s (retry 1 of 5).",
                "HTTP 503 Service Unavailable. Retrying in 1s (retry 2 of 5).",
            ],
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::Error(_))),
            "a recovered retry shows no error: {events:?}",
        );
        let last_notice = events
            .iter()
            .rposition(|e| matches!(e, Event::Retrying(_)))
            .unwrap();
        let first_token = events
            .iter()
            .position(|e| matches!(e, Event::StreamToken(_)))
            .unwrap();
        assert!(last_notice < first_token, "{events:?}");
    }

    #[tokio::test]
    async fn agent_turn_real_client_bills_only_the_attempt_that_answered() {
        let failed = indoc::indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"cache_creation_input_tokens":900,"output_tokens":0}}}

            event: error
            data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}

        "#};
        let answered = indoc::indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_2","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"cache_read_input_tokens":900,"output_tokens":0}}}

            event: content_block_start
            data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hello"}}

            event: content_block_stop
            data: {"type":"content_block_stop","index":0}

            event: message_delta
            data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}

            event: message_stop
            data: {"type":"message_stop"}

        "#};

        let (outcome, _, _) =
            real_client_turn(vec![sse(failed), sse(answered)], &ToolRegistry::new(vec![])).await;

        let usage = outcome.report.usage.expect("the answer reports usage");
        assert_eq!(
            (
                usage.input_tokens(),
                usage.cache_creation_input_tokens(),
                usage.cache_read_input_tokens(),
                usage.output_tokens(),
            ),
            (5, 0, 900, 7),
            "the failed attempt's cache write is not billed again",
        );
    }

    #[tokio::test]
    async fn agent_turn_real_client_refusal_discards_the_reply_and_names_the_policy() {
        let body = indoc::indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"output_tokens":0}}}

            event: content_block_start
            data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"echo","input":{}}}

            event: content_block_delta
            data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}

            event: content_block_stop
            data: {"type":"content_block_stop","index":0}

            event: message_delta
            data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","explanation":"Exploit development."}},"usage":{"output_tokens":3}}

            event: message_stop
            data: {"type":"message_stop"}

        "#};

        let (outcome, messages, sink) = real_client_turn(
            vec![sse(body)],
            &ToolRegistry::new(vec![Box::new(EchoTool)]),
        )
        .await;

        outcome.expect("a refusal ends the turn cleanly");
        assert_eq!(
            messages.len(),
            1,
            "the refused reply is dropped: {messages:?}"
        );
        let events = sink.events();
        assert!(
            !events.iter().any(|e| matches!(e, Event::ToolCallStart(_))),
            "refused tool calls never run",
        );
        let notices: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Error(message) => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [
                "The model declined this request (category: cyber): Exploit development. Rephrase \
              it, or run /clear to start over."
            ],
        );
    }

    #[tokio::test]
    async fn agent_turn_deny_rule_blocks_the_tool_and_returns_a_denial_result() {
        // The gate must sit on the dispatch path: a denied call never reaches the tool, and the
        // model sees the refusal as that call's tool_result.
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
            &mut inert_ops(),
            None,
            &gate_with(crate::permission::Mode::Auto, &[], &["bash"], true),
        )
        .await
        .unwrap();

        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = &messages[2].content[0]
        else {
            panic!("expected ToolResult, got {:?}", messages[2].content[0]);
        };
        assert_eq!(tool_use_id, "tool_1");
        assert!(*is_error);
        assert!(
            content.contains("denied by the permission policy"),
            "the echo must not have run: {content}",
        );
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::ApprovalRequested { .. })),
            "a deny rule never prompts",
        );
    }

    #[tokio::test]
    async fn agent_turn_approval_decision_arriving_mid_turn_settles_the_call() {
        // An `ask` blocks the round on `ops` until the reply lands, and the reply alone decides
        // whether the tool runs.
        for (decision, runs) in [(Decision::Approve, true), (Decision::Deny, false)] {
            let dir = tempfile::tempdir().unwrap();
            let session = test_session(dir.path());
            let client = FakeClient::new(vec![
                tool_use_turn("tool_1", "echo", r#"{"v":42}"#),
                text_turn("Done"),
            ]);
            let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
            let (op_tx, mut ops) = mpsc::channel(4);
            let sink = ReplyingSink {
                events: CapturingSink::new(),
                op_tx,
                decision,
            };
            let mut messages = vec![Message::user("run echo")];

            agent_turn(
                &client,
                &tools,
                &mut messages,
                &empty_prompt(),
                &sink,
                &session,
                &mut ops,
                None,
                &gate_with(crate::permission::Mode::Auto, &[], &[], true),
            )
            .await
            .unwrap();

            let prompts = sink
                .events
                .events()
                .into_iter()
                .filter(|e| matches!(e, Event::ApprovalRequested { id, .. } if id == "tool_1"))
                .count();
            assert_eq!(
                prompts, 1,
                "{decision:?}: one approval request per asked call"
            );
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = &messages[2].content[0]
            else {
                panic!("expected ToolResult, got {:?}", messages[2].content[0]);
            };
            if runs {
                assert_eq!(content, r#"{"v":42}"#);
                assert!(!is_error);
            } else {
                assert!(
                    content.contains("denied by the permission policy"),
                    "{content}"
                );
                assert!(*is_error);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_turn_write_through_a_dangling_symlink_is_never_auto_allowed() {
        // A dangling `notes.md -> ../escape.txt` must resolve to its outside target, so the write
        // never passes as a new in-cwd file and a headless session denies it.
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let cwd = root.join("repo");
        std::fs::create_dir(&cwd).unwrap();
        std::os::unix::fs::symlink("../escape.txt", cwd.join("notes.md")).unwrap();

        let session_dir = tempfile::tempdir().unwrap();
        let session = test_session(session_dir.path());
        let input = json!({"file_path": cwd.join("notes.md"), "content": "pwned"}).to_string();
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "write", &input),
            text_turn("Done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(crate::tool::write::WriteTool::new(
            Arc::new(FileTracker::default()),
        ))]);
        let gate = GateContext {
            cwd: &cwd,
            ..gate_with(crate::permission::Mode::Auto, &[], &[], false)
        };
        let mut messages = vec![Message::user("take notes")];

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &CapturingSink::new(),
            &session,
            &mut inert_ops(),
            None,
            &gate,
        )
        .await
        .unwrap();

        assert!(
            !root.join("escape.txt").exists(),
            "the write must not follow the link out of cwd",
        );
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &messages[2].content[0]
        else {
            panic!("expected ToolResult, got {:?}", messages[2].content[0]);
        };
        assert!(*is_error);
        assert!(
            content.contains("denied by the permission policy"),
            "{content}"
        );
    }

    // ── refusal_message ──

    #[test]
    fn refusal_message_without_details_still_offers_a_way_forward() {
        assert_eq!(
            refusal_message(None),
            "The model declined this request. Rephrase it, or run /clear to start over.",
        );
        let explanation_only = StopDetails {
            category: None,
            explanation: Some(" Unsafe request ".to_owned()),
        };
        assert_eq!(
            refusal_message(Some(&explanation_only)),
            "The model declined this request: Unsafe request. Rephrase it, or run /clear to start \
             over.",
        );
    }

    // ── truncation_error ──

    #[test]
    fn truncation_error_names_the_cause_and_whether_tools_were_skipped() {
        let text = || ContentBlock::Text {
            text: "prose".to_owned(),
        };
        let tool_use = ContentBlock::ToolUse {
            id: "tool_1".to_owned(),
            name: "echo".to_owned(),
            input: json!({}),
        };

        assert_eq!(
            truncation_error(StopReason::ModelContextWindowExceeded, &[text()]).to_string(),
            "The conversation filled the model's context window before it finished. Run \
             /compact, then continue.",
        );
        assert_eq!(
            truncation_error(StopReason::MaxTokens, &[tool_use, text()]).to_string(),
            "The response hit the max_tokens output limit before it finished, so no tools ran. \
             Ask for smaller steps, or raise `max_tokens`.",
        );
    }

    // ── notify_if_interrupted ──

    #[test]
    fn notify_if_interrupted_only_fires_on_a_final_interrupted_response() {
        let interrupted = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: INTERRUPTED_WORK_SENTINEL.to_owned(),
                signature: "sig".to_owned(),
            }],
        };
        let events = |messages: &[Message]| {
            let sink = CapturingSink::new();
            notify_if_interrupted(messages, &sink);
            sink.events()
                .iter()
                .map(|e| format!("{e:?}"))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            events(&[Message::user("hi"), interrupted.clone()]),
            [
                r#"Error("The response was interrupted before it finished. Send a message to continue.")"#
            ],
        );
        let none: [String; 0] = [];
        assert_eq!(events(&[]), none);
        assert_eq!(events(&[interrupted, Message::user("go on")]), none);
        assert_eq!(
            events(&[Message::user("hi"), Message::assistant("done")]),
            none
        );
    }

    // ── dispatch_tool_call ──

    #[tokio::test]
    async fn dispatch_tool_call_interrupt_during_approval_reports_the_call_not_run() {
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &[], &[], true);
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::Interrupt).unwrap();

        let (output, abort) = dispatch_tool_call(
            &tools,
            "tool_1",
            "echo",
            json!({"v": 1}),
            None,
            &sink,
            &mut rx,
            &mut Vec::new(),
            &gate,
        )
        .await;

        assert!(matches!(abort, Some(TurnAbort::Cancelled)), "got {abort:?}");
        assert!(
            sink.events()
                .iter()
                .any(|e| matches!(e, Event::ApprovalRequested { id, .. } if id == "tool_1")),
            "the cancel lands while the approval is pending",
        );
        assert_eq!(
            (output.content.as_str(), output.is_error),
            ("Not run, because the user interrupted the turn.", true),
        );
    }

    // ── check_permission ──

    #[tokio::test]
    async fn check_permission_allow_proceeds_without_an_approval_event() {
        // `echo` is a bash-class tool, so `auto` asks by default, and a class-wide allow clears it.
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &["bash"], &[], true);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("allow must not abort");

        assert!(denial.is_none(), "an allowed call proceeds");
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::ApprovalRequested { .. })),
            "allow must not surface an approval prompt",
        );
    }

    #[tokio::test]
    async fn check_permission_deny_rule_short_circuits_to_a_synthetic_denial() {
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &[], &["bash"], true);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("deny resolves without abort")
        .expect("a deny rule must yield a synthetic denial");

        assert!(
            denial.is_error,
            "denial is surfaced as a failed tool result"
        );
        assert!(denial.content.contains("denied by the permission policy"));
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::ApprovalRequested { .. })),
            "a deny rule never prompts",
        );
    }

    #[tokio::test]
    async fn check_permission_ask_in_non_interactive_session_denies_without_prompting() {
        // The bare REPL and headless modes have no modal surface, so an `ask` resolves to deny.
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &[], &[], false);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("non-interactive ask resolves without abort")
        .expect("non-interactive ask denies");

        assert!(denial.is_error);
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::ApprovalRequested { .. })),
            "non-interactive ask must not emit an approval event no UI can answer",
        );
    }

    #[tokio::test]
    async fn check_permission_ask_emits_event_and_approve_proceeds() {
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &[], &[], true);
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::Resolve {
            id: "tool_1".to_owned(),
            decision: Decision::Approve,
        })
        .unwrap();

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut rx,
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("approve resolves without abort");

        assert!(denial.is_none(), "an approved call proceeds");
        let prompted = sink.events().into_iter().find_map(|e| match e {
            Event::ApprovalRequested { id, .. } => Some(id),
            _ => None,
        });
        assert_eq!(
            prompted.as_deref(),
            Some("tool_1"),
            "ask must surface an approval event carrying the call id",
        );
    }

    #[tokio::test]
    async fn check_permission_ask_undeliverable_event_fails_closed() {
        // If the approval event can't be delivered, no modal can answer it, so the gate must deny
        // rather than block in await_approval forever. An inert receiver would hang on a regression.
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = crate::protocol::FailingSink;
        let gate = gate_with(crate::permission::Mode::Auto, &[], &[], true);
        let (_tx, mut rx) = mpsc::channel::<Op>(1);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut rx,
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("an undeliverable approval resolves without abort")
        .expect("an undeliverable approval yields a synthetic denial");

        assert!(denial.is_error);
        assert!(denial.content.contains("denied by the permission policy"));
    }

    #[tokio::test]
    async fn check_permission_ask_deny_decision_yields_a_synthetic_denial() {
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &[], &[], true);
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::Resolve {
            id: "tool_1".to_owned(),
            decision: Decision::Deny,
        })
        .unwrap();

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut rx,
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("deny decision resolves without abort")
        .expect("a deny decision yields a synthetic denial");

        assert!(denial.is_error);
        assert!(denial.content.contains("denied by the permission policy"));
    }

    #[tokio::test]
    async fn check_permission_unknown_tool_proceeds_so_run_can_report_it() {
        // An unknown tool has no permission class, so the gate steps aside and lets `tools.run`
        // emit its own "unknown tool" error. A class-wide deny must not catch it.
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let gate = gate_with(crate::permission::Mode::Auto, &[], &["bash"], true);
        let denial = check_permission(
            &tools,
            "tool_1",
            "mystery",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("unknown tool resolves without abort");

        assert!(denial.is_none(), "unknown tool bypasses the gate");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_permission_pins_a_path_input_to_the_resolved_target() {
        // The tool must act on the file the gate checked, so a relative or symlinked `file_path` is
        // rewritten to the resolved location before the preview and the run see it.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();
        std::fs::write(cwd.join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink("real.txt", cwd.join("link.txt")).unwrap();
        let tools = ToolRegistry::new(vec![Box::new(crate::tool::read::ReadTool::new(Arc::new(
            FileTracker::default(),
        )))]);
        let gate = GateContext {
            cwd: &cwd,
            ..gate_with(crate::permission::Mode::Auto, &[], &[], true)
        };
        let mut input = json!({"file_path": "link.txt"});

        let denial = check_permission(
            &tools,
            "tool_1",
            "read",
            &mut input,
            &CapturingSink::new(),
            &mut inert_ops(),
            &mut Vec::new(),
            &gate,
        )
        .await
        .expect("a read resolves without abort");

        assert!(denial.is_none(), "a read-class call proceeds");
        assert_eq!(input["file_path"], json!(cwd.join("real.txt")));
    }

    // ── await_approval ──

    #[tokio::test]
    async fn await_approval_ignores_a_stale_decision_for_another_call() {
        // A decision for an already-resolved call must not satisfy the wait, but the matching id does.
        let (tx, mut rx) = mpsc::channel::<Op>(2);
        tx.try_send(Op::Resolve {
            id: "other".to_owned(),
            decision: Decision::Approve,
        })
        .unwrap();
        tx.try_send(Op::Resolve {
            id: "tool_1".to_owned(),
            decision: Decision::Deny,
        })
        .unwrap();

        let decision = await_approval("tool_1", &mut rx, &mut Vec::new())
            .await
            .expect("matching decision resolves");
        assert_eq!(decision, Decision::Deny);
    }

    #[tokio::test]
    async fn await_approval_buffers_a_mid_approval_submit_into_pending() {
        let (tx, mut rx) = mpsc::channel::<Op>(2);
        tx.try_send(Op::Queue("queued".to_owned())).unwrap();
        tx.try_send(Op::Resolve {
            id: "tool_1".to_owned(),
            decision: Decision::Approve,
        })
        .unwrap();
        let mut pending = Vec::new();

        let decision = await_approval("tool_1", &mut rx, &mut pending)
            .await
            .expect("approval resolves");
        assert_eq!(decision, Decision::Approve);
        assert_eq!(pending, vec!["queued".to_owned()], "the submit buffers");
    }

    #[tokio::test]
    async fn await_approval_interrupt_and_shutdown_abort_the_turn() {
        for (op, want_shutdown) in [(Op::Interrupt, false), (Op::Shutdown, true)] {
            let (tx, mut rx) = mpsc::channel::<Op>(1);
            tx.try_send(op.clone()).unwrap();
            let abort = await_approval("tool_1", &mut rx, &mut Vec::new())
                .await
                .expect_err("interrupt / shutdown abort the wait");
            if want_shutdown {
                assert!(matches!(abort, TurnAbort::Shutdown), "{op:?}");
            } else {
                assert!(matches!(abort, TurnAbort::Cancelled), "{op:?}");
            }
        }
    }

    #[tokio::test]
    async fn await_approval_dropped_channel_collapses_to_shutdown() {
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        drop(tx);
        let abort = await_approval("tool_1", &mut rx, &mut Vec::new())
            .await
            .expect_err("a closed channel aborts");
        assert!(matches!(abort, TurnAbort::Shutdown));
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
    fn parse_tool_json_empty_buffer_is_an_empty_object() {
        assert_eq!(parse_tool_json(""), (json!({}), None));
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
        assert_eq!(json_buf, "");
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
        assert_eq!(json_buf, "");
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
        assert!(matches!(&events[0], Event::StreamToken(t) if t == "llo"));
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
        assert_eq!(sink.events(), []);
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
        assert_eq!(sink.events(), []);
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
        assert!(matches!(&events[0], Event::ThinkingToken(t) if t == ", step 2"));
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
        assert_eq!(sink.events(), []);
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
        assert_eq!(sink.events(), []);
    }
}
