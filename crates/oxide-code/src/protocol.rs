//! The protocol between the engine and its frontends: the [`Op`]s a frontend sends, the [`Event`]s
//! the engine emits, and the [`EventSink`] that delivers them. Ops, events, and their payloads
//! serialize to JSON, with each enum tagged by `type` and its payload under `data`, so they can
//! cross a process boundary. Headless `stream-json` output prints events in this form, so renaming
//! a tag or field breaks the release stability surface.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{AutoCompactionConfig, Effort};
use crate::model::{ModelSpec, ResolvedModelId};
use crate::permission::SessionGrant;
use crate::permission::rule::Rule;
use crate::tool::{ApprovalPreview, ToolResultView};

// ── Ops ──

/// Commands from a frontend to the engine. Multiplexed onto a single `mpsc`, so a running turn can
/// race them against its in-flight stream and tool futures with one biased `select!`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum Op {
    /// Starts a turn with this prompt.
    Prompt(String),
    /// Adds a prompt typed while a turn runs. The turn splices it in at its next round boundary and
    /// confirms with [`Event::PromptDrained`]. The frontend keeps every unconfirmed prompt and
    /// submits it later, so the engine drops one that arrives after its turn ended.
    Queue(String),
    /// Cancels the running turn or compaction, which ends with [`Event::Cancelled`].
    Interrupt,
    /// Answers the outstanding [`Event::ApprovalRequested`]. The `id` is matched against the
    /// blocked call so a decision for an already-dropped call is ignored.
    Resolve { id: String, decision: Decision },
    /// Swaps the model, the effort, or both for later turns, and confirms with
    /// [`Event::ConfigChanged`]. A `None` field keeps its current value.
    SetModel {
        model: Option<ResolvedModelId>,
        effort: Option<Effort>,
    },
    /// Summarizes the transcript into a synthetic continuation, focused by `instructions`, and
    /// persists the compaction boundary. Confirms with [`Event::SessionCompacted`].
    Compact { instructions: Option<String> },
    /// Finalizes the live session and continues in a fresh, empty one.
    Clear,
    /// Archives the live session, then continues in a fresh one like [`Self::Clear`].
    Archive,
    /// Finalizes the live session and continues the session file at `path` in place.
    Resume { path: PathBuf },
    /// Sets the session title by hand, which stops AI title generation for this session.
    Rename { title: String },
    /// Ends any running operation without a terminal event, finalizes the live session, and stops
    /// the engine. Closing the op channel does the same.
    Shutdown,
}

/// The user's verdict on a pending approval, carried back on [`Op::Resolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum Decision {
    Approve,
    /// Approve, and allow every later call `rule` matches for the rest of the session. The engine
    /// admits `rule` against its own grant for the call and denies the call when it fails.
    AllowForSession {
        rule: Box<Rule>,
    },
    Deny,
}

// ── Events ──

/// Token and cost snapshot for status-line rendering.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageSnapshot {
    /// Cache-aware input total: `input + cache_creation + cache_read`. Drives the context-pressure
    /// percentage and auto-compaction threshold. `None` after a compaction, manual or automatic,
    /// until the next request reports usage, which hides the context segment.
    pub(crate) context_tokens: Option<u32>,
    /// Active model context window. `None` hides the percentage.
    pub(crate) context_window: Option<u32>,
    /// Running session cost in USD, accumulated across every request, summaries included. Cleared
    /// when the session rolls or resumes. `None` hides the cost segment.
    pub(crate) estimated_cost_usd: Option<f64>,
}

/// One-way notifications from the engine to the frontend reading its
/// [`EngineHandle`](crate::engine::EngineHandle).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum Event {
    /// Assistant text chunk to append to the in-flight reply.
    StreamToken(String),
    /// Extended-thinking chunk rendered separately from `StreamToken` and only when the user
    /// has thinking display enabled.
    ThinkingToken(String),
    /// Progress note the model wrote before a tool call, emitted as that call's block starts
    /// streaming. Sinks drop it when thinking display is on, since the same text already streamed
    /// as a [`Self::ThinkingToken`].
    ProgressUpdate(String),
    /// A request failed before any content and will be sent again after a delay. Leaves the turn
    /// running and renders as a dim notice.
    Retrying(String),
    /// Tool call emitted by the model.
    ToolCallStart(ToolCall),
    /// Result of the call the matching [`Self::ToolCallStart`] opened.
    ToolCallEnd(ToolResult),
    /// An [`Op::Queue`] prompt was spliced into the live transcript, so the frontend moves it from
    /// the queue into the chat.
    PromptDrained(String),
    /// Turn ended cleanly with a final assistant reply (no further tool rounds).
    TurnComplete,
    /// The in-flight prompt, compaction, or resume failed. Like [`Self::TurnComplete`] and
    /// [`Self::Cancelled`], it ends the operation.
    TurnFailed(String),
    /// Live usage and estimated cost snapshot for the current process.
    UsageUpdated(UsageSnapshot),
    /// The user cancelled mid-turn with [`Op::Interrupt`]. The in-flight reply is truncated, and
    /// frontends mark where it stopped.
    Cancelled,
    /// Automatic compaction started before the submitted prompt runs. The frontend shows compacting
    /// status while the summarizer request streams.
    AutoCompactionStarted,
    /// The session title changed, from the background generator or an [`Op::Rename`].
    SessionTitleUpdated { session_id: String, title: String },
    /// An [`Op::Clear`] or [`Op::Archive`] rolled the session, making a new session UUID active.
    /// `archived` marks an archive roll, so the frontend can name the session it archived.
    SessionRolled { id: String, archived: bool },
    /// An [`Op::Resume`] swapped to an existing session in place. Payload carries the target's
    /// transcript projected for display, so the frontend can rebuild chat without restarting the
    /// process. `unarchived` reports that the target stayed archived until this resume.
    SessionResumed {
        id: String,
        title: Option<String>,
        unarchived: bool,
        history: Vec<Interaction>,
    },
    /// A compaction finished. The in-memory transcript has already been swapped to the synthetic
    /// continuation, so the frontend clears its chat and replays a single boundary block.
    /// `automatic` is true when the compaction ran ahead of a submitted prompt, in which case the
    /// frontend stays busy since the next turn starts immediately.
    SessionCompacted {
        summary: String,
        pre_count: u32,
        instructions: Option<String>,
        automatic: bool,
    },
    /// Live config after an [`Op::SetModel`]. `effort` is the resolved value. `requested_effort`
    /// is the user's pick, surfaced when an unknown model drops it.
    ConfigChanged {
        model: ModelSpec,
        effort: Option<Effort>,
        requested_effort: Option<Effort>,
        auto_compaction: AutoCompactionConfig,
    },
    /// User-visible error that leaves any in-flight turn running, such as a session write failure.
    /// Renders as a dedicated chat block.
    Error(String),
    /// A gated tool call needs the user's approval before it runs. Carries the tool-use id so a
    /// stale decision for a dropped call is ignored, plus a small preview for the modal to render.
    /// `grant` is the session allow rule the user may add instead, when one can name the call.
    /// The agent loop blocks on the matching [`Op::Resolve`].
    ApprovalRequested {
        id: String,
        preview: ApprovalPreview,
        grant: Option<Box<SessionGrant>>,
    },
    /// The user granted a session allow rule. Carries every session allow rule now in force.
    SessionRulesUpdated(Vec<Rule>),
}

// ── Tool Calls ──

/// A tool call as the row a frontend shows for it, live in [`Event::ToolCallStart`] and resumed in
/// [`Interaction::ToolCall`]. `input` carries the model's raw arguments for consumers of the
/// serialized protocol, and `icon` / `label` are their display form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) icon: String,
    pub(crate) label: String,
}

/// A tool call's result, live in [`Event::ToolCallEnd`] and resumed in
/// [`Interaction::ToolResult`]. `content` is what the model sees, `title` the tool's own header for
/// the result, and `view` its structured body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolResult {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) title: Option<String>,
    pub(crate) view: ToolResultView,
}

// ── History ──

/// A unit of a resumed transcript as a frontend shows it, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum Interaction {
    /// The boundary a compaction left, which the rest of the transcript continues from. Renders
    /// like a live [`Event::SessionCompacted`].
    Compacted {
        summary: String,
        pre_count: u32,
        instructions: Option<String>,
    },
    UserText(String),
    AssistantText(String),
    AssistantThinking(String),
    /// Note introducing the next tool call. Its text also appears in the preceding
    /// [`Self::AssistantThinking`], so renderers show one or the other.
    ProgressUpdate(String),
    ToolCall(ToolCall),
    /// A result that follows its call, or stands alone when the transcript lost the call.
    ToolResult(ToolResult),
}

// ── Event Sink ──

/// Transport from the engine to a frontend. Implementations must be cheap to clone-by-`&` (the
/// loop calls `send` on a hot path) and tolerate dropped events without panicking. Call sites use
/// [`Self::emit`] for one-shot or user-facing events (logs on drop). Raw [`Self::send`] with
/// `_ = ` is reserved for per-token streaming where dropping a single event is harmless.
pub(crate) trait EventSink: Send + Sync {
    fn send(&self, event: Event) -> Result<()>;

    /// Sends `event` and logs at `error!` if the channel rejects it. Use for any signal whose
    /// loss would leave the frontend inconsistent (errors, cancellation, turn completion, state
    /// changes).
    fn emit(&self, event: Event, label: &'static str) {
        if let Err(e) = self.send(event) {
            tracing::error!("{label} event dropped: {e}");
        }
    }

    /// Surfaces a session-writer failure as a single user-visible [`Event::Error`].
    /// No-op when `failure` is `None`.
    fn session_write_error(&self, failure: Option<&str>) {
        if let Some(msg) = failure {
            self.emit(
                Event::Error(format!("Session write failed: {msg}")),
                "session-write-failed",
            );
        }
    }
}

// ── Test Fixtures ──

/// Channel pair whose `recv()` stays pending forever.
#[cfg(test)]
pub(crate) fn inert_op_channel() -> (
    tokio::sync::mpsc::Sender<Op>,
    tokio::sync::mpsc::Receiver<Op>,
) {
    tokio::sync::mpsc::channel(1)
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct CapturingSink(std::sync::Arc<std::sync::Mutex<Vec<Event>>>);

#[cfg(test)]
impl CapturingSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl EventSink for CapturingSink {
    fn send(&self, event: Event) -> Result<()> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

/// An [`EventSink`] whose `send` always fails, for exercising the fail-closed paths that depend on
/// a control-plane event reaching the frontend.
#[cfg(test)]
pub(crate) struct FailingSink;

#[cfg(test)]
impl EventSink for FailingSink {
    fn send(&self, _event: Event) -> Result<()> {
        anyhow::bail!("sink closed")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::permission::{GateTarget, PermissionClass, Policy};
    use crate::tool::{
        ApprovalBody, DiffChunk, DiffLine, GrepFileGroup, GrepMatchLine, ReadExcerptLine,
    };

    fn diff_chunks() -> Vec<DiffChunk> {
        vec![DiffChunk {
            old: vec![DiffLine {
                number: 3,
                text: "old".to_owned(),
            }],
            new: vec![DiffLine {
                number: 3,
                text: "new".to_owned(),
            }],
        }]
    }

    fn ls_call() -> ToolCall {
        ToolCall {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "ls"}),
            icon: "$".to_owned(),
            label: "ls".to_owned(),
        }
    }

    fn ls_result(view: ToolResultView) -> ToolResult {
        ToolResult {
            id: "t1".to_owned(),
            content: "ok".to_owned(),
            is_error: false,
            title: Some("Ran".to_owned()),
            view,
        }
    }

    /// A resumed history holding every [`Interaction`] variant, with one result per
    /// [`ToolResultView`] variant.
    fn full_history() -> Vec<Interaction> {
        let views = [
            ToolResultView::Text {
                content: "ok".to_owned(),
            },
            ToolResultView::ReadExcerpt {
                path: "src/main.rs".to_owned(),
                lines: vec![ReadExcerptLine {
                    number: 1,
                    text: "fn main() {}".to_owned(),
                }],
                total_lines: 1,
            },
            ToolResultView::Diff {
                chunks: diff_chunks(),
                replace_all: false,
                replacements: 1,
            },
            ToolResultView::GrepMatches {
                groups: vec![GrepFileGroup {
                    path: "src/lib.rs".to_owned(),
                    lines: vec![GrepMatchLine {
                        number: 7,
                        text: "TODO".to_owned(),
                        is_match: true,
                    }],
                }],
                truncated: true,
            },
            ToolResultView::GlobFiles {
                pattern: "*.rs".to_owned(),
                files: vec!["src/main.rs".to_owned()],
                total: 1,
            },
        ];
        let mut history = vec![
            Interaction::Compacted {
                summary: "Summary.".to_owned(),
                pre_count: 4,
                instructions: Some("focus".to_owned()),
            },
            Interaction::UserText("ask".to_owned()),
            Interaction::AssistantThinking("thinking".to_owned()),
            Interaction::ProgressUpdate("Reading.".to_owned()),
            Interaction::ToolCall(ls_call()),
            Interaction::AssistantText("reply".to_owned()),
        ];
        history.extend(
            views
                .into_iter()
                .map(|view| Interaction::ToolResult(ls_result(view))),
        );
        history
    }

    /// Each [`Op`]'s wire tag. The match has no wildcard arm, so a new variant fails to compile
    /// here until it gets a tag and a case in `every_op`.
    fn op_tag(op: &Op) -> &'static str {
        match op {
            Op::Prompt(_) => "prompt",
            Op::Queue(_) => "queue",
            Op::Interrupt => "interrupt",
            Op::Resolve { .. } => "resolve",
            Op::SetModel { .. } => "set_model",
            Op::Compact { .. } => "compact",
            Op::Clear => "clear",
            Op::Archive => "archive",
            Op::Resume { .. } => "resume",
            Op::Rename { .. } => "rename",
            Op::Shutdown => "shutdown",
        }
    }

    fn every_op() -> Vec<Op> {
        vec![
            Op::Prompt("hi".to_owned()),
            Op::Queue("next".to_owned()),
            Op::Interrupt,
            Op::Resolve {
                id: "t1".to_owned(),
                decision: Decision::Approve,
            },
            Op::Resolve {
                id: "t1".to_owned(),
                decision: Decision::AllowForSession {
                    rule: Box::new(Rule::parse("bash(cargo test:*)").unwrap()),
                },
            },
            Op::SetModel {
                model: Some(ResolvedModelId::new("claude-opus-5-5".to_owned())),
                effort: Some(Effort::High),
            },
            Op::Compact {
                instructions: Some("focus on auth".to_owned()),
            },
            Op::Clear,
            Op::Archive,
            Op::Resume {
                path: PathBuf::from("/repo/session.jsonl"),
            },
            Op::Rename {
                title: "Title".to_owned(),
            },
            Op::Shutdown,
        ]
    }

    /// Each [`Event`]'s wire tag. The match has no wildcard arm, so a new variant fails to compile
    /// here until it gets a tag and a case in `every_event`.
    fn event_tag(event: &Event) -> &'static str {
        match event {
            Event::StreamToken(_) => "stream_token",
            Event::ThinkingToken(_) => "thinking_token",
            Event::ProgressUpdate(_) => "progress_update",
            Event::Retrying(_) => "retrying",
            Event::ToolCallStart(_) => "tool_call_start",
            Event::ToolCallEnd(_) => "tool_call_end",
            Event::PromptDrained(_) => "prompt_drained",
            Event::TurnComplete => "turn_complete",
            Event::TurnFailed(_) => "turn_failed",
            Event::UsageUpdated(_) => "usage_updated",
            Event::Cancelled => "cancelled",
            Event::AutoCompactionStarted => "auto_compaction_started",
            Event::SessionTitleUpdated { .. } => "session_title_updated",
            Event::SessionRolled { .. } => "session_rolled",
            Event::SessionResumed { .. } => "session_resumed",
            Event::SessionCompacted { .. } => "session_compacted",
            Event::ConfigChanged { .. } => "config_changed",
            Event::Error(_) => "error",
            Event::ApprovalRequested { .. } => "approval_requested",
            Event::SessionRulesUpdated(_) => "session_rules_updated",
        }
    }

    /// Every [`Event`] variant, plus one approval per [`ApprovalBody`] variant.
    fn every_event() -> Vec<Event> {
        vec![
            Event::StreamToken("hello".to_owned()),
            Event::ThinkingToken("hmm".to_owned()),
            Event::ProgressUpdate("Reading.".to_owned()),
            Event::Retrying("Retrying in 1s.".to_owned()),
            Event::ToolCallStart(ls_call()),
            Event::ToolCallEnd(ToolResult {
                is_error: true,
                title: None,
                ..ls_result(ToolResultView::Text {
                    content: "ok".to_owned(),
                })
            }),
            Event::PromptDrained("next".to_owned()),
            Event::TurnComplete,
            Event::TurnFailed("boom".to_owned()),
            Event::UsageUpdated(UsageSnapshot {
                context_tokens: Some(1_200),
                context_window: Some(200_000),
                estimated_cost_usd: Some(0.25),
            }),
            Event::Cancelled,
            Event::AutoCompactionStarted,
            Event::SessionTitleUpdated {
                session_id: "s1".to_owned(),
                title: "Title".to_owned(),
            },
            Event::SessionRolled {
                id: "s2".to_owned(),
                archived: true,
            },
            Event::SessionResumed {
                id: "s3".to_owned(),
                title: Some("Resumed".to_owned()),
                unarchived: true,
                history: full_history(),
            },
            Event::SessionCompacted {
                summary: "Summary.".to_owned(),
                pre_count: 4,
                instructions: Some("focus".to_owned()),
                automatic: true,
            },
            Event::ConfigChanged {
                model: ModelSpec::resolve("claude-opus-5-5".to_owned()),
                effort: Some(Effort::High),
                requested_effort: Some(Effort::Max),
                auto_compaction: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(400_000),
                },
            },
            Event::Error("Session write failed.".to_owned()),
            Event::ApprovalRequested {
                id: "t2".to_owned(),
                preview: ApprovalPreview {
                    title: "Edit".to_owned(),
                    body: ApprovalBody::Diff(diff_chunks()),
                },
                grant: None,
            },
            Event::ApprovalRequested {
                id: "t3".to_owned(),
                preview: ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command("cargo test".to_owned()),
                },
                grant: Some(Box::new(
                    Policy::default()
                        .session_grant(
                            PermissionClass::Bash,
                            &GateTarget::Command("cargo test".to_owned()),
                        )
                        .unwrap(),
                )),
            },
            Event::SessionRulesUpdated(vec![Rule::parse("bash(cargo test:*)").unwrap()]),
        ]
    }

    // ── Op ──

    #[test]
    fn op_round_trips_every_variant_under_its_type_tag() {
        for op in every_op() {
            let value = serde_json::to_value(&op).unwrap();
            assert_eq!(value["type"], op_tag(&op), "{value}");
            assert_eq!(serde_json::from_value::<Op>(value).unwrap(), op);
        }
    }

    #[test]
    fn op_carries_its_payload_under_data() {
        assert_eq!(
            serde_json::to_value(Op::Prompt("hi".to_owned())).unwrap(),
            json!({"type": "prompt", "data": "hi"}),
        );
        assert_eq!(
            serde_json::to_value(Op::Resolve {
                id: "t1".to_owned(),
                decision: Decision::Deny,
            })
            .unwrap(),
            json!({"type": "resolve", "data": {"id": "t1", "decision": {"type": "deny"}}}),
        );
        assert_eq!(
            serde_json::to_value(Decision::AllowForSession {
                rule: Box::new(Rule::parse("bash(cargo test:*)").unwrap()),
            })
            .unwrap(),
            json!({"type": "allow_for_session", "data": {"rule": "bash(cargo test:*)"}}),
        );
        assert_eq!(
            serde_json::to_value(Op::Interrupt).unwrap(),
            json!({"type": "interrupt"}),
        );
    }

    // ── Event ──

    #[test]
    fn event_round_trips_every_variant_under_its_type_tag() {
        for event in every_event() {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["type"], event_tag(&event), "{value}");
            assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
        }
    }

    #[test]
    fn event_carries_nested_payloads_tagged_the_same_way() {
        let event = Event::ToolCallEnd(ToolResult {
            id: "t1".to_owned(),
            content: "Edited.".to_owned(),
            is_error: false,
            title: Some("Edited f.rs".to_owned()),
            view: ToolResultView::Diff {
                chunks: diff_chunks(),
                replace_all: false,
                replacements: 1,
            },
        });
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "tool_call_end",
                "data": {
                    "id": "t1",
                    "content": "Edited.",
                    "is_error": false,
                    "title": "Edited f.rs",
                    "view": {
                        "type": "diff",
                        "data": {
                            "chunks": [{
                                "old": [{"number": 3, "text": "old"}],
                                "new": [{"number": 3, "text": "new"}],
                            }],
                            "replace_all": false,
                            "replacements": 1,
                        },
                    },
                },
            }),
        );
    }

    #[test]
    fn event_serializes_a_model_as_its_id() {
        let event = Event::ConfigChanged {
            model: ModelSpec::resolve("claude-opus-5-5".to_owned()),
            effort: None,
            auto_compaction: AutoCompactionConfig::disabled(),
            requested_effort: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["data"]["model"], "claude-opus-5-5");

        let Event::ConfigChanged { model, .. } = serde_json::from_value(value).unwrap() else {
            panic!("config_changed must round-trip to ConfigChanged");
        };
        assert_eq!(model.short_display_name(), "Opus 5.5");
    }
}
