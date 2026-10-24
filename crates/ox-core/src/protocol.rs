//! The protocol between the engine and its frontends: the [`Op`]s a frontend sends, the [`Event`]s
//! the engine emits, and the [`EventSink`] that delivers them. Ops, events, and their payloads
//! serialize to JSON, with each enum tagged by `type` and its payload under `data`, so they can
//! cross a process boundary. Headless `stream-json` output prints events in this form, so renaming
//! a tag or field in an event listed in `docs/guide/headless.md` breaks the stability surface.

pub mod notice;

use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::error;

use crate::client::RateLimits;
use crate::config::auth::AuthSource;
use crate::config::compaction::AutoCompactionConfig;
use crate::model::{Effort, ModelSpec, ResolvedModelId};
use crate::permission::rule::Rule;
use crate::permission::{Basis, Mode, SessionGrant};
use crate::session::entry::ArchiveState;
use crate::tool::bash::background::BackgroundShell;
use crate::tool::tasks::Task;
use crate::tool::view::{ApprovalPreview, ToolResultView};

// ── Ops ──

/// Commands from a frontend to the engine. Multiplexed onto a single `mpsc`, so a running turn can
/// race them against its in-flight stream and tool futures with one biased `select!`.
///
/// [`Self::Prompt`], [`Self::SetModel`], [`Self::Compact`], [`Self::Clear`], [`Self::Archive`],
/// [`Self::Resume`], [`Self::Rename`], [`Self::Login`], and [`Self::Logout`] require an idle engine.
/// Frontends wait for the preceding operation's terminal event before submitting them. An
/// idle-only op received during another operation is dropped without a terminal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Op {
    /// Starts a turn with `text`. On a fresh session, `title` supplies the first-prompt title, so a
    /// synthesized slash-command prompt can retain the typed command as its title.
    Prompt { text: String, title: Option<String> },
    /// Adds a prompt typed while a turn runs. The turn splices it in at its next round boundary and
    /// confirms with [`Event::PromptDrained`]. The frontend keeps every unconfirmed prompt and
    /// submits it later, so the engine drops one that arrives after its turn ended.
    Queue(String),
    /// Cancels the running turn, compaction, or sign-in, which ends with [`Event::Cancelled`].
    Interrupt,
    /// Answers the outstanding [`Event::ApprovalRequested`]. The `id` is matched against the
    /// blocked call so a decision for an already-dropped call is ignored.
    Resolve {
        id: String,
        decision: ApprovalDecision,
    },
    /// Swaps the model, the effort, or both for later turns.
    SetModel(ConfigPick),
    /// Switches the permission mode. Mid-turn, the switch applies from the next tool call the gate
    /// weighs.
    SetMode(Mode),
    /// Summarizes the transcript into a synthetic continuation, focused by `instructions`, and
    /// persists the compaction boundary, which [`Event::SessionCompacted`] reports.
    Compact { instructions: Option<String> },
    /// Finalizes the live session and continues in a fresh, empty one.
    Clear,
    /// Archives the live session, then continues in a fresh one like [`Self::Clear`].
    Archive,
    /// Finalizes the live session and continues `target`'s saved session in place, which
    /// [`Event::SessionResumed`] reports.
    Resume { target: ResumeTarget },
    /// Sets the session title by hand, which stops AI title generation for this session.
    Rename { title: String },
    /// Starts a browser sign-in for the oxide-code login, announced with [`Event::LoginStarted`].
    /// [`Self::Interrupt`] cancels it like a turn.
    Login,
    /// Finishes the waiting [`Self::Login`] with the `code#state` the sign-in page shows when the
    /// browser cannot reach the sign-in listener. A code from another sign-in, or an incomplete
    /// one, reports [`Event::Error`] and the sign-in keeps waiting. With no sign-in waiting, the
    /// engine ignores it.
    LoginCode(SignInCode),
    /// Removes the oxide-code login.
    Logout,
    /// Ends any running operation without a terminal event, finalizes the live session, and stops
    /// the engine. Closing the op channel does the same.
    Shutdown,
}

impl Op {
    /// An [`Op::Prompt`] whose session title comes from its own text.
    pub fn prompt(text: impl Into<String>) -> Self {
        Self::Prompt {
            text: text.into(),
            title: None,
        }
    }
}

/// The `code#state` an [`Op::LoginCode`] carries, as the user pasted it. `Debug` leaves it out, so
/// a logged op never records the code.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SignInCode(pub String);

impl fmt::Debug for SignInCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignInCode(..)")
    }
}

/// The saved session an [`Op::Resume`] continues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ResumeTarget {
    /// The session file at this path.
    Path(PathBuf),
    /// The one saved session other than the live one whose id starts with this prefix, searched for
    /// in the current project first and then in every project.
    Prefix(String),
}

/// The answer to an [`Event::ApprovalRequested`], carried back on [`Op::Resolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    /// Approve, and allow every later call `rule` matches for the rest of the session. The engine
    /// admits `rule` against its own grant for the call and denies the call when it fails.
    AllowForSession {
        rule: Box<Rule>,
    },
    Deny,
    /// Deny, with the user's `feedback` returned to the model as the call's result.
    DenyWithFeedback {
        feedback: String,
    },
    /// Deny, and end the turn as [`Op::Interrupt`] does.
    Abort,
}

/// The model, the effort, or both that an [`Op::SetModel`] picks. An omitted model keeps its
/// current value, and an omitted effort carries over where the new model supports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ConfigPick {
    /// A model, with the effort to use on it when one was picked alongside.
    Model {
        id: ResolvedModelId,
        effort: Option<Effort>,
    },
    /// An effort for the active model.
    Effort(Effort),
}

impl ConfigPick {
    /// The picked effort, if any.
    pub fn effort(&self) -> Option<Effort> {
        match *self {
            Self::Model { effort, .. } => effort,
            Self::Effort(effort) => Some(effort),
        }
    }
}

// ── Events ──

/// Token and cost snapshot for status-line rendering.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    /// Input tokens of the latest request, cache writes and reads included. `None` after a
    /// compaction, manual or automatic, until the next request reports usage, which hides the
    /// context segment.
    pub context_tokens: Option<u32>,
    /// Active model context window. `None` hides the percentage.
    pub context_window: Option<u32>,
    /// Running session cost in USD, accumulated across every request, summaries and the AI title
    /// included, and positive when present. A roll starts it over, and a resume restores it from
    /// the session file. `None` hides the cost segment.
    pub estimated_cost_usd: Option<f64>,
}

/// Notifications from the engine to a frontend, which receives them on the `events` channel of the
/// [`EngineHandle`](crate::engine::EngineHandle) it drives.
///
/// For each op submitted in the state [`Op`] requires, the engine sends one terminal event last,
/// unless [`Op::Shutdown`] stops it first:
///
/// - [`Op::Prompt`] ends with [`Self::TurnComplete`], [`Self::TurnFailed`], or [`Self::Cancelled`].
/// - [`Op::Compact`] ends with [`Self::SessionReady`], `TurnFailed`, or `Cancelled`.
/// - [`Op::Resume`] and [`Op::Archive`] end with `SessionReady` or `TurnFailed`.
/// - [`Op::Clear`] ends with `SessionReady`.
/// - [`Op::Login`] ends with [`Self::SignedIn`], `TurnFailed`, or `Cancelled`.
/// - [`Op::Logout`] ends with [`Self::SignedOut`] or `TurnFailed`.
/// - [`Op::Rename`] ends with [`Self::SessionTitleUpdated`] or [`Self::RenameFailed`].
/// - [`Op::SetModel`] ends with [`Self::ConfigChanged`].
/// - [`Op::SetMode`] ends with [`Self::ModeChanged`].
///
/// [`Op::Queue`], [`Op::Interrupt`], [`Op::Resolve`], and [`Op::LoginCode`] act on the op in flight
/// and have no terminal event of their own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// Assistant text chunk to append to the in-flight reply.
    StreamToken(String),
    /// Extended-thinking chunk rendered separately from `StreamToken` and only when the user has
    /// thinking display enabled.
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
    /// A refusal rolled back the turn and its drained prompts. The frontend returns the removed
    /// prompts to the user, then [`Self::TurnFailed`] ends the operation. After
    /// [`CompactTrigger::MidTurn`], only content after the summary is removed.
    TurnRolledBack,
    /// Automatic compaction failed before the prompt was sent. The frontend returns the prompt to
    /// the user, then [`Self::TurnFailed`] ends the operation.
    PromptUnsent,
    /// The in-flight prompt, compaction, resume, archive, sign-in, or sign-out failed. Like
    /// [`Self::TurnComplete`] and [`Self::Cancelled`], it ends the operation.
    TurnFailed(String),
    /// Live usage and estimated cost snapshot for the current process.
    UsageUpdated(UsageSnapshot),
    /// The account's subscription usage limits, which a request just reported.
    RateLimitsUpdated(RateLimits),
    /// The user cancelled a turn, compaction, or sign-in with [`Op::Interrupt`]. The in-flight
    /// reply is truncated, and frontends mark where it stopped.
    Cancelled,
    /// Automatic compaction started, ahead of the submitted prompt or at a round boundary of the
    /// turn in flight. The frontend shows compacting status while the summarizer request streams.
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
    /// The title of session `id` changed, from the background generator or an [`Op::Rename`]. A
    /// generated title can arrive after a roll, so frontends ignore one whose `id` is not live.
    SessionTitleUpdated { id: String, title: String },
    /// An [`Op::Rename`] could not record its title, so the title stays. Ends the rename as
    /// [`Self::SessionTitleUpdated`] does.
    RenameFailed(String),
    /// [`Op::Clear`] or [`Op::Archive`] started session `id`. `old_session` reports the prior
    /// session's archive state for the frontend's notice.
    SessionRolled {
        id: String,
        old_session: ArchiveState,
    },
    /// [`Op::Resume`] switched to an existing session. `history` rebuilds the frontend's chat, and
    /// `prior` reports the session's archive state before resuming made it active.
    SessionResumed {
        id: String,
        title: Option<String>,
        prior: ArchiveState,
        history: Vec<Interaction>,
    },
    /// Compaction replaced the transcript with a synthetic continuation. The frontend clears its
    /// chat and shows the summary boundary. Automatic compaction leaves the frontend busy while the
    /// pending prompt or turn continues.
    SessionCompacted {
        summary: String,
        pre_count: u32,
        instructions: Option<String>,
        trigger: CompactTrigger,
    },
    /// Ends an [`Op::Clear`], [`Op::Archive`], [`Op::Resume`], or [`Op::Compact`] that replaced the
    /// transcript. Follows the swap event, [`Self::SessionRolled`], [`Self::SessionResumed`], or
    /// [`Self::SessionCompacted`], and every event that reports the new transcript's state, such as
    /// its usage, task list, and notices.
    SessionReady,
    /// Live config after [`Op::SetModel`]. `requested` records the user's pick, `effort` its
    /// resolved value, and `notice` explains the change. Unknown models drop the requested effort.
    ConfigChanged {
        model: ModelSpec,
        effort: Option<Effort>,
        requested: ConfigPick,
        auto_compaction: AutoCompactionConfig,
        notice: String,
    },
    /// The permission mode after an [`Op::SetMode`].
    ModeChanged(Mode),
    /// Sign-in is waiting. The frontend opens `url`, which redirects to the local listener. A
    /// browser on another machine can use `manual_url` and return its code through
    /// [`Op::LoginCode`].
    LoginStarted { url: String, manual_url: String },
    /// An [`Op::Login`] stored a new oxide-code login. `auth` is the credential requests now use,
    /// which stays an env var or `client.api_key` when one outranks the login.
    SignedIn { auth: AuthSource },
    /// A stored login recovered from a transient failure. The pending prompt or compaction
    /// continues with `auth`.
    LoginRestored { auth: AuthSource },
    /// An [`Op::Logout`] finished. `removed` reports whether an oxide-code login was stored, and
    /// `auth` is the credential requests now use, `None` when none is left. `fallback_error` says
    /// why a stored Claude Code login could not take over.
    SignedOut {
        removed: bool,
        auth: Option<AuthSource>,
        fallback_error: Option<String>,
    },
    /// User-visible warning about a condition the session works around, such as tracked files that
    /// changed on disk since a resumed session read them. Renders as a notice.
    Notice(String),
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
    /// The full task list, after a `tasks` call sets it or a loaded session restores a non-empty
    /// one. Frontends empty their copy on [`Self::SessionRolled`] and [`Self::SessionResumed`].
    TasksUpdated(Vec<Task>),
    /// A background shell started, exited, or stopped. Carries every open shell. Frontends empty
    /// their copy on [`Self::SessionRolled`] and [`Self::SessionResumed`], since the session's
    /// shells end with it.
    BackgroundShellsUpdated(Vec<BackgroundShell>),
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    /// The user ran `/compact`.
    Manual,
    /// The context estimate reached the threshold ahead of a submitted prompt.
    Auto,
    /// The context reached the threshold at a round boundary inside a turn, which continues from
    /// the summary.
    MidTurn,
}

// ── Tool Calls ──

/// A tool call as the row a frontend shows for it, live in [`Event::ToolCallStart`] and resumed in
/// [`Interaction::ToolCall`]. `input` carries the model's raw arguments for consumers of the
/// serialized protocol, and `icon` / `label` are their display form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub icon: String,
    pub label: String,
}

/// A tool call's result, live in [`Event::ToolCallEnd`] and resumed in [`Interaction::ToolResult`].
/// `content` is what the model sees, `title` the tool's own header for the result, and `view` its
/// structured body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
    pub title: Option<String>,
    pub view: ToolResultView,
}

// ── History ──

/// A unit of a resumed transcript as a frontend shows it, in display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Interaction {
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

/// Transport from the engine to a frontend. `send` runs for every streamed token, so it must be
/// cheap, and it must not panic once the frontend is gone. Discard its error with `_ =` only for
/// streamed text, where losing a token is harmless.
pub trait EventSink: Send + Sync {
    fn send(&self, event: Event) -> Result<()>;

    /// Sends `event` and logs at `error!` if the channel rejects it. Use it for an event whose loss
    /// would leave the frontend inconsistent, such as an error, a cancellation, or a turn's end.
    fn emit(&self, event: Event, label: &'static str) {
        if let Err(e) = self.send(event) {
            error!(label, error = %format_args!("{e:#}"), "event dropped");
        }
    }

    /// Surfaces a session-writer failure, if any, as one [`Event::Error`].
    fn session_write_error(&self, failure: Option<&str>) {
        if let Some(failure) = failure {
            self.emit(
                Event::Error(notice::session_write_failed(failure)),
                "session-write-failed",
            );
        }
    }
}

/// Delivers to the frontend holding the receiver.
impl EventSink for mpsc::UnboundedSender<Event> {
    fn send(&self, event: Event) -> Result<()> {
        mpsc::UnboundedSender::send(self, event)
            .map_err(|_| anyhow!("the frontend stopped reading events"))
    }
}

// ── Testing ──

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, bail};
    use serde_json::json;

    use super::{Event, EventSink, ToolCall, ToolResult};
    use crate::tool::view::ToolResultView;

    /// An [`EventSink`] that keeps every event it receives. Clones share one log.
    #[derive(Clone, Default)]
    pub struct CapturingSink(Arc<Mutex<Vec<Event>>>);

    impl CapturingSink {
        pub fn events(&self) -> Vec<Event> {
            self.0
                .lock()
                .expect("the event log is not poisoned")
                .clone()
        }
    }

    impl EventSink for CapturingSink {
        fn send(&self, event: Event) -> Result<()> {
            self.0
                .lock()
                .expect("the event log is not poisoned")
                .push(event);
            Ok(())
        }
    }

    /// An [`EventSink`] whose `send` always fails, as it does once the frontend is gone.
    pub struct FailingSink;

    impl EventSink for FailingSink {
        fn send(&self, _event: Event) -> Result<()> {
            bail!("sink closed")
        }
    }

    /// The `bash` call `t1` that runs `ls`.
    pub fn ls_call() -> ToolCall {
        ToolCall {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: json!({"command": "ls"}),
            icon: "$".to_owned(),
            label: "ls".to_owned(),
        }
    }

    /// A result whose view shows `content` as plain text.
    pub fn text_result(id: &str, content: &str, is_error: bool, title: Option<&str>) -> ToolResult {
        ToolResult {
            id: id.to_owned(),
            content: content.to_owned(),
            is_error,
            title: title.map(ToOwned::to_owned),
            view: ToolResultView::Text {
                content: content.to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testing::{ls_call, text_result};
    use super::*;
    use crate::client::testing::five_hour_limit;
    use crate::permission::testing::exec_grant;
    use crate::permission::{PermissionClass, Tier};
    use crate::tool::bash::background::testing::shells_in_each_state;
    use crate::tool::tasks::testing::plan;
    use crate::tool::testing::bash_preview;
    use crate::tool::view::{
        ApprovalBody, DiffChunk, EditDiff, GrepFileGroup, GrepMatchLine, NumberedLine,
    };

    mod op {
        use super::*;

        fn op_tag(op: &Op) -> &'static str {
            match op {
                Op::Prompt { .. } => "prompt",
                Op::Queue(_) => "queue",
                Op::Interrupt => "interrupt",
                Op::Resolve { .. } => "resolve",
                Op::SetModel(_) => "set_model",
                Op::SetMode(_) => "set_mode",
                Op::Compact { .. } => "compact",
                Op::Clear => "clear",
                Op::Archive => "archive",
                Op::Resume { .. } => "resume",
                Op::Rename { .. } => "rename",
                Op::Login => "login",
                Op::LoginCode(_) => "login_code",
                Op::Logout => "logout",
                Op::Shutdown => "shutdown",
            }
        }

        fn every_op() -> Vec<Op> {
            vec![
                Op::prompt("hi"),
                Op::Prompt {
                    text: "Create AGENTS.md".to_owned(),
                    title: Some("/init AGENTS.md".to_owned()),
                },
                Op::Queue("next".to_owned()),
                Op::Interrupt,
                Op::Resolve {
                    id: "t1".to_owned(),
                    decision: ApprovalDecision::Approve,
                },
                Op::Resolve {
                    id: "t1".to_owned(),
                    decision: ApprovalDecision::AllowForSession {
                        rule: Box::new(Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()),
                    },
                },
                Op::Resolve {
                    id: "t1".to_owned(),
                    decision: ApprovalDecision::Deny,
                },
                Op::Resolve {
                    id: "t1".to_owned(),
                    decision: ApprovalDecision::DenyWithFeedback {
                        feedback: "Use rg instead.".to_owned(),
                    },
                },
                Op::Resolve {
                    id: "t1".to_owned(),
                    decision: ApprovalDecision::Abort,
                },
                Op::SetModel(ConfigPick::Model {
                    id: ResolvedModelId::new("claude-opus-5-5"),
                    effort: Some(Effort::High),
                }),
                Op::SetModel(ConfigPick::Effort(Effort::Low)),
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
                Op::LoginCode(SignInCode("code#state".to_owned())),
                Op::Logout,
                Op::Shutdown,
            ]
        }

        #[test]
        fn hides_a_sign_in_code_from_debug_output() {
            let op = Op::LoginCode(SignInCode("secret-code#state".to_owned()));

            let debug = format!("{op:?}");

            assert_eq!(debug, "LoginCode(SignInCode(..))");
        }

        #[test]
        fn round_trips_every_variant_under_its_type_tag() {
            for op in every_op() {
                let value = serde_json::to_value(&op).unwrap();
                assert_eq!(value["type"], op_tag(&op), "{value}");
                assert_eq!(serde_json::from_value::<Op>(value).unwrap(), op);
            }
        }

        #[test]
        fn carries_its_payload_under_data() {
            assert_eq!(
                serde_json::to_value(Op::Queue("next".to_owned())).unwrap(),
                json!({"type": "queue", "data": "next"}),
            );
            assert_eq!(
                serde_json::to_value(Op::Prompt {
                    text: "Create AGENTS.md".to_owned(),
                    title: Some("/init AGENTS.md".to_owned())
                })
                .unwrap(),
                json!({"type": "prompt", "data": {"text": "Create AGENTS.md", "title": "/init AGENTS.md"}}),
            );
            assert_eq!(
                serde_json::to_value(Op::Resolve {
                    id: "t1".to_owned(),
                    decision: ApprovalDecision::Deny,
                })
                .unwrap(),
                json!({"type": "resolve", "data": {"id": "t1", "decision": {"type": "deny"}}}),
            );
            assert_eq!(
                serde_json::to_value(ApprovalDecision::AllowForSession {
                    rule: Box::new(Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()),
                })
                .unwrap(),
                json!({
                    "type": "allow_for_session",
                    "data": {"rule": {"class": "exec", "pattern": "cargo test *"}},
                }),
            );
            assert_eq!(
                serde_json::to_value(ApprovalDecision::DenyWithFeedback {
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
    }

    mod event {
        use super::*;

        fn diff_chunk() -> DiffChunk {
            DiffChunk {
                old: vec![NumberedLine {
                    number: 3,
                    text: "old".to_owned(),
                }],
                new: vec![NumberedLine {
                    number: 3,
                    text: "new".to_owned(),
                }],
            }
        }

        fn edit_diff() -> EditDiff {
            EditDiff {
                chunk: diff_chunk(),
                locations: vec![3],
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
                    lines: vec![NumberedLine {
                        number: 1,
                        text: "fn main() {}".to_owned(),
                    }],
                    total_lines: 1,
                },
                ToolResultView::Diff(edit_diff()),
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
                Event::TurnRolledBack => "turn_rolled_back",
                Event::PromptUnsent => "prompt_unsent",
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
                Event::SessionReady => "session_ready",
                Event::ConfigChanged { .. } => "config_changed",
                Event::ModeChanged(_) => "mode_changed",
                Event::LoginStarted { .. } => "login_started",
                Event::SignedIn { .. } => "signed_in",
                Event::LoginRestored { .. } => "login_restored",
                Event::SignedOut { .. } => "signed_out",
                Event::Notice(_) => "notice",
                Event::Error(_) => "error",
                Event::ApprovalRequested { .. } => "approval_requested",
                Event::SessionRulesUpdated(_) => "session_rules_updated",
                Event::TasksUpdated(_) => "tasks_updated",
                Event::BackgroundShellsUpdated(_) => "background_shells_updated",
            }
        }

        /// One approval per [`ApprovalBody`] variant, each held by a different basis.
        fn approval_requests() -> [Event; 2] {
            [
                Event::ApprovalRequested {
                    id: "t2".to_owned(),
                    preview: ApprovalPreview {
                        title: "Edit".to_owned(),
                        body: ApprovalBody::Diff(diff_chunk()),
                    },
                    basis: Basis::Mode(Mode::Manual),
                    grant: None,
                },
                Event::ApprovalRequested {
                    id: "t3".to_owned(),
                    preview: bash_preview("cargo test"),
                    basis: Basis::Rule {
                        tier: Tier::Ask,
                        rule: Rule::parse(PermissionClass::Exec, "cargo *").unwrap(),
                    },
                    grant: Some(Box::new(exec_grant("cargo test"))),
                },
            ]
        }

        /// Every [`Event`] variant, plus one approval per [`ApprovalBody`] variant.
        fn every_event() -> Vec<Event> {
            let mut events = vec![
                Event::StreamToken("hello".to_owned()),
                Event::ThinkingToken("hmm".to_owned()),
                Event::ProgressUpdate("Reading.".to_owned()),
                Event::Retrying("Retrying in 1 s.".to_owned()),
                Event::ToolCallStart(ls_call()),
                Event::ToolCallEnd(text_result("t1", "ok", true, None)),
                Event::PromptDrained("next".to_owned()),
                Event::TurnComplete,
                Event::TurnRolledBack,
                Event::PromptUnsent,
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
                    id: "s1".to_owned(),
                    title: "Title".to_owned(),
                },
                Event::RenameFailed("Session write failed: disk full".to_owned()),
                Event::SessionRolled {
                    id: "s2".to_owned(),
                    old_session: ArchiveState::Archived,
                },
                Event::SessionResumed {
                    id: "s3".to_owned(),
                    title: Some("Resumed".to_owned()),
                    prior: ArchiveState::Archived,
                    history: full_history(),
                },
                Event::SessionCompacted {
                    summary: "Summary.".to_owned(),
                    pre_count: 4,
                    instructions: Some("focus".to_owned()),
                    trigger: CompactTrigger::Auto,
                },
                Event::SessionReady,
                Event::ConfigChanged {
                    model: ModelSpec::resolve("claude-opus-5-5"),
                    effort: Some(Effort::High),
                    requested: ConfigPick::Effort(Effort::Max),
                    auto_compaction: AutoCompactionConfig {
                        enabled: true,
                        threshold_tokens: Some(400_000),
                    },
                    notice: "Effort set to high".to_owned(),
                },
                Event::ModeChanged(Mode::Yolo),
                Event::LoginStarted {
                    url: "https://claude.ai/oauth/authorize?state=s".to_owned(),
                    manual_url: "https://claude.ai/oauth/authorize?code=true&state=s".to_owned(),
                },
                Event::SignedIn {
                    auth: AuthSource::OxLogin,
                },
                Event::LoginRestored {
                    auth: AuthSource::ClaudeCodeLogin,
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
                Event::Notice("1 tracked file changed on disk".to_owned()),
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

        #[test]
        fn round_trips_every_variant_under_its_type_tag() {
            for event in every_event() {
                let value = serde_json::to_value(&event).unwrap();
                assert_eq!(value["type"], event_tag(&event), "{value}");
                assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);
            }
        }

        #[test]
        fn carries_nested_payloads_tagged_the_same_way() {
            let event = Event::ToolCallEnd(ToolResult {
                id: "t1".to_owned(),
                content: "Edited.".to_owned(),
                is_error: false,
                title: Some("Edited f.rs".to_owned()),
                view: ToolResultView::Diff(edit_diff()),
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
                                "chunk": {
                                    "old": [{"number": 3, "text": "old"}],
                                    "new": [{"number": 3, "text": "new"}],
                                },
                                "locations": [3],
                            },
                        },
                    },
                }),
            );
        }

        #[test]
        fn carries_a_task_list_as_content_and_status_pairs() {
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
        fn carries_each_background_shell_with_its_state_tagged() {
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
        fn serializes_a_model_as_its_id() {
            let event = Event::ConfigChanged {
                model: ModelSpec::resolve("claude-opus-5-5"),
                effort: None,
                requested: ConfigPick::Model {
                    id: ResolvedModelId::new("claude-opus-5-5"),
                    effort: None,
                },
                auto_compaction: AutoCompactionConfig::disabled(),
                notice: "Using Claude Opus 5.5 (claude-opus-5-5)".to_owned(),
            };
            let value = serde_json::to_value(&event).unwrap();
            let parsed = serde_json::from_value(value.clone()).unwrap();

            assert_eq!(value["data"]["model"], "claude-opus-5-5");
            let Event::ConfigChanged { model, .. } = parsed else {
                panic!("config_changed must round-trip to ConfigChanged");
            };
            assert_eq!(model.short_display_name(), "Opus 5.5");
        }

        #[test]
        fn tags_an_auth_source_like_its_own_variants() {
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

    mod compact_trigger {
        use super::*;

        #[test]
        fn tags_each_compaction_trigger_as_headless_output_names_it() {
            let triggers = [
                CompactTrigger::Manual,
                CompactTrigger::Auto,
                CompactTrigger::MidTurn,
            ];

            let tags = serde_json::to_value(triggers).unwrap();

            assert_eq!(tags, json!(["manual", "auto", "mid_turn"]));
        }
    }

    mod unbounded_sender {
        use super::*;

        mod send {
            use super::*;

            #[test]
            fn fails_once_the_frontend_drops_the_receiver() {
                let (tx, rx) = mpsc::unbounded_channel();
                drop(rx);

                let err = EventSink::send(&tx, Event::TurnComplete).unwrap_err();

                assert_eq!(err.to_string(), "the frontend stopped reading events");
            }
        }
    }
}
