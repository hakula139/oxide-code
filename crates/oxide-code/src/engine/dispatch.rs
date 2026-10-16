//! One round's tool calls: the permission gate, approvals, and each call's result.

use std::collections::HashMap;

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::warn;

use super::abort::{AbortResult, TurnAbort, await_unless_aborted};
use crate::file_tracker;
use crate::message::ContentBlock;
use crate::permission::SessionGrant;
use crate::permission::rule::Rule;
use crate::protocol::{Decision, Event, EventSink, Op, ToolCall, ToolResult};
use crate::tool::{ToolMetadata, ToolOutput, ToolRegistry};

/// Inputs the permission gate needs at each tool call: the policy, which gains the session allow
/// rules the user grants, and the working directory to resolve path targets against.
pub(super) struct GateContext<'a> {
    pub(super) policy: &'a mut crate::permission::Policy,
    pub(super) cwd: &'a std::path::Path,
}

pub(super) fn collect_tool_uses(
    blocks: &[ContentBlock],
) -> Vec<(String, String, serde_json::Value)> {
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
pub(super) struct ToolRound {
    pub(super) results: Vec<ContentBlock>,
    pub(super) sidecars: Vec<(String, ToolMetadata)>,
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

pub(super) async fn run_tool_round(
    tools: &ToolRegistry,
    tool_uses: Vec<(String, String, serde_json::Value)>,
    parse_errors: &HashMap<String, String>,
    sink: &dyn EventSink,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
    gate: &mut GateContext<'_>,
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
pub(super) const NOT_RUN: &str = "Not run, because the user interrupted the turn.";

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
    gate: &mut GateContext<'_>,
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
/// `Ok(Some(output))` with a synthetic denial, or an abort on an interrupt, a shutdown, or an
/// [`Decision::Abort`] during an outstanding approval. An `ask` emits [`Event::ApprovalRequested`]
/// and blocks on the matching [`Op::Resolve`], which every frontend answers in its own way. An
/// allow-for-session answer adds its rule to the policy once the call's own grant admits it. It
/// denies at once when no frontend can receive the request.
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
    gate: &mut GateContext<'_>,
) -> AbortResult<Option<ToolOutput>> {
    let Some(tool) = tools.get(name) else {
        // Unknown tool: let `tools.run` produce its own "unknown tool" error rather than denying.
        return Ok(None);
    };

    let class = tool.permission_class();
    let gate_target = tool.gate_target(input, gate.cwd);
    tool.pin_target(input, &gate_target);
    let decision = gate.policy.decide(class, &gate_target.as_target());

    match decision {
        crate::permission::Decision::Allow => Ok(None),
        crate::permission::Decision::Deny => Ok(Some(denied_output(name))),
        crate::permission::Decision::Ask => {
            // The approval request is a control-plane event: if it can't be delivered, no frontend
            // can answer it and `await_approval` would block forever. Fail closed on a send
            // error, so the turn is not stranded.
            let grant = gate.policy.session_grant(class, &gate_target);
            if sink
                .send(Event::ApprovalRequested {
                    id: id.to_owned(),
                    preview: tool.approval_preview(input),
                    grant: grant.clone().map(Box::new),
                })
                .is_err()
            {
                return Ok(Some(denied_output(name)));
            }
            match await_approval(id, ops, pending).await? {
                Decision::Approve => Ok(None),
                Decision::AllowForSession { rule } => {
                    Ok(allow_for_session(gate, grant.as_ref(), *rule, name, sink))
                }
                Decision::Deny => Ok(Some(denied_output(name))),
                Decision::DenyWithFeedback { feedback } => Ok(Some(ToolOutput::error(format!(
                    "The {name} call was denied with this feedback: {feedback}"
                )))),
                Decision::Abort => Err(TurnAbort::Cancelled),
            }
        }
    }
}

/// Adds the frontend's `rule` to the session allow rules once the call's own `grant` admits it,
/// since the frontend's check is advisory. Returns a denial when it does not.
fn allow_for_session(
    gate: &mut GateContext<'_>,
    grant: Option<&SessionGrant>,
    rule: Rule,
    name: &str,
    sink: &dyn EventSink,
) -> Option<ToolOutput> {
    let admitted = grant
        .context("this call offers no session rule")
        .and_then(|grant| grant.admit(rule));
    match admitted {
        Ok(rule) => {
            gate.policy.allow_for_session(rule);
            sink.emit(
                Event::SessionRulesUpdated(gate.policy.session_allow().to_vec()),
                "session-rules-updated",
            );
            None
        }
        Err(e) => {
            sink.emit(
                Event::Error(format!(
                    "Session rule refused, so the {name} call was denied: {e:#}"
                )),
                "session-rule-refused",
            );
            Some(denied_output(name))
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::engine::testing::{EchoTool, gate_with, inert_ops};
    #[cfg(unix)]
    use crate::file_tracker::FileTracker;
    use crate::protocol::CapturingSink;
    use crate::tool::ToolRegistry;

    // ── dispatch_tool_call ──

    #[tokio::test]
    async fn dispatch_tool_call_interrupt_during_approval_reports_the_call_not_run() {
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &[]);
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
            &mut gate,
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
        let mut gate = gate_with(crate::permission::Mode::Auto, &["bash"], &[]);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &mut gate,
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
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &["bash"]);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &mut gate,
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
    async fn check_permission_ask_emits_event_and_approve_proceeds() {
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = CapturingSink::new();
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &[]);
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
            &mut gate,
        )
        .await
        .expect("approve resolves without abort");

        assert!(denial.is_none(), "an approved call proceeds");
        assert!(
            matches!(
                sink.events().as_slice(),
                [Event::ApprovalRequested { id, .. }] if id == "tool_1",
            ),
            "ask must surface one approval event carrying the call id: {:?}",
            sink.events(),
        );
    }

    #[tokio::test]
    async fn check_permission_allow_for_session_runs_later_calls_the_rule_covers_unasked() {
        let tools = ToolRegistry::new(vec![Box::new(crate::tool::bash::BashTool)]);
        let sink = CapturingSink::new();
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &[]);
        let rule = Rule::parse("bash(cargo test:*)").unwrap();
        let (tx, mut rx) = mpsc::channel::<Op>(1);
        tx.try_send(Op::Resolve {
            id: "tool_1".to_owned(),
            decision: Decision::AllowForSession {
                rule: Box::new(rule.clone()),
            },
        })
        .unwrap();
        drop(tx);

        for (id, command) in [("tool_1", "cargo test"), ("tool_2", "cargo test --release")] {
            let denial = check_permission(
                &tools,
                id,
                "bash",
                &mut json!({ "command": command }),
                &sink,
                &mut rx,
                &mut Vec::new(),
                &mut gate,
            )
            .await
            .expect("the granted rule settles the second call without waiting");
            assert!(denial.is_none(), "{command} proceeds");
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
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(suggested, ["bash(cargo test)"], "only the first call asks");
        assert!(
            events.iter().any(
                |e| matches!(e, Event::SessionRulesUpdated(rules) if *rules == [rule.clone()])
            ),
            "{events:?}",
        );
        assert_eq!(gate.policy.session_allow(), [rule]);
    }

    #[tokio::test]
    async fn check_permission_allow_for_session_denies_a_rule_the_call_grant_does_not_admit() {
        // The engine re-checks the frontend's rule against its own grant: a rule that misses the
        // call, one in another class, and any rule for a call that offered no grant all deny.
        let tools = ToolRegistry::new(vec![
            Box::new(crate::tool::bash::BashTool),
            Box::new(EchoTool),
        ]);
        for (name, input, rule) in [
            (
                "bash",
                json!({ "command": "cargo test" }),
                "bash(cargo build:*)",
            ),
            ("bash", json!({ "command": "cargo test" }), "edit(**)"),
            ("echo", json!({}), "bash"),
        ] {
            let sink = CapturingSink::new();
            let mut gate = gate_with(crate::permission::Mode::Auto, &[], &[]);
            let (tx, mut rx) = mpsc::channel::<Op>(1);
            tx.try_send(Op::Resolve {
                id: "tool_1".to_owned(),
                decision: Decision::AllowForSession {
                    rule: Box::new(Rule::parse(rule).unwrap()),
                },
            })
            .unwrap();

            let denial = check_permission(
                &tools,
                "tool_1",
                name,
                &mut input.clone(),
                &sink,
                &mut rx,
                &mut Vec::new(),
                &mut gate,
            )
            .await
            .expect("a refused rule resolves without abort")
            .unwrap_or_else(|| panic!("{rule} must not let the {name} call run"));

            assert!(denial.content.contains("denied by the permission policy"));
            assert_eq!(gate.policy.session_allow(), [], "{rule}");
            let events = sink.events();
            assert!(
                events.iter().any(|e| matches!(
                    e,
                    Event::Error(msg) if msg.starts_with(&format!("Session rule refused, so the {name} call was denied: "))
                )),
                "{rule}: {events:?}",
            );
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, Event::SessionRulesUpdated(_))),
                "{rule}: {events:?}",
            );
        }
    }

    #[tokio::test]
    async fn check_permission_ask_undeliverable_event_fails_closed() {
        // If the approval event can't be delivered, no modal can answer it, so the gate must deny
        // before `await_approval` blocks forever. An inert receiver would hang on a regression.
        let tools = ToolRegistry::new(vec![Box::new(EchoTool)]);
        let sink = crate::protocol::FailingSink;
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &[]);
        let (_tx, mut rx) = mpsc::channel::<Op>(1);

        let denial = check_permission(
            &tools,
            "tool_1",
            "echo",
            &mut json!({}),
            &sink,
            &mut rx,
            &mut Vec::new(),
            &mut gate,
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
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &[]);
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
            &mut gate,
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
        let mut gate = gate_with(crate::permission::Mode::Auto, &[], &["bash"]);
        let denial = check_permission(
            &tools,
            "tool_1",
            "mystery",
            &mut json!({}),
            &sink,
            &mut inert_ops(),
            &mut Vec::new(),
            &mut gate,
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
        let mut gate = GateContext {
            cwd: &cwd,
            ..gate_with(crate::permission::Mode::Auto, &[], &[])
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
            &mut gate,
        )
        .await
        .expect("a read resolves without abort");

        assert!(denial.is_none(), "a read-class call proceeds");
        assert_eq!(input["file_path"], json!(cwd.join("real.txt")));
    }

    // ── await_approval ──

    #[tokio::test]
    async fn await_approval_ignores_a_stale_decision_for_another_call() {
        // A decision for a resolved call must not satisfy the wait, but the matching id does.
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
}
