//! The protocol between the engine and its frontends: the [`Op`]s a frontend sends, the [`Event`]s
//! the engine emits, and the [`EventSink`] that delivers them. Ops and events travel in-process,
//! while the payload types they carry serialize to JSON, as the saved session format and the `ox`
//! crate's headless wire module use. That module restates each documented event to define the
//! `stream-json` output, so renaming a payload field in an event listed in `docs/guide/headless.md`
//! breaks the stability surface.

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
/// [`Self::SetArchive`], [`Self::Resume`], [`Self::Rename`], [`Self::Login`], and [`Self::Logout`]
/// require an idle engine. Frontends wait for the preceding operation's terminal event before
/// submitting them. An idle-only op received during another operation is dropped without a
/// terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Sets the archive state of the saved session at `path`. `session_id` identifies that file.
    /// Completion arrives through [`Event::ArchiveUpdated`] before the next idle operation runs.
    SetArchive {
        path: PathBuf,
        session_id: String,
        state: ArchiveState,
    },
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
/// - [`Op::SetArchive`] ends with [`Self::ArchiveUpdated`].
/// - [`Op::Clear`] ends with `SessionReady`.
/// - [`Op::Login`] ends with [`Self::SignedIn`], `TurnFailed`, or `Cancelled`.
/// - [`Op::Logout`] ends with [`Self::SignedOut`] or `TurnFailed`.
/// - [`Op::Rename`] ends with [`Self::SessionTitleUpdated`] or [`Self::RenameFailed`].
/// - [`Op::SetModel`] ends with [`Self::ConfigChanged`].
/// - [`Op::SetMode`] ends with [`Self::ModeChanged`].
///
/// [`Op::Queue`], [`Op::Interrupt`], [`Op::Resolve`], and [`Op::LoginCode`] act on the op in flight
/// and have no terminal event of their own.
#[derive(Debug, Clone, PartialEq)]
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
    /// The engine did not record the prompt, because a failure or a cancel stopped the operation
    /// first: automatic compaction failed, or sign-in failed or was interrupted. The frontend
    /// returns the prompt to the user, then the operation's terminal event follows.
    PromptUnsent,
    /// The in-flight prompt, compaction, resume, archive, sign-in, or sign-out failed. Like
    /// [`Self::TurnComplete`] and [`Self::Cancelled`], it ends the operation.
    TurnFailed(String),
    /// Live usage and estimated cost snapshot for the current process.
    UsageUpdated(UsageSnapshot),
    /// The account's subscription usage limits, which a request just reported.
    RateLimitsUpdated(RateLimits),
    /// The user cancelled a turn, compaction, or sign-in with [`Op::Interrupt`]. A turn that had
    /// recorded the prompt truncates the reply, and frontends mark where it stopped. One that had
    /// not emits [`Self::PromptUnsent`] first.
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
    /// The saved session's archive operation completed. The error is a display-ready sentence.
    ArchiveUpdated {
        id: String,
        result: Result<ArchiveState, String>,
    },
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
    /// which stays an env var or `client.auth` when one outranks the login.
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

/// How an operation ended, after the sink has already shown it. A headless run reports it as its
/// stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    Completed,
    Cancelled,
    Failed,
    Shutdown,
}

impl TurnEnd {
    /// How `op` ended, if `event` is one of the terminal events [`Event`] lists for it.
    pub fn of(op: &Op, event: &Event) -> Option<Self> {
        match (op, event) {
            (Op::SetArchive { .. }, Event::ArchiveUpdated { result, .. }) => Some(if result.is_ok() {
                Self::Completed
            } else {
                Self::Failed
            }),
            (
                Op::Prompt { .. }
                | Op::Compact { .. }
                | Op::Archive
                | Op::Resume { .. }
                | Op::Login
                | Op::Logout,
                Event::TurnFailed(_),
            )
            | (Op::Rename { .. }, Event::RenameFailed(_))
            // The model has no level for the effort, so the pick changed nothing.
            | (Op::SetModel(ConfigPick::Effort(_)), Event::ConfigChanged { effort: None, .. }) => {
                Some(Self::Failed)
            }
            (Op::Prompt { .. }, Event::TurnComplete)
            | (
                Op::Compact { .. } | Op::Clear | Op::Archive | Op::Resume { .. },
                Event::SessionReady,
            )
            | (Op::Rename { .. }, Event::SessionTitleUpdated { .. })
            | (Op::Login, Event::SignedIn { .. })
            | (Op::Logout, Event::SignedOut { .. })
            | (Op::SetModel(_), Event::ConfigChanged { .. })
            | (Op::SetMode(_), Event::ModeChanged(_)) => Some(Self::Completed),
            (Op::Prompt { .. } | Op::Compact { .. } | Op::Login, Event::Cancelled) => {
                Some(Self::Cancelled)
            }
            _ => None,
        }
    }
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

    use super::*;
    use crate::config::compaction::CompactionConfig;

    mod op {
        use super::*;

        #[test]
        fn hides_a_sign_in_code_from_debug_output() {
            let op = Op::LoginCode(SignInCode("secret-code#state".to_owned()));

            let debug = format!("{op:?}");

            assert_eq!(debug, "LoginCode(SignInCode(..))");
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

    mod turn_end {
        use super::*;

        mod of {
            use super::*;

            #[test]
            fn saved_archive_ends_on_its_completion_result() {
                let op = Op::SetArchive {
                    path: "/repo/saved.jsonl".into(),
                    session_id: "saved".to_owned(),
                    state: ArchiveState::Archived,
                };

                for (result, expected) in [
                    (Ok(ArchiveState::Archived), TurnEnd::Completed),
                    (Err("Archive failed".to_owned()), TurnEnd::Failed),
                ] {
                    assert_eq!(
                        TurnEnd::of(
                            &op,
                            &Event::ArchiveUpdated {
                                id: "saved".to_owned(),
                                result
                            }
                        ),
                        Some(expected)
                    );
                }
            }

            #[test]
            fn reads_only_the_terminal_events_of_the_op_in_flight() {
                let prompt = Op::prompt("hi");
                let compact = Op::Compact { instructions: None };
                let resume = Op::Resume {
                    target: ResumeTarget::Prefix("1a2b".to_owned()),
                };
                let rename = Op::Rename {
                    title: "Fix login".to_owned(),
                };
                let set_model = Op::SetModel(ConfigPick::Effort(Effort::Low));
                let set_mode = Op::SetMode(Mode::Plan);
                let compacted = |trigger| Event::SessionCompacted {
                    summary: "summary".to_owned(),
                    pre_count: 4,
                    instructions: None,
                    trigger,
                };
                let rolled = Event::SessionRolled {
                    id: "new".to_owned(),
                    old_session: ArchiveState::Active,
                };
                let notice = Event::Error("Session write failed".to_owned());
                let failed = Event::TurnFailed("failure".to_owned());

                for (op, event, end) in [
                    (&prompt, Event::TurnComplete, Some(TurnEnd::Completed)),
                    (&prompt, Event::Cancelled, Some(TurnEnd::Cancelled)),
                    (&prompt, failed.clone(), Some(TurnEnd::Failed)),
                    (&prompt, compacted(CompactTrigger::Auto), None),
                    (&prompt, notice.clone(), None),
                    (&compact, compacted(CompactTrigger::Manual), None),
                    (&compact, Event::SessionReady, Some(TurnEnd::Completed)),
                    (&compact, Event::Cancelled, Some(TurnEnd::Cancelled)),
                    (&compact, Event::TurnComplete, None),
                    (
                        &resume,
                        Event::SessionResumed {
                            id: "1a2b".to_owned(),
                            title: None,
                            prior: ArchiveState::Active,
                            history: Vec::new(),
                        },
                        None,
                    ),
                    (&resume, Event::SessionReady, Some(TurnEnd::Completed)),
                    (&resume, failed.clone(), Some(TurnEnd::Failed)),
                    (&Op::Clear, notice.clone(), None),
                    (&Op::Clear, rolled.clone(), None),
                    (&Op::Clear, Event::SessionReady, Some(TurnEnd::Completed)),
                    (
                        &rename,
                        Event::SessionTitleUpdated {
                            id: "live".to_owned(),
                            title: "Fix login".to_owned(),
                        },
                        Some(TurnEnd::Completed),
                    ),
                    (&rename, notice.clone(), None),
                    (
                        &rename,
                        Event::RenameFailed("Session write failed".to_owned()),
                        Some(TurnEnd::Failed),
                    ),
                    (
                        &set_model,
                        Event::ConfigChanged {
                            model: ModelSpec::resolve("claude-opus-5-5"),
                            effort: Some(Effort::Low),
                            requested: ConfigPick::Effort(Effort::Low),
                            auto_compaction: CompactionConfig::with_threshold(50_000).auto,
                            notice: String::new(),
                        },
                        Some(TurnEnd::Completed),
                    ),
                    (
                        &set_model,
                        Event::ConfigChanged {
                            model: ModelSpec::resolve("custom-model"),
                            effort: None,
                            requested: ConfigPick::Effort(Effort::Low),
                            auto_compaction: CompactionConfig::with_threshold(50_000).auto,
                            notice: String::new(),
                        },
                        Some(TurnEnd::Failed),
                    ),
                    (&set_model, failed.clone(), None),
                    (
                        &set_mode,
                        Event::ModeChanged(Mode::Plan),
                        Some(TurnEnd::Completed),
                    ),
                    (&set_mode, notice.clone(), None),
                    (&Op::Archive, rolled, None),
                    (&Op::Archive, Event::SessionReady, Some(TurnEnd::Completed)),
                    (&Op::Archive, failed, Some(TurnEnd::Failed)),
                ] {
                    assert_eq!(TurnEnd::of(op, &event), end, "{op:?} then {event:?}");
                }
            }

            #[test]
            fn reads_the_sign_in_terminal_events() {
                let failed = Event::TurnFailed("failure".to_owned());

                for (op, event, end) in [
                    (
                        &Op::Login,
                        Event::LoginStarted {
                            url: "https://example.com/login".to_owned(),
                            manual_url: "https://example.com/login?manual".to_owned(),
                        },
                        None,
                    ),
                    (
                        &Op::Login,
                        Event::SignedIn {
                            auth: AuthSource::ApiKeyEnv,
                        },
                        Some(TurnEnd::Completed),
                    ),
                    (&Op::Login, Event::Cancelled, Some(TurnEnd::Cancelled)),
                    (&Op::Login, failed.clone(), Some(TurnEnd::Failed)),
                    (
                        &Op::Logout,
                        Event::SignedOut {
                            removed: true,
                            auth: None,
                            fallback_error: None,
                        },
                        Some(TurnEnd::Completed),
                    ),
                    (&Op::Logout, Event::Cancelled, None),
                    (&Op::Logout, failed, Some(TurnEnd::Failed)),
                ] {
                    assert_eq!(TurnEnd::of(op, &event), end, "{op:?} then {event:?}");
                }
            }
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
