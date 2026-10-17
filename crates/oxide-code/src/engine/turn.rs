//! Agent turn loop. Streams the model response, runs each round's tool calls through
//! [`super::dispatch`], records to the session, and stops on a text-only response or the optional
//! per-turn round cap.

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;

use super::abort::{AbortResult, Inbox, TurnAbort, await_unless_aborted};
use super::dispatch::{self, DispatchContext};
use crate::client::{Response, ResponseEvent, StopDetails, StopReason, TokenUsage, Wire};
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::PromptParts;
use crate::protocol::{Event, EventSink, Op};
use crate::session::handle::{RecordOutcome, SessionHandle};
use crate::tool::tasks::Task;
use crate::tool::{ToolDefinition, ToolMetadata, ToolRegistry};

// ── Agent Turn ──

/// What one turn leaves for the engine: the usage its requests reported and the task list its
/// tool calls set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TurnReport {
    /// The last round's usage, whose prompt size the auto-compaction threshold checks.
    pub(super) usage: Option<TokenUsage>,
    /// Every request's usage in order, including one that failed or was cancelled partway. Some
    /// models pick their price tier from a single request's prompt size, so cost is per entry.
    pub(super) billable_rounds: Vec<TokenUsage>,
    /// The task list the turn's last `tasks` call left, `None` when no call set one.
    pub(super) tasks: Option<Vec<Task>>,
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
pub(super) struct TurnOutcome {
    pub(super) report: TurnReport,
    pub(super) result: AbortResult<()>,
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

    /// The report of a completed turn, or the abort that stopped it.
    #[cfg(test)]
    fn into_result(self) -> AbortResult<TurnReport> {
        self.result.map(|()| self.report)
    }
}

/// Drives one user prompt to a final assistant text reply. The loop returns as soon as a round
/// produces no tool calls, and a `pause_turn` stop re-requests without a new user message.
/// [`Op::Queue`]s splice in as user messages at round boundaries, and an [`Op::SetMode`] waits in
/// `inbox` for the next gated call. Long-running awaits race `ops` so `Interrupt` / `Shutdown`
/// abort promptly. An abort mid-stream commits none of the reply, while
/// an abort mid-round commits the round with the unfinished calls marked interrupted. Either way
/// the transcript then ends on [`Message::interruption`].
/// `max_tool_rounds = None` runs unbounded, and `Some(n)` bails after `n` rounds without a reply.
#[expect(
    clippy::too_many_arguments,
    reason = "agent_turn takes the full per-turn dependency set, \
              since a bundle struct would obscure lifetimes"
)]
pub(super) async fn agent_turn(
    client: &dyn Wire,
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    prompt: &PromptParts,
    sink: &dyn EventSink,
    session: &SessionHandle,
    ops: &mut mpsc::Receiver<Op>,
    inbox: &mut Inbox,
    max_tool_rounds: Option<u32>,
    dispatch_ctx: &mut DispatchContext<'_>,
) -> TurnOutcome {
    let tool_defs = tools.definitions();
    let mut report = TurnReport::default();

    for _ in 0..max_tool_rounds.unwrap_or(u32::MAX) {
        let mut round_usage = None;
        let stream = await_unless_aborted(
            stream_response(client, messages, &tool_defs, prompt, sink, &mut round_usage),
            ops,
            inbox,
            sink,
        )
        .await;
        report.record(round_usage);
        let Response {
            content: blocks,
            parse_errors,
            stop_reason,
            stop_details,
        } = match stream {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return TurnOutcome::aborted(report, TurnAbort::Failed(error)),
            Err(abort) => return interrupt_turn(session, sink, messages, report, abort).await,
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

        let tool_uses = dispatch::collect_tool_uses(&blocks);
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

        let round = dispatch::run_tool_round(
            tools,
            tool_uses,
            &parse_errors,
            sink,
            ops,
            inbox,
            dispatch_ctx,
        )
        .await;
        if round.tasks.is_some() {
            report.tasks = round.tasks;
        }
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
            return interrupt_turn(session, sink, messages, report, abort).await;
        }
        record_drained_prompts(inbox.prompts.drain(..), messages, session, sink).await;
    }

    // `None` resolves to `u32::MAX`, so this branch is reachable only when the caller set
    // `Some(n)` and the model ran `n` rounds without a final reply.
    let cap = max_tool_rounds.unwrap_or(u32::MAX);
    TurnOutcome::aborted(
        report,
        TurnAbort::Failed(anyhow!(
            "Agent stopped after {cap} tool rounds without a final response. \
             Raise or unset `client.max_tool_rounds` (`OX_MAX_TOOL_ROUNDS`) to allow longer turns."
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
        Some(StopReason::Refusal) => StopAction::Finish(Err(TurnAbort::Failed(anyhow!(
            refusal_message(stop_details)
        )))),
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

/// Ends a turn on a user abort, recording [`Message::interruption`] after whatever the turn
/// committed so later requests leave the abandoned prompts unanswered.
async fn interrupt_turn(
    session: &SessionHandle,
    sink: &dyn EventSink,
    messages: &mut Vec<Message>,
    report: TurnReport,
    abort: TurnAbort,
) -> TurnOutcome {
    let marker = Message::interruption();
    record_message(session, marker.clone(), sink).await;
    messages.push(marker);
    TurnOutcome::aborted(report, abort)
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
        "The model declined this request{category}{explanation} \
         Rephrase it, or run `/clear` to start over."
    )
}

/// Error for a reply cut off by the output cap or the context window, naming the tool call it cut.
fn truncation_error(reason: StopReason, blocks: &[ContentBlock]) -> anyhow::Error {
    let (cause, remedy) = if reason == StopReason::ModelContextWindowExceeded {
        (
            "The conversation filled the model's context window",
            "Run `/compact`, then continue.",
        )
    } else {
        (
            "The response hit the `max_tokens` output limit",
            "Ask for smaller steps, or raise `client.max_tokens`.",
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

/// Tells the user to send a message when `messages` ends on a response that stopped early, which
/// only a new prompt resumes.
pub(super) fn notify_if_interrupted(messages: &[Message], sink: &dyn EventSink) {
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
pub(super) const INTERRUPTED_WORK_SENTINEL: &str =
    "This part of the response was interrupted before it finished.";

fn ends_on_interrupted_work(blocks: &[ContentBlock]) -> bool {
    matches!(
        blocks.last(),
        Some(ContentBlock::Thinking { thinking, .. }) if thinking == INTERRUPTED_WORK_SENTINEL
    )
}

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

pub(super) async fn record_drained_prompts(
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

async fn record_message(session: &SessionHandle, msg: Message, sink: &dyn EventSink) {
    let outcome: RecordOutcome = session.record_message(msg).await;
    sink.session_write_error(outcome.failure.as_deref());
}

// ── Stream Processing ──

/// Streams one conversation request and collects its reply with [`collect_response`].
async fn stream_response(
    client: &dyn Wire,
    messages: &[Message],
    tools: &[ToolDefinition],
    prompt: &PromptParts,
    sink: &dyn EventSink,
    usage: &mut Option<TokenUsage>,
) -> Result<Response> {
    let section_refs: Vec<&str> = prompt.system_sections.iter().map(String::as_str).collect();
    let rx = client.stream_message(messages, &section_refs, Some(&prompt.user_context), tools)?;
    collect_response(rx, sink, usage).await
}

/// Forwards one response stream's deltas to `sink` and returns its completed reply. `usage`
/// tracks what the wire has reported so far, so it stays accurate when the stream fails or the
/// caller drops the future.
pub(super) async fn collect_response(
    mut rx: mpsc::Receiver<Result<ResponseEvent>>,
    sink: &dyn EventSink,
    usage: &mut Option<TokenUsage>,
) -> Result<Response> {
    while let Some(event) = rx.recv().await {
        match event? {
            ResponseEvent::Text(text) => _ = sink.send(Event::StreamToken(text)),
            ResponseEvent::Thinking(text) => _ = sink.send(Event::ThinkingToken(text)),
            ResponseEvent::Progress(note) => _ = sink.send(Event::ProgressUpdate(note)),
            ResponseEvent::Usage(reported) => *usage = Some(reported),
            ResponseEvent::RateLimits(limits) => {
                sink.emit(Event::RateLimitsUpdated(limits), "rate-limits");
            }
            ResponseEvent::Retrying(notice) => sink.emit(Event::Retrying(notice), "retry-notice"),
            ResponseEvent::Completed(response) => return Ok(response),
        }
    }
    bail!("API stream ended before the reply completed")
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;

    use indoc::indoc;
    use serde_json::json;
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::testing::test_client;
    use crate::client::{RateLimitWindow, RateLimits};
    use crate::config::Effort;
    use crate::config::auth::{Auth, AuthSource};
    use crate::engine::dispatch::NOT_RUN;
    use crate::engine::testing::{
        EchoTool, FakeClient, MODEL, dead_test_session, dispatch_with, empty_prompt, inert_ops,
        reply_of, reply_turn, test_session, text_block, text_turn, thinking_block, with_stop,
    };
    use crate::message::{Replay, Role};
    use crate::model::ResolvedModelId;
    use crate::protocol::{CapturingSink, Decision, ResumeTarget, ToolCall, ToolResult};
    use crate::tool::{
        DEFAULT_TOOL_ICON, Prepared, ReadExcerptLine, Tool, ToolCtx, ToolMetadata, ToolOutput,
        ToolRegistry, ToolResultView, ToolSpec,
    };

    // ── Fixtures ──

    fn tool_use_block(id: &str, name: &str, input_json: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input: serde_json::from_str(input_json).expect("fixture tool input is JSON"),
        }
    }

    fn tool_use_turn(id: &str, name: &str, input_json: &str) -> Vec<ResponseEvent> {
        reply_turn(vec![tool_use_block(id, name, input_json)])
    }

    /// One turn holding a `tool_use` block per `(id, name, input_json)`.
    fn tool_uses_turn(calls: &[(&str, &str, &str)]) -> Vec<ResponseEvent> {
        reply_turn(
            calls
                .iter()
                .map(|(id, name, input_json)| tool_use_block(id, name, input_json))
                .collect(),
        )
    }

    /// Reports `input_tokens` and `output_tokens` just before the turn's reply completes.
    fn with_usage(
        mut events: Vec<ResponseEvent>,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Vec<ResponseEvent> {
        reply_of(&mut events);
        let usage = TokenUsage::new(input_tokens, output_tokens);
        events.insert(events.len() - 1, ResponseEvent::Usage(usage));
        events
    }

    /// Path tool whose result view shows the input and metadata it was resolved from.
    struct PathViewTool;

    impl Tool for PathViewTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "path_view".to_owned(),
                description: "view a path".to_owned(),
                input_schema: json!({"type": "object"}),
                class: crate::permission::PermissionClass::Read,
                parallel_safe: true,
            }
        }

        fn path_field(&self) -> Option<&'static str> {
            Some("file_path")
        }

        fn run<'a>(
            &'a self,
            prepared: Prepared,
            _ctx: &'a ToolCtx,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
            Box::pin(async move {
                let input = prepared.input;
                if input.get("fail").is_some() {
                    return ToolOutput::error("cannot view notes");
                }
                ToolOutput::success(input["file_path"].as_str().unwrap_or_default())
                    .with_title("Viewed notes")
            })
        }

        fn result_view(
            &self,
            input: &serde_json::Value,
            _content: &str,
            metadata: &ToolMetadata,
            _cwd: &Path,
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

    /// Streams one text token, then holds the stream open until the turn drops it.
    struct StalledClient(&'static str);

    impl Wire for StalledClient {
        fn stream_message(
            &self,
            _messages: &[Message],
            _system_sections: &[&str],
            _user_context: Option<&str>,
            _tools: &[ToolDefinition],
        ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
            let (tx, rx) = mpsc::channel(1);
            tx.try_send(Ok(ResponseEvent::Text(self.0.to_owned())))
                .expect("channel capacity");
            tokio::spawn(async move { tx.closed().await });
            Ok(rx)
        }
    }

    /// Signals on entry then blocks forever, so cancel tests can synchronize on the agent being
    /// parked inside the tool future before sending the interrupt.
    struct GateTool {
        started: Arc<Notify>,
    }

    impl Tool for GateTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "gate".to_owned(),
                description: "blocks until the turn is cancelled".to_owned(),
                input_schema: json!({"type": "object"}),
                class: crate::permission::PermissionClass::Exec,
                parallel_safe: false,
            }
        }

        fn run<'a>(
            &'a self,
            _prepared: Prepared,
            _ctx: &'a ToolCtx,
        ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
            let started = self.started.clone();
            Box::pin(async move {
                started.notify_one();
                std::future::pending::<ToolOutput>().await
            })
        }
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

    fn round_totals(report: &TurnReport) -> Vec<u32> {
        report
            .billable_rounds
            .iter()
            .map(|usage| usage.total_tokens())
            .collect()
    }

    /// Dispatch context in `cwd` whose gate runs every call that no deny or ask rule stops
    /// (`yolo`), so the turn tests exercise the loop without approvals. The `dispatch` and
    /// `permission` tests cover the gate itself.
    fn yolo_in(cwd: &Path) -> DispatchContext<'static> {
        dispatch_with(crate::permission::Mode::Yolo, &[], &[], cwd)
    }

    /// [`yolo_in`] for a turn whose tools touch no files.
    fn yolo_dispatch() -> DispatchContext<'static> {
        yolo_in(Path::new("/repo"))
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
                        decision: self.decision.clone(),
                    })
                    .expect("user channel has room");
            }
            self.events.send(event)
        }
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
    async fn agent_turn_reports_latest_stream_usage() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let input_only = with_usage(text_turn("Hello!"), 100, 0);
        let client = FakeClient::new(vec![with_usage(input_only, 100, 7)]);
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .unwrap();

        assert_eq!(report.usage.map(TokenUsage::total_tokens), Some(107));
        assert_eq!(round_totals(&report), [107]);
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
        let cwd = tempfile::tempdir().unwrap();
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
            &mut Inbox::default(),
            None,
            &mut yolo_in(cwd.path()),
        )
        .await
        .into_result()
        .unwrap();

        let pinned = dunce::canonicalize(cwd.path()).unwrap().join("notes.txt");
        let pinned = pinned.to_str().unwrap();
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
            with_usage(tool_use_turn("tool_1", "echo", r#"{"v":1}"#), 9, 2),
            with_usage(text_turn("Done"), 1, 2),
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
        tx.send(Op::Queue("follow up".to_owned())).await.unwrap();

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
        tx.send(Op::Queue("first".to_owned())).await.unwrap();
        tx.send(Op::Queue("second".to_owned())).await.unwrap();

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
    async fn agent_turn_interrupt_before_any_output_ends_on_the_interruption() {
        // Biased select picks the queued Cancel before the stream future, so the turn commits only
        // the interruption and the abort is Cancelled.
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect_err("cancel must surface as Err(Cancelled)");

        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        assert_eq!(messages, [Message::user("hi"), Message::interruption()]);
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_interrupt_mid_stream_discards_the_partial_reply() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = StalledClient("Once upon a time");
        let tools = ToolRegistry::new(vec![]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("write a story")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        let prompt = empty_prompt();
        let mut dispatch_ctx = yolo_dispatch();
        let mut inbox = Inbox::default();

        let (outcome, ()) = tokio::join!(
            agent_turn(
                &client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut rx,
                &mut inbox,
                None,
                &mut dispatch_ctx,
            ),
            async {
                while sink.events().is_empty() {
                    tokio::task::yield_now().await;
                }
                tx.send(Op::Interrupt).await.unwrap();
            },
        );

        let abort = outcome
            .into_result()
            .expect_err("cancel must surface as Err(Cancelled)");
        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        assert_eq!(
            sink.events(),
            [Event::StreamToken("Once upon a time".to_owned())]
        );
        assert_eq!(
            messages,
            [Message::user("write a story"), Message::interruption()]
        );
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect_err("shutdown must surface as Err(Shutdown)");

        assert!(matches!(abort, TurnAbort::Shutdown), "got {abort:?}");
        assert_eq!(messages, [Message::user("hi"), Message::interruption()]);
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
                target: ResumeTarget::Path("/repo/other.jsonl".into()),
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
                &mut Inbox::default(),
                None,
                &mut yolo_dispatch(),
            )
            .await;
            assert!(outcome.result.is_ok(), "turn must complete despite {op:?}");

            assert_eq!(messages.len(), 2, "assistant message recorded for {op:?}");
            drop(tx);
        }
    }

    #[tokio::test]
    async fn agent_turn_interrupt_during_tool_round_commits_finished_and_interrupted_results() {
        // Waits for `GateTool`'s started signal, so Cancel arrives while the tool future runs.
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
        let mut dispatch_ctx = yolo_dispatch();
        let mut inbox = Inbox::default();

        let (turn_result, ()) = tokio::join!(
            agent_turn(
                &client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut rx,
                &mut inbox,
                None,
                &mut dispatch_ctx,
            ),
            async {
                started.notified().await;
                tx.send(Op::Interrupt).await.unwrap();
            },
        );

        let abort = turn_result
            .into_result()
            .expect_err("cancel must surface as Err(Cancelled)");
        assert!(matches!(abort, TurnAbort::Cancelled), "got {abort:?}");
        let [prompt, assistant, results, interruption] = &messages[..] else {
            panic!("the round commits, then the interruption: {messages:#?}");
        };
        assert_eq!(prompt, &Message::user("kick off"));
        assert_eq!(
            assistant.content,
            [
                tool_use_block("tool_1", "echo", r#"{"v":1}"#),
                tool_use_block("tool_2", "gate", "{}"),
                tool_use_block("tool_3", "echo", "{}"),
            ]
        );
        assert_eq!(interruption, &Message::interruption());
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
                    "The user interrupted this call before it finished, \
                     so it may have run partially.",
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
        let client = FakeClient::new(vec![with_usage(
            tool_use_turn("tool_1", "gate", "{}"),
            7,
            3,
        )]);
        let started = Arc::new(Notify::new());
        let tools = ToolRegistry::new(vec![Box::new(GateTool {
            started: started.clone(),
        })]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("kick off")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        let prompt = empty_prompt();
        let mut dispatch_ctx = yolo_dispatch();
        let mut inbox = Inbox::default();

        let (outcome, ()) = tokio::join!(
            agent_turn(
                &client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut rx,
                &mut inbox,
                None,
                &mut dispatch_ctx,
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
            tool_use_turn("tool_1", "nonexistent", "{}"),
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
        // A call whose streamed input failed to parse must not run with the `{}` fallback. The
        // agent answers it with an `is_error: true` result naming the failure to self-correct on.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let mut malformed = tool_use_turn("tool_1", "echo", "{}");
        reply_of(&mut malformed).parse_errors.insert(
            "tool_1".to_owned(),
            "EOF while parsing an object".to_owned(),
        );
        let client = FakeClient::new(vec![malformed, text_turn("recovered")]);
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .unwrap();

        // The ToolUse keeps the empty-object fallback, so the assistant message stays well-formed.
        assert!(matches!(
            &messages[1].content[0],
            ContentBlock::ToolUse { id, input, .. }
                if id == "tool_1" && *input == json!({}),
        ));

        // The tool never runs, so the result is the synthetic parse error.
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
    async fn agent_turn_with_some_cap_bails_naming_the_setting() {
        const CAP: u32 = 3;
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let turns: Vec<Vec<ResponseEvent>> = (0..CAP)
            .map(|i| tool_use_turn(&format!("tool_{i}"), "echo", "{}"))
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
            &mut Inbox::default(),
            Some(CAP),
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect_err("cap must trip");
        assert_eq!(
            format!("{err:#}"),
            "Agent stopped after 3 tool rounds without a final response. \
             Raise or unset `client.max_tool_rounds` (`OX_MAX_TOOL_ROUNDS`) to allow longer turns.",
        );
    }

    #[tokio::test]
    async fn agent_turn_safety_cap_bail_preserves_completed_round_usage() {
        // Each capped round reports usage. The cap-trip abort must still fold every round into
        // billable_rounds so the caller bills what the API charged for.
        const CAP: u32 = 2;
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let turns: Vec<Vec<ResponseEvent>> = (0..CAP)
            .map(|i| with_usage(tool_use_turn(&format!("tool_{i}"), "echo", "{}"), 5, 2))
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
            &mut Inbox::default(),
            Some(CAP),
            &mut yolo_dispatch(),
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
        let mut turns: Vec<Vec<ResponseEvent>> = (0..50)
            .map(|i| tool_use_turn(&format!("tool_{i}"), "echo", "{}"))
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect("unbounded loop must reach the text-only round");
        let last = messages.last().expect("assistant reply recorded");
        assert!(matches!(last.role, Role::Assistant), "{last:?}");
    }

    #[tokio::test]
    async fn agent_turn_stream_error_fails_the_turn_with_its_cause() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(Vec::new()).then_fail("API error (overloaded_error): Busy");
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect_err("api error must propagate");
        assert_eq!(format!("{err:#}"), "API error (overloaded_error): Busy");
    }

    #[tokio::test]
    async fn agent_turn_failed_after_completed_round_preserves_billable_rounds() {
        // Round 1 completes with reported usage. Round 2's stream fails, so the turn bails with
        // TurnAbort::Failed. The completed round's usage must still ride out on billable_rounds
        // so the caller bills what the API charged for.
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![with_usage(
            tool_use_turn("tool_1", "echo", "{}"),
            9,
            4,
        )])
        .then_fail("API error (overloaded_error): Busy");
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
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
    async fn agent_turn_keeps_interrupted_work_note_when_tool_results_follow() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            reply_turn(vec![
                tool_use_block("t1", "echo", "{}"),
                thinking_block(INTERRUPTED_WORK_SENTINEL),
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
        let client = FakeClient::new(vec![reply_turn(vec![thinking_block(
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
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
            reply_turn(vec![
                text_block("Writing it now."),
                tool_use_block("tool_1", "echo", "{}"),
            ]),
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect_err("a truncated reply fails the turn");

        assert_eq!(
            abort.to_string(),
            "The response hit the `max_tokens` output limit while writing the `echo` call, \
             so no tools ran. \
             Ask for smaller steps, or raise `client.max_tokens`.",
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
            reply_turn(Vec::new()),
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .unwrap();

        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    #[tokio::test]
    async fn agent_turn_stream_closed_before_the_reply_fails_without_committing() {
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await
        .into_result()
        .expect_err("a cut stream fails the turn");

        assert_eq!(
            abort.to_string(),
            "API stream ended before the reply completed"
        );
        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    const HELLO_SSE: &str = indoc! {r#"
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

    /// Drives one turn through the real `<Client as Wire>` path. The server answers with
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
            &mut Inbox::default(),
            None,
            &mut yolo_dispatch(),
        )
        .await;
        (outcome, messages, sink)
    }

    #[tokio::test]
    async fn agent_turn_drives_real_client_over_wiremock() {
        let (outcome, messages, _) =
            real_client_turn(vec![sse(HELLO_SSE)], &ToolRegistry::new(vec![])).await;

        outcome.into_result().unwrap();
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

        outcome.into_result().expect("the third attempt succeeds");
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
                "HTTP 529. Retrying in 1 s (retry 1 of 5).",
                "HTTP 503 Service Unavailable. Retrying in 1 s (retry 2 of 5).",
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
        let failed = indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"cache_creation_input_tokens":900,"output_tokens":0}}}

            event: error
            data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}

        "#};
        let answered = indoc! {r#"
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

        assert_eq!(
            outcome.report.usage,
            Some(TokenUsage {
                input_tokens: 5,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 900,
                output_tokens: 7,
            }),
            "the failed attempt's cache write is not billed again",
        );
    }

    #[tokio::test]
    async fn agent_turn_real_client_refusal_discards_the_reply_and_names_the_policy() {
        let body = indoc! {r#"
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

        let abort = outcome.into_result().expect_err("a refusal fails the turn");
        assert_eq!(
            abort.to_string(),
            "The model declined this request (category: cyber): Exploit development. \
             Rephrase it, or run `/clear` to start over.",
        );
        assert_eq!(
            messages.len(),
            1,
            "the refused reply is dropped: {messages:?}"
        );
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, Event::ToolCallStart(_) | Event::Error(_))),
            "refused tool calls never run, and the failure is the only notice",
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
            &mut Inbox::default(),
            None,
            &mut dispatch_with(
                crate::permission::Mode::Manual,
                &["*"],
                &[],
                Path::new("/repo"),
            ),
        )
        .await
        .into_result()
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
            content.starts_with("The echo call was denied by the `deny.exec` pattern `*`."),
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
    async fn agent_turn_mode_switch_arriving_mid_turn_gates_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        let client = FakeClient::new(vec![
            tool_use_turn("tool_1", "echo", r#"{"v":42}"#),
            text_turn("Done"),
        ]);
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut messages = vec![Message::user("run echo")];
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::SetMode(crate::permission::Mode::Plan))
            .unwrap();
        let mut inbox = Inbox::default();
        let mut dispatch_ctx = yolo_dispatch();

        let outcome = agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut rx,
            &mut inbox,
            None,
            &mut dispatch_ctx,
        )
        .await;

        assert!(outcome.result.is_ok());
        assert_eq!(dispatch_ctx.policy.mode(), crate::permission::Mode::Plan);
        assert_eq!(inbox.mode, None, "the gate took the switch");
        let ContentBlock::ToolResult { content, .. } = &messages[2].content[0] else {
            panic!("expected ToolResult, got {:?}", messages[2].content[0]);
        };
        assert!(
            content.starts_with("The echo call was denied by `plan` mode."),
            "{content}"
        );
        assert!(
            matches!(
                sink.events().first(),
                Some(Event::ModeChanged(crate::permission::Mode::Plan))
            ),
            "the switch confirms on receipt: {:?}",
            sink.events(),
        );
        drop(tx);
    }

    #[tokio::test]
    async fn agent_turn_approval_decision_arriving_mid_turn_settles_the_call() {
        // An `ask` blocks the round on `ops` until the reply lands, and the reply alone decides
        // whether the tool runs, what the model reads in its place, or whether the turn ends.
        let feedback = Decision::DenyWithFeedback {
            feedback: "Use ls instead.".to_owned(),
        };
        for (decision, result, aborted) in [
            (Decision::Approve, r#"{"v":42}"#, false),
            (
                Decision::Deny,
                "The echo call was denied by the user.",
                false,
            ),
            (
                feedback,
                "The echo call was denied with this feedback: Use ls instead.",
                false,
            ),
            (Decision::Abort, NOT_RUN, true),
        ] {
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
                decision: decision.clone(),
            };
            let mut messages = vec![Message::user("run echo")];

            let outcome = agent_turn(
                &client,
                &tools,
                &mut messages,
                &empty_prompt(),
                &sink,
                &session,
                &mut ops,
                &mut Inbox::default(),
                None,
                &mut dispatch_with(
                    crate::permission::Mode::Manual,
                    &[],
                    &[],
                    Path::new("/repo"),
                ),
            )
            .await;

            if aborted {
                assert!(
                    matches!(outcome.result, Err(TurnAbort::Cancelled)),
                    "{decision:?}: {:?}",
                    outcome.result,
                );
            } else {
                assert!(outcome.result.is_ok(), "{decision:?}: {:?}", outcome.result);
            }
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
            assert!(content.starts_with(result), "{decision:?}: {content}");
            assert_eq!(*is_error, decision != Decision::Approve, "{decision:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_turn_write_through_a_dangling_symlink_is_never_auto_allowed() {
        // A dangling `notes.md -> ../escape.txt` must resolve to its outside target, so the write
        // never passes as a new in-cwd file. The gate asks, and a denial keeps it from running.
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
        let tools = ToolRegistry::new(vec![Box::new(crate::tool::write::WriteTool)]);
        let mut dispatch_ctx = dispatch_with(crate::permission::Mode::Manual, &[], &[], &cwd);
        let mut messages = vec![Message::user("take notes")];
        let (op_tx, mut ops) = mpsc::channel(4);
        let sink = ReplyingSink {
            events: CapturingSink::new(),
            op_tx,
            decision: Decision::Deny,
        };

        agent_turn(
            &client,
            &tools,
            &mut messages,
            &empty_prompt(),
            &sink,
            &session,
            &mut ops,
            &mut Inbox::default(),
            None,
            &mut dispatch_ctx,
        )
        .await
        .into_result()
        .unwrap();

        assert!(
            sink.events
                .events()
                .iter()
                .any(|e| matches!(e, Event::ApprovalRequested { id, .. } if id == "tool_1")),
            "the write must ask",
        );

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
            content.starts_with("The write call was denied by the user."),
            "{content}"
        );
    }

    // ── refusal_message ──

    #[test]
    fn refusal_message_without_details_still_offers_a_way_forward() {
        assert_eq!(
            refusal_message(None),
            "The model declined this request. Rephrase it, or run `/clear` to start over.",
        );
        let explanation_only = StopDetails {
            category: None,
            explanation: Some(" Unsafe request ".to_owned()),
        };
        assert_eq!(
            refusal_message(Some(&explanation_only)),
            "The model declined this request: Unsafe request. \
             Rephrase it, or run `/clear` to start over.",
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
            "The conversation filled the model's context window before it finished. \
             Run `/compact`, then continue.",
        );
        assert_eq!(
            truncation_error(StopReason::MaxTokens, &[tool_use, text()]).to_string(),
            "The response hit the `max_tokens` output limit before it finished, so no tools ran. \
             Ask for smaller steps, or raise `client.max_tokens`.",
        );
    }

    // ── notify_if_interrupted ──

    #[test]
    fn notify_if_interrupted_only_fires_on_a_final_interrupted_response() {
        let interrupted = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: INTERRUPTED_WORK_SENTINEL.to_owned(),
                replay: Replay::with_model(MODEL, "sig"),
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
            ["Error(\"The response was interrupted before it finished. \
              Send a message to continue.\")"],
        );
        let none: [String; 0] = [];
        assert_eq!(events(&[]), none);
        assert_eq!(events(&[interrupted, Message::user("go on")]), none);
        assert_eq!(
            events(&[Message::user("hi"), Message::assistant("done")]),
            none
        );
    }

    // ── collect_response ──

    #[tokio::test]
    async fn collect_response_forwards_each_delta_and_keeps_the_latest_usage() {
        let reply = || Response {
            content: vec![thinking_block("plan"), text_block("Found it.")],
            stop_reason: Some(StopReason::EndTurn),
            ..Response::default()
        };
        let limits = RateLimits {
            five_hour: Some(RateLimitWindow {
                utilization: 0.42,
                resets_at: 1_760_000_000,
            }),
            seven_day: None,
        };
        let events = [
            ResponseEvent::RateLimits(limits),
            ResponseEvent::Usage(TokenUsage::new(5, 0)),
            ResponseEvent::Retrying("HTTP 529. Retrying in 1 s (retry 1 of 5).".to_owned()),
            ResponseEvent::Thinking("plan".to_owned()),
            ResponseEvent::Progress("Searching.".to_owned()),
            ResponseEvent::Text("Found it.".to_owned()),
            ResponseEvent::Usage(TokenUsage::new(5, 7)),
            ResponseEvent::Completed(reply()),
        ];
        let (tx, rx) = mpsc::channel(events.len());
        for event in events {
            tx.try_send(Ok(event)).unwrap();
        }
        let sink = CapturingSink::new();
        let mut usage = None;

        let response = collect_response(rx, &sink, &mut usage).await.unwrap();

        assert_eq!(response, reply());
        assert_eq!(usage, Some(TokenUsage::new(5, 7)));
        assert_eq!(
            sink.events(),
            [
                Event::RateLimitsUpdated(limits),
                Event::Retrying("HTTP 529. Retrying in 1 s (retry 1 of 5).".to_owned()),
                Event::ThinkingToken("plan".to_owned()),
                Event::ProgressUpdate("Searching.".to_owned()),
                Event::StreamToken("Found it.".to_owned()),
            ],
        );
    }
}
