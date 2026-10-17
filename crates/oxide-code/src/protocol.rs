//! The protocol between the engine and its frontends: the [`Op`]s a frontend sends, the [`Event`]s
//! the engine emits, and the [`EventSink`] that delivers them. Ops, events, and their payloads
//! serialize to JSON, with each enum tagged by `type` and its payload under `data`, so they can
//! cross a process boundary. Headless `stream-json` output prints events in this form, so renaming
//! a tag or field breaks the release stability surface.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::client::RateLimits;
use crate::config::Effort;
use crate::config::auth::AuthSource;
use crate::config::compaction::AutoCompactionConfig;
use crate::model::{ModelSpec, ResolvedModelId};
use crate::permission::rule::Rule;
use crate::permission::{Basis, Mode, SessionGrant};
use crate::tool::bash::background::BackgroundShell;
use crate::tool::tasks::Task;
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
    /// Cancels the running turn, compaction, or sign-in, which ends with [`Event::Cancelled`].
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
    /// Switches the permission mode and confirms with [`Event::ModeChanged`]. Mid-turn, the switch
    /// applies from the next tool call the gate weighs.
    SetMode(Mode),
    /// Summarizes the transcript into a synthetic continuation, focused by `instructions`, and
    /// persists the compaction boundary. Confirms with [`Event::SessionCompacted`].
    Compact { instructions: Option<String> },
    /// Finalizes the live session and continues in a fresh, empty one.
    Clear,
    /// Archives the live session, then continues in a fresh one like [`Self::Clear`].
    Archive,
    /// Finalizes the live session and continues the saved session `target` names in place.
    /// Confirms with [`Event::SessionResumed`], or reports a target it cannot load with
    /// [`Event::TurnFailed`].
    Resume { target: ResumeTarget },
    /// Sets the session title by hand, which stops AI title generation for this session.
    Rename { title: String },
    /// Starts a browser sign-in for the oxide-code login, announced with [`Event::LoginStarted`].
    /// It ends with [`Event::SignedIn`], or with [`Event::TurnFailed`] or [`Event::Cancelled`]
    /// like a turn, and [`Self::Interrupt`] cancels it.
    Login,
    /// Removes the oxide-code login and confirms with [`Event::SignedOut`], or reports a login it
    /// cannot remove with [`Event::Error`].
    Logout,
    /// Ends any running operation without a terminal event, finalizes the live session, and stops
    /// the engine. Closing the op channel does the same.
    Shutdown,
}

/// The saved session an [`Op::Resume`] continues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum ResumeTarget {
    /// The session file at this path.
    Path(PathBuf),
    /// The one saved session other than the live one whose id starts with this prefix, searched
    /// for in the current project first and then in every project.
    Prefix(String),
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
    /// Deny, with `feedback` returned to the model as the call's result.
    DenyWithFeedback {
        feedback: String,
    },
    /// Deny, and end the turn as [`Op::Interrupt`] does.
    Abort,
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
    /// The in-flight prompt, compaction, resume, or sign-in failed. Like [`Self::TurnComplete`]
    /// and [`Self::Cancelled`], it ends the operation.
    TurnFailed(String),
    /// Live usage and estimated cost snapshot for the current process.
    UsageUpdated(UsageSnapshot),
    /// The account's subscription usage limits, which a request just reported.
    RateLimitsUpdated(RateLimits),
    /// The user cancelled a turn, compaction, or sign-in with [`Op::Interrupt`]. The in-flight
    /// reply is truncated, and frontends mark where it stopped.
    Cancelled,
    /// Automatic compaction started before the submitted prompt runs. The frontend shows compacting
    /// status while the summarizer request streams.
    AutoCompactionStarted,
    /// Automatic compaction failed `failures` times in a row, so later prompts go out without it
    /// until [`Self::AutoCompactionResumed`].
    AutoCompactionPaused { failures: u8 },
    /// Automatic compaction runs again after [`Self::AutoCompactionPaused`], because a compaction
    /// succeeded, the model or effort changed, or a new or resumed session started.
    AutoCompactionResumed,
    /// Compaction summary chunk to append to the in-flight summary. [`Self::SessionCompacted`] or
    /// [`Self::SummaryDiscarded`] ends that summary.
    SummaryToken(String),
    /// The summary streamed so far was dropped, because a retry starts it over or the compaction
    /// failed or was cancelled. Precedes the event that reports the failure or cancellation.
    SummaryDiscarded,
    /// The session title changed, from the background generator or an [`Op::Rename`].
    SessionTitleUpdated { session_id: String, title: String },
    /// An [`Op::Rename`] could not record its title, so the title stays. Ends the rename as
    /// [`Self::SessionTitleUpdated`] does.
    RenameFailed(String),
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
    /// The permission mode after an [`Op::SetMode`].
    ModeChanged(Mode),
    /// An [`Op::Login`] is waiting for the user to sign in at `url`, which the frontend opens in a
    /// browser.
    LoginStarted { url: String },
    /// An [`Op::Login`] stored a new oxide-code login. `auth` is the credential requests now use,
    /// which stays an env var or `client.api_key` when one outranks the login.
    SignedIn { auth: AuthSource },
    /// An [`Op::Logout`] finished. `removed` reports whether an oxide-code login was stored, and
    /// `auth` is the credential requests now use, `None` when none is left. `fallback_error` says
    /// why a stored Claude Code login could not take over.
    SignedOut {
        removed: bool,
        auth: Option<AuthSource>,
        fallback_error: Option<String>,
    },
    /// User-visible error that leaves any in-flight turn running, such as a session write failure.
    /// Renders as a dedicated chat block.
    Error(String),
    /// A gated tool call needs the user's approval before it runs. Carries the tool-use id so a
    /// stale decision for a dropped call is ignored, plus a small preview for the modal to render.
    /// `basis` is what made the call ask, and `grant` is the session allow rule the user may add
    /// instead, when one can name the call. The agent loop blocks on the matching [`Op::Resolve`].
    ApprovalRequested {
        id: String,
        preview: ApprovalPreview,
        basis: Basis,
        grant: Option<Box<SessionGrant>>,
    },
    /// The user granted a session allow rule. Carries every session allow rule now in force.
    SessionRulesUpdated(Vec<Rule>),
    /// A `tasks` call set the session's task list, or a loaded session restored a non-empty one.
    /// Carries the whole list. Frontends empty their copy on [`Self::SessionRolled`] and
    /// [`Self::SessionResumed`].
    TasksUpdated(Vec<Task>),
    /// A background shell started, exited, or stopped. Carries every open shell. Frontends empty
    /// their copy on [`Self::SessionRolled`] and [`Self::SessionResumed`], since the session's
    /// shells end with it.
    BackgroundShellsUpdated(Vec<BackgroundShell>),
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
    /// Where the user interrupted a turn. Renders like a live [`Event::Cancelled`].
    Interrupted,
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
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::client::RateLimitWindow;
    use crate::permission::{Access, PermissionClass, Policy, Tier, Workspace};
    use crate::tool::bash::background::testing::shells_in_each_state;
    use crate::tool::tasks::testing::plan;
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
            ToolResultView::Tasks { tasks: plan() },
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
            Interaction::Interrupted,
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
            Op::SetMode(_) => "set_mode",
            Op::Compact { .. } => "compact",
            Op::Clear => "clear",
            Op::Archive => "archive",
            Op::Resume { .. } => "resume",
            Op::Rename { .. } => "rename",
            Op::Login => "login",
            Op::Logout => "logout",
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
                    rule: Box::new(Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()),
                },
            },
            Op::Resolve {
                id: "t1".to_owned(),
                decision: Decision::Deny,
            },
            Op::Resolve {
                id: "t1".to_owned(),
                decision: Decision::DenyWithFeedback {
                    feedback: "Use rg instead.".to_owned(),
                },
            },
            Op::Resolve {
                id: "t1".to_owned(),
                decision: Decision::Abort,
            },
            Op::SetModel {
                model: Some(ResolvedModelId::new("claude-opus-5-5".to_owned())),
                effort: Some(Effort::High),
            },
            Op::SetMode(Mode::Plan),
            Op::Compact {
                instructions: Some("focus on auth".to_owned()),
            },
            Op::Clear,
            Op::Archive,
            Op::Resume {
                target: ResumeTarget::Path(PathBuf::from("/repo/session.jsonl")),
            },
            Op::Resume {
                target: ResumeTarget::Prefix("1a2b".to_owned()),
            },
            Op::Rename {
                title: "Title".to_owned(),
            },
            Op::Login,
            Op::Logout,
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
            Event::RateLimitsUpdated(_) => "rate_limits_updated",
            Event::Cancelled => "cancelled",
            Event::AutoCompactionStarted => "auto_compaction_started",
            Event::AutoCompactionPaused { .. } => "auto_compaction_paused",
            Event::AutoCompactionResumed => "auto_compaction_resumed",
            Event::SummaryToken(_) => "summary_token",
            Event::SummaryDiscarded => "summary_discarded",
            Event::SessionTitleUpdated { .. } => "session_title_updated",
            Event::RenameFailed(_) => "rename_failed",
            Event::SessionRolled { .. } => "session_rolled",
            Event::SessionResumed { .. } => "session_resumed",
            Event::SessionCompacted { .. } => "session_compacted",
            Event::ConfigChanged { .. } => "config_changed",
            Event::ModeChanged(_) => "mode_changed",
            Event::LoginStarted { .. } => "login_started",
            Event::SignedIn { .. } => "signed_in",
            Event::SignedOut { .. } => "signed_out",
            Event::Error(_) => "error",
            Event::ApprovalRequested { .. } => "approval_requested",
            Event::SessionRulesUpdated(_) => "session_rules_updated",
            Event::TasksUpdated(_) => "tasks_updated",
            Event::BackgroundShellsUpdated(_) => "background_shells_updated",
        }
    }

    fn five_hour_limit() -> RateLimits {
        RateLimits {
            five_hour: Some(RateLimitWindow {
                utilization: 0.42,
                resets_at: 1_760_000_000,
            }),
            seven_day: None,
        }
    }

    fn cargo_test_grant() -> SessionGrant {
        Policy::default()
            .session_grant(
                PermissionClass::Exec,
                &Access::for_command("cargo test", &Workspace::new(Path::new("/repo"))),
            )
            .unwrap()
    }

    /// One approval per [`ApprovalBody`] variant, each held by a different basis.
    fn approval_requests() -> [Event; 2] {
        [
            Event::ApprovalRequested {
                id: "t2".to_owned(),
                preview: ApprovalPreview {
                    title: "Edit".to_owned(),
                    body: ApprovalBody::Diff(diff_chunks()),
                },
                basis: Basis::Mode(Mode::Manual),
                grant: None,
            },
            Event::ApprovalRequested {
                id: "t3".to_owned(),
                preview: ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command("cargo test".to_owned()),
                },
                basis: Basis::Rule {
                    tier: Tier::Ask,
                    rule: Rule::parse(PermissionClass::Exec, "cargo *").unwrap(),
                },
                grant: Some(Box::new(cargo_test_grant())),
            },
        ]
    }

    /// Every [`Event`] variant, plus one approval per [`ApprovalBody`] variant.
    fn every_event() -> Vec<Event> {
        let mut events = vec![
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
            Event::RateLimitsUpdated(five_hour_limit()),
            Event::Cancelled,
            Event::AutoCompactionStarted,
            Event::AutoCompactionPaused { failures: 3 },
            Event::AutoCompactionResumed,
            Event::SummaryToken("Recap".to_owned()),
            Event::SummaryDiscarded,
            Event::SessionTitleUpdated {
                session_id: "s1".to_owned(),
                title: "Title".to_owned(),
            },
            Event::RenameFailed("Session write failed: disk full".to_owned()),
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
            Event::ModeChanged(Mode::Yolo),
            Event::LoginStarted {
                url: "https://claude.ai/oauth/authorize?state=s".to_owned(),
            },
            Event::SignedIn {
                auth: AuthSource::OxLogin,
            },
            Event::SignedOut {
                removed: true,
                auth: Some(AuthSource::ClaudeCodeLogin),
                fallback_error: None,
            },
            Event::SignedOut {
                removed: false,
                auth: None,
                fallback_error: Some("Claude Code login has expired.".to_owned()),
            },
            Event::Error("Session write failed.".to_owned()),
            Event::SessionRulesUpdated(vec![
                Rule::parse(PermissionClass::Exec, "cargo test *").unwrap(),
            ]),
            Event::TasksUpdated(plan()),
            Event::BackgroundShellsUpdated(shells_in_each_state()),
        ];
        events.extend(approval_requests());
        events
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
                rule: Box::new(Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()),
            })
            .unwrap(),
            json!({
                "type": "allow_for_session",
                "data": {"rule": {"class": "exec", "pattern": "cargo test *"}},
            }),
        );
        assert_eq!(
            serde_json::to_value(Decision::DenyWithFeedback {
                feedback: "Use rg instead.".to_owned(),
            })
            .unwrap(),
            json!({"type": "deny_with_feedback", "data": {"feedback": "Use rg instead."}}),
        );
        assert_eq!(
            serde_json::to_value(Op::Resume {
                target: ResumeTarget::Prefix("1a2b".to_owned()),
            })
            .unwrap(),
            json!({"type": "resume", "data": {"target": {"type": "prefix", "data": "1a2b"}}}),
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
    fn event_carries_a_task_list_as_content_and_status_pairs() {
        let event = Event::TasksUpdated(plan());
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "tasks_updated",
                "data": [
                    {"content": "Read the config loader", "status": "completed"},
                    {"content": "Add the entry type", "status": "in_progress"},
                    {"content": "Write the tests", "status": "pending"},
                ],
            }),
        );
    }

    #[test]
    fn event_carries_each_background_shell_with_its_state_tagged() {
        let event = Event::BackgroundShellsUpdated(shells_in_each_state());
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "background_shells_updated",
                "data": [
                    {"id": "shell-1", "command": "cargo watch", "state": {"type": "running"}},
                    {
                        "id": "shell-2",
                        "command": "make test",
                        "state": {"type": "exited", "data": 2},
                    },
                    {
                        "id": "shell-3",
                        "command": "sleep 60",
                        "state": {"type": "killed", "data": 15},
                    },
                ],
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

    #[test]
    fn event_tags_an_auth_source_like_its_own_variants() {
        let event = Event::SignedOut {
            removed: true,
            auth: Some(AuthSource::ClaudeCodeLogin),
            fallback_error: None,
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "signed_out",
                "data": {
                    "removed": true,
                    "auth": {"type": "claude_code_login"},
                    "fallback_error": null,
                },
            }),
        );
    }
}
