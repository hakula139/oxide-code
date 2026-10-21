//! One round's tool calls: the permission gate, approvals, and each call's result.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::abort::{AbortResult, Inbox, TurnAbort, await_unless_aborted};
use crate::file_tracker::FileTracker;
use crate::message::ContentBlock;
use crate::permission::rule::Rule;
use crate::permission::{Policy, SessionGrant, Workspace};
use crate::protocol::{Decision, Event, EventSink, Op, ToolCall, ToolResult};
use crate::tool::spill::run_capped;
use crate::tool::tasks::Task;
use crate::tool::{Prepared, Tool, ToolCtx, ToolMetadata, ToolOutput, ToolRegistry, ToolSpec};

/// What each call of a round runs against: the policy that gates it, which gains the session allow
/// rules the user grants, and the session-wide parts of its [`ToolCtx`].
pub(super) struct DispatchContext<'a> {
    pub(super) policy: &'a mut Policy,
    pub(super) workspace: Arc<Workspace>,
    pub(super) session_id: String,
    pub(super) artifact_dir: Arc<Path>,
    pub(super) tracker: Arc<FileTracker>,
}

impl DispatchContext<'_> {
    /// The context the call `tool_use_id` prepares and runs in.
    fn tool_ctx(&self, tool_use_id: &str) -> ToolCtx {
        ToolCtx {
            workspace: Arc::clone(&self.workspace),
            session_id: self.session_id.clone(),
            artifact_dir: Arc::clone(&self.artifact_dir),
            tracker: Arc::clone(&self.tracker),
            tool_use_id: tool_use_id.to_owned(),
            cancel: CancellationToken::new(),
        }
    }
}

/// A tool call a reply made, which the round has yet to run.
pub(super) struct PendingToolUse {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// The tool calls in `blocks`, in the order the reply made them.
pub(super) fn collect_tool_uses(blocks: &[ContentBlock]) -> Vec<PendingToolUse> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(PendingToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// One tool round's results, with a result for every call even when a user abort stopped dispatch.
pub(super) struct ToolRound {
    pub(super) results: Vec<ContentBlock>,
    pub(super) sidecars: Vec<(String, ToolMetadata)>,
    /// The task list the round's last `tasks` call left, if any call set one.
    pub(super) tasks: Option<Vec<Task>>,
    pub(super) abort: Option<TurnAbort>,
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

/// Runs a round's calls in order. Consecutive parallel-safe calls form one batch, whose calls each
/// pass the gate in turn before the allowed ones run together, so an approval never waits on a
/// running call. Any other call is a batch of its own.
pub(super) async fn run_tool_round(
    tools: &ToolRegistry,
    tool_uses: Vec<PendingToolUse>,
    parse_errors: &HashMap<String, String>,
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
    inbox: &mut Inbox,
    dispatch_ctx: &mut DispatchContext<'_>,
) -> ToolRound {
    let mut round = ToolRound {
        results: Vec::with_capacity(tool_uses.len()),
        sidecars: Vec::with_capacity(tool_uses.len()),
        tasks: None,
        abort: None,
    };
    let mut dispatcher = Dispatcher {
        tools,
        parse_errors,
        sink,
        ops,
        inbox,
        dispatch_ctx,
    };
    let parallel_safe = |name: &str| tools.get(name).is_some_and(|(spec, _)| spec.parallel_safe);
    let mut calls = tool_uses.into_iter().peekable();
    while let Some(first) = calls.next() {
        let mut batch = vec![first];
        if parallel_safe(&batch[0].name) {
            batch.extend(std::iter::from_fn(|| {
                calls.next_if(|call| parallel_safe(&call.name))
            }));
        }
        if let Some(abort) = dispatcher.run_batch(batch, &mut round).await {
            round.abort = Some(abort);
            break;
        }
    }
    for call in calls {
        round.push(call.id, ToolOutput::error(NOT_RUN));
    }
    round
}

/// Result of a call the user interrupted before its tool started.
pub(super) const NOT_RUN: &str = "Not run, because the user interrupted the turn.";

/// Result of a call the user interrupted while its tool ran.
const INTERRUPTED: &str =
    "The user interrupted this call before it finished, so it may have run partially.";

/// The state one round's calls share.
struct Dispatcher<'r, 'd> {
    tools: &'r ToolRegistry,
    parse_errors: &'r HashMap<String, String>,
    sink: &'r dyn EventSink,
    ops: &'r mut mpsc::Receiver<Op>,
    inbox: &'r mut Inbox,
    dispatch_ctx: &'r mut DispatchContext<'d>,
}

/// A call of a batch that passed the gate, and the slot its output fills.
struct Ready<'r> {
    slot: usize,
    tool: &'r dyn Tool,
    prepared: Prepared,
    ctx: ToolCtx,
}

/// A call of a batch the round started, with its output once it has one.
struct Started {
    id: String,
    name: String,
    input: serde_json::Value,
    output: Option<ToolOutput>,
}

/// How the gate settled one call.
enum Gate<'r> {
    Ready {
        tool: &'r dyn Tool,
        prepared: Prepared,
        ctx: ToolCtx,
    },
    /// Not run, since the input failed to parse, the tool is unknown, or the gate denied the call.
    Settled(ToolOutput),
}

impl<'r> Dispatcher<'r, '_> {
    /// Starts each call of `batch` and gates it, runs the allowed ones together, then ends each
    /// started call with its result. Returns the abort that stopped the batch, once every call in
    /// it has a result.
    async fn run_batch(
        &mut self,
        batch: Vec<PendingToolUse>,
        round: &mut ToolRound,
    ) -> Option<TurnAbort> {
        let workspace = Arc::clone(&self.dispatch_ctx.workspace);
        let cwd = workspace.cwd();
        let mut started = Vec::with_capacity(batch.len());
        let mut ready = Vec::new();
        let mut abort = None;
        let mut calls = batch.into_iter();
        for PendingToolUse { id, name, input } in calls.by_ref() {
            self.sink.emit(
                Event::ToolCallStart(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    icon: self.tools.icon(&name).to_owned(),
                    label: self.tools.label(&name, &input, cwd),
                }),
                "tool-call-start",
            );
            let output = match self.gate(&id, &name, input.clone()).await {
                Ok(Gate::Ready {
                    tool,
                    prepared,
                    ctx,
                }) => {
                    ready.push(Ready {
                        slot: started.len(),
                        tool,
                        prepared,
                        ctx,
                    });
                    None
                }
                Ok(Gate::Settled(output)) => Some(output),
                Err(stop) => {
                    abort = Some(stop);
                    None
                }
            };
            started.push(Started {
                id,
                name,
                input,
                output,
            });
            if abort.is_some() {
                break;
            }
        }

        if abort.is_none() {
            abort = self.run_ready(ready, &mut started).await;
        }
        for call in started {
            let mut output = call.output.unwrap_or_else(|| ToolOutput::error(NOT_RUN));
            self.sink.emit(
                Event::ToolCallEnd(ToolResult {
                    id: call.id.clone(),
                    content: output.content.clone(),
                    is_error: output.is_error,
                    title: output.metadata.title.clone(),
                    view: self.tools.result_view(
                        &call.name,
                        &call.input,
                        &output.content,
                        &output.metadata,
                        output.is_error,
                        cwd,
                    ),
                }),
                "tool-call-end",
            );
            if let Some(tasks) = output.tasks.take() {
                self.sink
                    .emit(Event::TasksUpdated(tasks.clone()), "tasks-updated");
                round.tasks = Some(tasks);
            }
            round.push(call.id, output);
        }
        for call in calls {
            round.push(call.id, ToolOutput::error(NOT_RUN));
        }
        abort
    }

    /// Runs the `ready` calls together, filling each one's slot in `started` as it finishes. On an
    /// interrupt or shutdown it cancels and marks the unfinished ones, then returns the abort.
    async fn run_ready(
        &mut self,
        ready: Vec<Ready<'_>>,
        started: &mut [Started],
    ) -> Option<TurnAbort> {
        let cancels: Vec<CancellationToken> =
            ready.iter().map(|call| call.ctx.cancel.clone()).collect();
        let mut runs: FuturesUnordered<_> = ready
            .into_iter()
            .map(|call| async move {
                let output = run_capped(call.tool, call.prepared, &call.ctx).await;
                (call.slot, output)
            })
            .collect();
        while !runs.is_empty() {
            match await_unless_aborted(runs.select_next_some(), self.ops, self.inbox, self.sink)
                .await
            {
                Ok((slot, output)) => started[slot].output = Some(output),
                Err(abort) => {
                    drop(runs);
                    for cancel in &cancels {
                        cancel.cancel();
                    }
                    for call in started.iter_mut().filter(|call| call.output.is_none()) {
                        call.output = Some(ToolOutput::error(INTERRUPTED));
                    }
                    return Some(abort);
                }
            }
        }
        None
    }

    /// Prepares one call and runs it through the permission gate. Returns the call ready to run,
    /// a synthetic output for one that does not run, or an abort on an interrupt, a shutdown, or a
    /// [`Decision::Abort`] while an approval is outstanding.
    async fn gate(
        &mut self,
        id: &str,
        name: &str,
        input: serde_json::Value,
    ) -> AbortResult<Gate<'r>> {
        if let Some(err) = self.parse_errors.get(id) {
            return Ok(Gate::Settled(ToolOutput::error(format!(
                "Tool input JSON failed to parse: {err}. Retry with a valid JSON object."
            ))));
        }
        let Some((spec, tool)) = self.tools.get(name) else {
            return Ok(Gate::Settled(ToolOutput::error(format!(
                "Unknown tool: {name}"
            ))));
        };

        let ctx = self.dispatch_ctx.tool_ctx(id);
        let prepared = tool.prepare(input, &ctx);
        Ok(
            match self.check_permission(spec, tool, id, &prepared).await? {
                None => Gate::Ready {
                    tool,
                    prepared,
                    ctx,
                },
                Some(denial) => Gate::Settled(denial),
            },
        )
    }

    /// Runs the permission gate on a prepared call, returning `None` to proceed or a synthetic
    /// denial. An `ask` emits [`Event::ApprovalRequested`] and blocks on the matching
    /// [`Op::Resolve`]. It denies at once when no frontend can receive the request, since nothing
    /// would ever resolve it.
    async fn check_permission(
        &mut self,
        spec: &ToolSpec,
        tool: &dyn Tool,
        id: &str,
        prepared: &Prepared,
    ) -> AbortResult<Option<ToolOutput>> {
        let name = spec.name.as_str();
        let class = spec.class;
        if let Some(mode) = self.inbox.mode.take() {
            self.dispatch_ctx.policy.set_mode(mode);
        }
        let verdict = self
            .dispatch_ctx
            .policy
            .verdict(class, &prepared.access.as_target());
        debug!(
            "permission: {name} call {id} settled as {:?} by {}",
            verdict.decision, verdict.basis
        );
        match verdict.decision {
            crate::permission::Decision::Allow => Ok(None),
            crate::permission::Decision::Deny => Ok(Some(denied_output(
                name,
                format_args!("by {}", verdict.basis),
            ))),
            crate::permission::Decision::Ask => {
                let grant = self
                    .dispatch_ctx
                    .policy
                    .session_grant(class, &prepared.access);
                let preview = tool
                    .approval_preview(name, &prepared.input, self.dispatch_ctx.workspace.cwd())
                    .await;
                if self
                    .sink
                    .send(Event::ApprovalRequested {
                        id: id.to_owned(),
                        preview,
                        basis: verdict.basis.clone(),
                        grant: grant.clone().map(Box::new),
                    })
                    .is_err()
                {
                    return Ok(Some(denied_output(
                        name,
                        "because its approval request could not reach the user",
                    )));
                }
                match await_approval(id, self.ops, self.inbox, self.sink).await? {
                    Decision::Approve => Ok(None),
                    Decision::AllowForSession { rule } => {
                        Ok(self.allow_for_session(grant.as_ref(), *rule, name))
                    }
                    Decision::Deny => Ok(Some(denied_output(name, "by the user"))),
                    Decision::DenyWithFeedback { feedback } => Ok(Some(ToolOutput::error(
                        format!("The {name} call was denied with this feedback: {feedback}"),
                    ))),
                    Decision::Abort => Err(TurnAbort::Cancelled),
                }
            }
        }
    }

    /// Adds the frontend's `rule` to the session allow rules once the call's own `grant` admits it,
    /// since the frontend's check is advisory. A refused rule returns a denial.
    fn allow_for_session(
        &mut self,
        grant: Option<&SessionGrant>,
        rule: Rule,
        name: &str,
    ) -> Option<ToolOutput> {
        let admitted = grant
            .context("this call offers no session rule")
            .and_then(|grant| grant.admit(rule));
        match admitted {
            Ok(rule) => {
                self.dispatch_ctx.policy.allow_for_session(rule);
                self.sink.emit(
                    Event::SessionRulesUpdated(self.dispatch_ctx.policy.session_allow().to_vec()),
                    "session-rules-updated",
                );
                None
            }
            Err(e) => {
                self.sink.emit(
                    Event::Error(format!(
                        "Session rule refused, so the {name} call was denied: {e:#}"
                    )),
                    "session-rule-refused",
                );
                Some(denied_output(
                    name,
                    "because the session rule chosen in its approval does not cover it",
                ))
            }
        }
    }
}

/// The synthetic error result for a call denied for `reason`, a phrase that follows "was denied",
/// such as `by the user`.
fn denied_output(name: &str, reason: impl fmt::Display) -> ToolOutput {
    ToolOutput::error(format!(
        "The {name} call was denied {reason}. \
         Do not retry it. \
         Choose a different approach or ask the user to adjust permissions."
    ))
}

/// Blocks on `ops` for the [`Op::Resolve`] matching `id`, while [`Inbox::absorb`] handles the
/// other ops. A decision for a different ID is a stale reply for an already-resolved call, so it
/// is ignored, like any op the gate must not act on mid-approval. Cancel-safe by drop.
async fn await_approval(
    id: &str,
    ops: &mut mpsc::Receiver<Op>,
    inbox: &mut Inbox,
    sink: &dyn EventSink,
) -> AbortResult<Decision> {
    loop {
        match inbox.absorb(ops.recv().await, sink)? {
            Some(Op::Resolve {
                id: target,
                decision,
            }) if target == id => return Ok(decision),
            Some(other) => warn!("ignored op while awaiting approval {id}: {other:?}"),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::pin::Pin;
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::Barrier;

    use super::*;
    use crate::engine::local_testing::{EchoTool, dispatch_with, inert_ops};
    use crate::permission::{Basis, Mode, PermissionClass};
    use crate::protocol::testing::{CapturingSink, FailingSink};
    #[cfg(unix)]
    use crate::tool::read::ReadTool;
    use crate::tool::tasks::TasksTool;
    use crate::tool::tasks::testing::plan;
    use crate::tool::testing::bash_registry;
    #[cfg(unix)]
    use crate::tool::write::WriteTool;

    fn approval_ids(sink: &CapturingSink) -> Vec<String> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::ApprovalRequested { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    mod run_tool_round {
        use super::*;

        fn call(id: &str, name: &str, input: serde_json::Value) -> PendingToolUse {
            PendingToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
            }
        }

        /// Each result of `round` as `(id, content, is_error)`, in order.
        fn results(round: &ToolRound) -> Vec<(&str, &str, bool)> {
            round
                .results
                .iter()
                .map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => (tool_use_id.as_str(), content.as_str(), *is_error),
                    other => panic!("expected a tool result, got {other:?}"),
                })
                .collect()
        }

        fn parallel_spec(name: &str, class: PermissionClass) -> ToolSpec {
            ToolSpec {
                name: name.to_owned(),
                description: format!("the {name} fixture"),
                input_schema: json!({"type": "object"}),
                class,
                parallel_safe: true,
            }
        }

        /// Parallel-safe tool in the `exec` class, so `manual` asks before each of its calls.
        struct AskingTool;

        impl Tool for AskingTool {
            fn spec(&self) -> ToolSpec {
                parallel_spec("probe", PermissionClass::Exec)
            }

            fn run<'a>(
                &'a self,
                _prepared: Prepared,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(async { ToolOutput::success("probed") })
            }
        }

        /// Parallel-safe tool whose call echoes `v` once every call sharing its barrier is running,
        /// so a round that runs them one at a time never finishes.
        struct RendezvousTool(Arc<Barrier>);

        impl Tool for RendezvousTool {
            fn spec(&self) -> ToolSpec {
                parallel_spec("meet", PermissionClass::Read)
            }

            fn run<'a>(
                &'a self,
                prepared: Prepared,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(async move {
                    self.0.wait().await;
                    ToolOutput::success(prepared.input["v"].to_string())
                })
            }
        }

        /// Parallel-safe tool whose call hands its cancel token to the test, then never finishes.
        struct HangTool(mpsc::UnboundedSender<CancellationToken>);

        impl Tool for HangTool {
            fn spec(&self) -> ToolSpec {
                parallel_spec("hang", PermissionClass::Read)
            }

            fn run<'a>(
                &'a self,
                _prepared: Prepared,
                ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(async move {
                    self.0.send(ctx.cancel.clone()).unwrap();
                    std::future::pending().await
                })
            }
        }

        #[tokio::test]
        async fn interrupt_during_approval_reports_every_call_not_run() {
            // The first call passes the gate but never runs, since the interrupt lands while the
            // second awaits approval. The rest of the round never starts.
            let tools = ToolRegistry::new(vec![
                Box::new(RendezvousTool(Arc::new(Barrier::new(1)))),
                Box::new(AskingTool),
                Box::new(EchoTool),
            ]);
            let sink = CapturingSink::default();
            let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
            let (tx, mut rx) = mpsc::channel::<Op>(1);
            tx.try_send(Op::Interrupt).unwrap();
            let calls = vec![
                call("tool_1", "meet", json!({"v": 1})),
                call("tool_2", "probe", json!({})),
                call("tool_3", "probe", json!({})),
                call("tool_4", "echo", json!({})),
            ];

            let round = run_tool_round(
                &tools,
                calls,
                &HashMap::new(),
                &sink,
                &mut rx,
                &mut Inbox::default(),
                &mut dispatch_ctx,
            )
            .await;

            assert!(
                matches!(round.abort, Some(TurnAbort::Cancelled)),
                "got {:?}",
                round.abort,
            );
            assert_eq!(approval_ids(&sink), ["tool_2"]);
            let started: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|e| match e {
                    Event::ToolCallStart(call) => Some(call.id),
                    _ => None,
                })
                .collect();
            assert_eq!(started, ["tool_1", "tool_2"]);
            assert_eq!(
                results(&round),
                [
                    ("tool_1", NOT_RUN, true),
                    ("tool_2", NOT_RUN, true),
                    ("tool_3", NOT_RUN, true),
                    ("tool_4", NOT_RUN, true),
                ],
            );
        }

        #[tokio::test]
        async fn runs_consecutive_parallel_safe_calls_together() {
            // A parse failure in the middle of the batch must not shift the results after it.
            let tools = ToolRegistry::new(vec![
                Box::new(RendezvousTool(Arc::new(Barrier::new(2)))),
                Box::new(EchoTool),
            ]);
            let sink = CapturingSink::default();
            let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &["*"], Path::new("/repo"));
            let calls = vec![
                call("tool_1", "meet", json!({"v": 1})),
                call("tool_2", "meet", json!({})),
                call("tool_3", "meet", json!({"v": 3})),
                call("tool_4", "echo", json!({"v": 4})),
            ];
            let parse_errors = HashMap::from([("tool_2".to_owned(), "EOF".to_owned())]);

            let round = tokio::time::timeout(
                Duration::from_secs(10),
                run_tool_round(
                    &tools,
                    calls,
                    &parse_errors,
                    &sink,
                    &mut inert_ops(),
                    &mut Inbox::default(),
                    &mut dispatch_ctx,
                ),
            )
            .await
            .expect("the meet calls must run together");

            assert!(round.abort.is_none(), "{:?}", round.abort);
            assert_eq!(
                results(&round),
                [
                    ("tool_1", "1", false),
                    (
                        "tool_2",
                        "Tool input JSON failed to parse: EOF. Retry with a valid JSON object.",
                        true,
                    ),
                    ("tool_3", "3", false),
                    ("tool_4", r#"{"v":4}"#, false),
                ],
            );
            let order: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|e| match e {
                    Event::ToolCallStart(call) => Some(format!("start {}", call.id)),
                    Event::ToolCallEnd(result) => Some(format!("end {}", result.id)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                order,
                [
                    "start tool_1",
                    "start tool_2",
                    "start tool_3",
                    "end tool_1",
                    "end tool_2",
                    "end tool_3",
                    "start tool_4",
                    "end tool_4",
                ],
                "the echo call waits for the batch before it",
            );
        }

        #[tokio::test]
        async fn interrupt_while_calls_run_keeps_finished_results_and_cancels_the_rest() {
            let (started_tx, mut started) = mpsc::unbounded_channel();
            let tools = ToolRegistry::new(vec![
                Box::new(RendezvousTool(Arc::new(Barrier::new(1)))),
                Box::new(HangTool(started_tx)),
            ]);
            let sink = CapturingSink::default();
            let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
            let (tx, mut rx) = mpsc::channel::<Op>(1);
            let calls = vec![
                call("tool_1", "meet", json!({"v": 1})),
                call("tool_2", "hang", json!({})),
                call("tool_3", "hang", json!({})),
            ];
            let (parse_errors, mut inbox) = (HashMap::new(), Inbox::default());
            let interrupt = async {
                let tokens = [started.recv().await.unwrap(), started.recv().await.unwrap()];
                assert!(tokens.iter().all(|token| !token.is_cancelled()));
                tx.send(Op::Interrupt).await.unwrap();
                tokens
            };

            let (round, tokens) = tokio::join!(
                run_tool_round(
                    &tools,
                    calls,
                    &parse_errors,
                    &sink,
                    &mut rx,
                    &mut inbox,
                    &mut dispatch_ctx,
                ),
                interrupt,
            );

            assert!(
                matches!(round.abort, Some(TurnAbort::Cancelled)),
                "got {:?}",
                round.abort,
            );
            assert_eq!(
                results(&round),
                [
                    ("tool_1", "1", false),
                    ("tool_2", INTERRUPTED, true),
                    ("tool_3", INTERRUPTED, true),
                ],
            );
            assert!(tokens.iter().all(CancellationToken::is_cancelled));
        }

        #[tokio::test]
        async fn reports_the_list_only_a_successful_tasks_call_sets() {
            let tools = ToolRegistry::new(vec![Box::new(TasksTool)]);
            let sink = CapturingSink::default();
            let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
            let both_in_progress = json!({"tasks": [
                {"content": "Fix the parser", "status": "in_progress"},
                {"content": "Fix the lexer", "status": "in_progress"},
            ]});
            let calls = vec![
                call("call-1", "tasks", json!({"tasks": plan()})),
                call("call-2", "tasks", both_in_progress),
            ];

            let round = run_tool_round(
                &tools,
                calls,
                &HashMap::new(),
                &sink,
                &mut inert_ops(),
                &mut Inbox::default(),
                &mut dispatch_ctx,
            )
            .await;

            assert_eq!(round.tasks, Some(plan()));
            assert!(
                matches!(
                    &round.results[1],
                    ContentBlock::ToolResult { is_error: true, .. }
                ),
                "{:?}",
                round.results,
            );
            let events = sink.events();
            let updates: Vec<usize> = events
                .iter()
                .enumerate()
                .filter(|(_, event)| matches!(event, Event::TasksUpdated(_)))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(updates, [2], "{events:?}");
            assert!(
                matches!(&events[1], Event::ToolCallEnd(result) if result.id == "call-1"),
                "{events:?}",
            );
            assert_eq!(events[2], Event::TasksUpdated(plan()));
        }
    }

    mod dispatcher {
        use super::*;

        mod gate {
            use super::*;

            /// How [`gate_call`] settled a call, without the tool a ready call borrows.
            enum Gated {
                Ready(Prepared),
                Settled(ToolOutput),
            }

            impl Gated {
                fn prepared(self) -> Prepared {
                    match self {
                        Self::Ready(prepared) => prepared,
                        Self::Settled(output) => panic!("the call was settled: {}", output.content),
                    }
                }

                fn output(self) -> ToolOutput {
                    match self {
                        Self::Settled(output) => output,
                        Self::Ready(prepared) => panic!("the call is ready to run: {prepared:?}"),
                    }
                }
            }

            /// Gates the call `id` to `name` the way a round does.
            async fn gate_call(
                tools: &ToolRegistry,
                id: &str,
                name: &str,
                input: serde_json::Value,
                sink: &dyn EventSink,
                ops: &mut mpsc::Receiver<Op>,
                dispatch_ctx: &mut DispatchContext<'_>,
            ) -> AbortResult<Gated> {
                let parse_errors = HashMap::new();
                let mut inbox = Inbox::default();
                let mut dispatcher = Dispatcher {
                    tools,
                    parse_errors: &parse_errors,
                    sink,
                    ops,
                    inbox: &mut inbox,
                    dispatch_ctx,
                };
                Ok(match dispatcher.gate(id, name, input).await? {
                    Gate::Ready { prepared, .. } => Gated::Ready(prepared),
                    Gate::Settled(output) => Gated::Settled(output),
                })
            }

            #[tokio::test]
            async fn allow_proceeds_without_an_approval_event() {
                // `echo` is exec-class, so `manual` asks unless a class-wide allow clears it.
                let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
                let sink = CapturingSink::default();
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &["*"], Path::new("/repo"));

                let gate = gate_call(
                    &tools,
                    "tool_1",
                    "echo",
                    json!({"v": 1}),
                    &sink,
                    &mut inert_ops(),
                    &mut dispatch_ctx,
                )
                .await
                .expect("allow must not abort");

                assert_eq!(gate.prepared().input, json!({"v": 1}));
                assert_eq!(approval_ids(&sink), [] as [&str; 0]);
            }

            #[tokio::test]
            async fn deny_rule_short_circuits_to_a_synthetic_denial() {
                let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
                let sink = CapturingSink::default();
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &["*"], &[], Path::new("/repo"));

                let denial = gate_call(
                    &tools,
                    "tool_1",
                    "echo",
                    json!({}),
                    &sink,
                    &mut inert_ops(),
                    &mut dispatch_ctx,
                )
                .await
                .expect("deny resolves without abort")
                .output();

                assert!(denial.is_error);
                assert_eq!(
                    denial.content,
                    "The echo call was denied by the `deny.exec` pattern `*`. \
                     Do not retry it. \
                     Choose a different approach or ask the user to adjust permissions."
                );
                assert_eq!(
                    approval_ids(&sink),
                    [] as [&str; 0],
                    "a deny rule never prompts"
                );
            }

            #[tokio::test]
            async fn ask_emits_event_and_approve_proceeds() {
                let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
                let sink = CapturingSink::default();
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
                let (tx, mut rx) = mpsc::channel::<Op>(1);
                tx.try_send(Op::Resolve {
                    id: "tool_1".to_owned(),
                    decision: Decision::Approve,
                })
                .unwrap();

                let gate = gate_call(
                    &tools,
                    "tool_1",
                    "echo",
                    json!({}),
                    &sink,
                    &mut rx,
                    &mut dispatch_ctx,
                )
                .await
                .expect("approve resolves without abort");

                assert_eq!(gate.prepared().input, json!({}));
                assert!(
                    matches!(
                        sink.events().as_slice(),
                        [Event::ApprovalRequested {
                            id,
                            basis: Basis::Mode(Mode::Manual),
                            ..
                        }] if id == "tool_1",
                    ),
                    "ask must surface one approval event carrying the call id and its basis: {:?}",
                    sink.events(),
                );
            }

            #[tokio::test]
            async fn allow_for_session_runs_later_calls_the_rule_covers_unasked() {
                let tools = bash_registry(Vec::new());
                let sink = CapturingSink::default();
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
                let rule = Rule::parse(PermissionClass::Exec, "cargo test *").unwrap();
                let (tx, mut rx) = mpsc::channel::<Op>(1);
                tx.try_send(Op::Resolve {
                    id: "tool_1".to_owned(),
                    decision: Decision::AllowForSession {
                        rule: Box::new(rule.clone()),
                    },
                })
                .unwrap();
                drop(tx);

                for (id, command) in [("tool_1", "cargo test"), ("tool_2", "cargo test --release")]
                {
                    let gate = gate_call(
                        &tools,
                        id,
                        "bash",
                        json!({ "command": command }),
                        &sink,
                        &mut rx,
                        &mut dispatch_ctx,
                    )
                    .await
                    .expect("the granted rule settles the second call without waiting");
                    assert_eq!(gate.prepared().input, json!({ "command": command }));
                }

                let events = sink.events();
                let suggested: Vec<String> = events
                    .iter()
                    .filter_map(|e| match e {
                        Event::ApprovalRequested { grant, .. } => Some(
                            grant
                                .as_ref()
                                .expect("a command offers a grant")
                                .suggested()
                                .pattern()
                                .to_owned(),
                        ),
                        _ => None,
                    })
                    .collect();
                assert_eq!(suggested, ["cargo test"], "only the first call asks");
                assert!(
                    events.iter().any(|e| matches!(
                        e,
                        Event::SessionRulesUpdated(rules) if *rules == [rule.clone()]
                    )),
                    "{events:?}",
                );
                assert_eq!(dispatch_ctx.policy.session_allow(), [rule]);
            }

            #[tokio::test]
            async fn allow_for_session_denies_a_rule_the_call_grant_does_not_admit() {
                // The engine re-checks the frontend's rule against its own grant: a rule that
                // misses the call, one in another class, and any rule for a call that offered no
                // grant all deny.
                let tools = bash_registry(vec![Box::new(EchoTool)]);
                for (name, input, (class, pattern)) in [
                    (
                        "bash",
                        json!({ "command": "cargo test" }),
                        (PermissionClass::Exec, "cargo build *"),
                    ),
                    (
                        "bash",
                        json!({ "command": "cargo test" }),
                        (PermissionClass::Edit, "*"),
                    ),
                    ("echo", json!({}), (PermissionClass::Exec, "*")),
                ] {
                    let rule = Rule::parse(class, pattern).unwrap();
                    let sink = CapturingSink::default();
                    let mut dispatch_ctx =
                        dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
                    let (tx, mut rx) = mpsc::channel::<Op>(1);
                    tx.try_send(Op::Resolve {
                        id: "tool_1".to_owned(),
                        decision: Decision::AllowForSession {
                            rule: Box::new(rule),
                        },
                    })
                    .unwrap();

                    let denial = gate_call(
                        &tools,
                        "tool_1",
                        name,
                        input,
                        &sink,
                        &mut rx,
                        &mut dispatch_ctx,
                    )
                    .await
                    .expect("a refused rule resolves without abort")
                    .output();

                    assert_eq!(
                        denial.content,
                        format!(
                            "The {name} call was denied \
                             because the session rule chosen in its approval does not cover it. \
                             Do not retry it. \
                             Choose a different approach or ask the user to adjust permissions."
                        ),
                        "{class} {pattern}"
                    );
                    assert_eq!(dispatch_ctx.policy.session_allow(), [], "{class} {pattern}");
                    let events = sink.events();
                    assert!(
                        events.iter().any(|e| matches!(
                            e,
                            Event::Error(msg) if msg.starts_with(&format!("Session rule refused, so the {name} call was denied: "))
                        )),
                        "{class} {pattern}: {events:?}",
                    );
                    assert!(
                        !events
                            .iter()
                            .any(|e| matches!(e, Event::SessionRulesUpdated(_))),
                        "{class} {pattern}: {events:?}",
                    );
                }
            }

            #[tokio::test]
            async fn ask_undeliverable_event_fails_closed() {
                let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
                let (_tx, mut rx) = mpsc::channel::<Op>(1);

                let denial = gate_call(
                    &tools,
                    "tool_1",
                    "echo",
                    json!({}),
                    &FailingSink,
                    &mut rx,
                    &mut dispatch_ctx,
                )
                .await
                .expect("an undeliverable approval resolves without abort")
                .output();

                assert_eq!(
                    denial.content,
                    "The echo call was denied \
                     because its approval request could not reach the user. \
                     Do not retry it. \
                     Choose a different approach or ask the user to adjust permissions."
                );
            }

            #[tokio::test]
            async fn ask_deny_decision_yields_a_synthetic_denial() {
                let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
                let sink = CapturingSink::default();
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], Path::new("/repo"));
                let (tx, mut rx) = mpsc::channel::<Op>(1);
                tx.try_send(Op::Resolve {
                    id: "tool_1".to_owned(),
                    decision: Decision::Deny,
                })
                .unwrap();

                let denial = gate_call(
                    &tools,
                    "tool_1",
                    "echo",
                    json!({}),
                    &sink,
                    &mut rx,
                    &mut dispatch_ctx,
                )
                .await
                .expect("deny decision resolves without abort")
                .output();

                assert!(denial.is_error);
                assert_eq!(
                    denial.content,
                    "The echo call was denied by the user. \
                     Do not retry it. \
                     Choose a different approach or ask the user to adjust permissions."
                );
            }

            #[tokio::test]
            async fn unknown_tool_settles_without_consulting_the_policy() {
                // An unknown tool has no permission class, so a class-wide deny must not catch it,
                // and the model reads why the call failed.
                let tools = ToolRegistry::new(vec![]);
                let sink = CapturingSink::default();
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &["*"], &[], Path::new("/repo"));

                let output = gate_call(
                    &tools,
                    "tool_1",
                    "mystery",
                    json!({}),
                    &sink,
                    &mut inert_ops(),
                    &mut dispatch_ctx,
                )
                .await
                .expect("unknown tool resolves without abort")
                .output();

                assert_eq!(
                    (output.content.as_str(), output.is_error),
                    ("Unknown tool: mystery", true),
                );
                assert_eq!(approval_ids(&sink), [] as [&str; 0]);
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn pins_a_path_input_to_the_resolved_target() {
                let dir = tempfile::tempdir().unwrap();
                let cwd = dunce::canonicalize(dir.path()).unwrap();
                std::fs::write(cwd.join("real.txt"), "x").unwrap();
                std::os::unix::fs::symlink("real.txt", cwd.join("link.txt")).unwrap();
                let tools = ToolRegistry::new(vec![Box::new(ReadTool)]);
                let mut dispatch_ctx = dispatch_with(Mode::Manual, &[], &[], &cwd);

                let gate = gate_call(
                    &tools,
                    "tool_1",
                    "read",
                    json!({"file_path": "link.txt"}),
                    &CapturingSink::default(),
                    &mut inert_ops(),
                    &mut dispatch_ctx,
                )
                .await
                .expect("a read resolves without abort");

                assert_eq!(
                    gate.prepared().input,
                    json!({"file_path": cwd.join("real.txt")}),
                );
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn allows_edits_inside_every_root_and_asks_past_a_symlink_out_of_them() {
                // The cwd and the extra root form one set of roots: a link from one into the other
                // stays inside, and a link from either to anywhere else asks.
                let dir = tempfile::tempdir().unwrap();
                let base = dunce::canonicalize(dir.path()).unwrap();
                let (repo, extra, outside) =
                    (base.join("repo"), base.join("extra"), base.join("outside"));
                for root in [&repo, &extra, &outside] {
                    std::fs::create_dir(root).unwrap();
                }
                std::os::unix::fs::symlink("../extra/notes.md", repo.join("to-extra.md")).unwrap();
                std::os::unix::fs::symlink("../outside/notes.md", extra.join("to-outside.md"))
                    .unwrap();
                let workspace =
                    Workspace::resolve(&repo, &[extra.to_str().unwrap().to_owned()]).unwrap();
                let tools = ToolRegistry::new(vec![Box::new(WriteTool)]);
                let mut dispatch_ctx = DispatchContext {
                    workspace: Arc::new(workspace),
                    ..dispatch_with(Mode::Manual, &[], &[], &repo)
                };

                for (id, path, asks) in [
                    ("in_cwd", repo.join("plain.md"), false),
                    ("in_extra", extra.join("new.md"), false),
                    ("into_extra", repo.join("to-extra.md"), false),
                    ("out_of_extra", extra.join("to-outside.md"), true),
                    ("outside", outside.join("new.md"), true),
                ] {
                    let sink = CapturingSink::default();
                    let (tx, mut rx) = mpsc::channel::<Op>(1);
                    tx.try_send(Op::Resolve {
                        id: id.to_owned(),
                        decision: Decision::Deny,
                    })
                    .unwrap();

                    let gate = gate_call(
                        &tools,
                        id,
                        "write",
                        json!({"file_path": path, "content": "x"}),
                        &sink,
                        &mut rx,
                        &mut dispatch_ctx,
                    )
                    .await
                    .expect("a write resolves without abort");

                    assert_eq!(
                        approval_ids(&sink),
                        if asks { vec![id] } else { vec![] },
                        "{id}"
                    );
                    assert_eq!(matches!(gate, Gated::Ready(_)), !asks, "{id}");
                }
            }
        }
    }

    mod await_approval {
        use super::*;

        #[tokio::test]
        async fn ignores_a_stale_decision_for_another_call() {
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

            let decision = await_approval(
                "tool_1",
                &mut rx,
                &mut Inbox::default(),
                &CapturingSink::default(),
            )
            .await
            .expect("matching decision resolves");
            assert_eq!(decision, Decision::Deny);
        }

        #[tokio::test]
        async fn holds_a_mid_approval_submit_and_mode_switch_in_the_inbox() {
            let (tx, mut rx) = mpsc::channel::<Op>(3);
            tx.try_send(Op::Queue("queued".to_owned())).unwrap();
            tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
            tx.try_send(Op::Resolve {
                id: "tool_1".to_owned(),
                decision: Decision::Approve,
            })
            .unwrap();
            let mut inbox = Inbox::default();
            let sink = CapturingSink::default();

            let decision = await_approval("tool_1", &mut rx, &mut inbox, &sink)
                .await
                .expect("approval resolves");

            assert_eq!(decision, Decision::Approve);
            assert_eq!(inbox.prompts, vec!["queued".to_owned()]);
            assert_eq!(inbox.mode, Some(Mode::Plan));
            assert!(
                matches!(sink.events().as_slice(), [Event::ModeChanged(Mode::Plan)]),
                "{:?}",
                sink.events(),
            );
        }

        #[tokio::test]
        async fn interrupt_and_shutdown_abort_the_turn() {
            for (op, want_shutdown) in [(Op::Interrupt, false), (Op::Shutdown, true)] {
                let (tx, mut rx) = mpsc::channel::<Op>(1);
                tx.try_send(op.clone()).unwrap();
                let abort = await_approval(
                    "tool_1",
                    &mut rx,
                    &mut Inbox::default(),
                    &CapturingSink::default(),
                )
                .await
                .expect_err("interrupt / shutdown abort the wait");
                if want_shutdown {
                    assert!(matches!(abort, TurnAbort::Shutdown), "{op:?}");
                } else {
                    assert!(matches!(abort, TurnAbort::Cancelled), "{op:?}");
                }
            }
        }
    }
}
