//! The protocol between the engine and its frontends: the [`Op`]s a frontend submits through an
//! [`OpSender`], the [`Event`]s the engine emits, and the [`EventSink`] that delivers them. Ops and
//! events travel in-process, while the payload types they carry serialize to JSON, as the saved
//! session format and the `ox` crate's headless wire module use. That module restates each
//! documented event to define the `stream-json` output, so renaming a payload field in an event
//! listed in `docs/guide/headless.md` breaks the stability surface.

pub mod notice;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use serde::Serialize;
use strum::IntoStaticStr;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tracing::error;

use crate::auth::AuthSource;
use crate::client::RateLimits;
use crate::config::compaction::AutoCompactionConfig;
use crate::model::{Effort, ModelSpec};
use crate::permission::rule::Rule;
use crate::permission::{Basis, Mode, SessionGrant};
use crate::session::handle::WriteReport;
use crate::session::saved::{ArchiveUpdate, SavedError};
use crate::session::store::listing::{ArchiveState, SessionInfo};
use crate::tool::bash::background::BackgroundShell;
use crate::tool::tasks::Task;
use crate::tool::view::{ApprovalPreview, ToolResultView};

// ── Ops ──

/// Commands from a frontend to the engine. Multiplexed onto a single `mpsc`, so a running turn can
/// race them against its in-flight stream and tool futures with one biased `select!`.
///
/// Each op the engine takes while idle ends with one [`Event::OpEnded`] under its [`OpId`]. The ops
/// [`Self::is_idle_only`] names require an idle engine, so frontends wait for the preceding
/// operation's `OpEnded` before submitting them. An idle-only op received during another operation
/// is dropped, and the other ops act on the operation in flight, so neither has an `OpEnded`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Starts a turn with `text`. On a fresh session, `title` supplies the first-prompt title, so a
    /// synthesized slash-command prompt can retain the typed command as its title.
    Prompt { text: String, title: Option<String> },
    /// Adds a prompt typed while a turn runs. The turn splices it in at its next round boundary and
    /// confirms with [`Event::PromptDrained`]. The frontend keeps every unconfirmed prompt and
    /// submits it later, so the engine drops one that arrives after its turn ended.
    Queue(String),
    /// Cancels the running turn, compaction, or sign-in, which then ends with [`OpEnd::Cancelled`].
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
    /// Sets the archive state of the saved session `target` names, which the engine resolves and
    /// reports through [`Event::ArchiveUpdated`].
    SetArchive {
        target: SavedTarget,
        state: ArchiveState,
    },
    /// Looks up the one saved session other than the live one whose ID starts with `prefix`, as
    /// [`SavedTarget::Prefix`] does, and reports it through [`Event::SavedFound`], so a frontend
    /// can confirm a [`Self::DeleteSaved`] against what the engine resolved.
    FindSaved { prefix: String },
    /// Deletes saved session `session_id` with its artifacts, which [`Event::SavedDeleted`]
    /// reports. The live session is refused.
    DeleteSaved { session_id: String },
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
    /// Ends any running operation without its [`Event::OpEnded`], finalizes the live session, and
    /// stops the engine. Closing the op channel does the same.
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

    /// Returns whether the op requires an idle engine, so a frontend must not send it before the
    /// preceding operation's [`Event::OpEnded`].
    pub fn is_idle_only(&self) -> bool {
        match self {
            Self::Prompt { .. }
            | Self::SetModel(_)
            | Self::Compact { .. }
            | Self::Clear
            | Self::Archive
            | Self::SetArchive { .. }
            | Self::FindSaved { .. }
            | Self::DeleteSaved { .. }
            | Self::Resume { .. }
            | Self::Rename { .. }
            | Self::Login
            | Self::Logout => true,
            Self::Queue(_)
            | Self::Interrupt
            | Self::Resolve { .. }
            | Self::LoginCode(_)
            | Self::SetMode(_)
            | Self::Shutdown => false,
        }
    }
}

/// The `code#state` an [`Op::LoginCode`] carries, as the user pasted it. `Debug` leaves it out, so
/// a logged op never records the code.
#[derive(Clone, PartialEq, Eq)]
pub struct SignInCode(pub String);

impl fmt::Debug for SignInCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignInCode(..)")
    }
}

/// The session an [`Op::Resume`] or `ox --resume` continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTarget {
    /// The session that the export at this path holds, imported into the store first.
    Path(PathBuf),
    /// The saved session with this ID.
    Id(String),
    /// The one saved session other than the live one whose ID starts with this prefix, searched for
    /// in the current project first and then in every project.
    Prefix(String),
}

/// The saved session an [`Op::SetArchive`] acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedTarget {
    /// The saved session with this ID.
    Id(String),
    /// The one saved session other than the live one whose ID starts with this prefix, searched for
    /// in the current project first and then in every project.
    Prefix(String),
}

/// The answer to an [`Event::ApprovalRequested`], carried back on [`Op::Resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigPick {
    /// A model, with the effort to use on it when one was picked alongside. The engine accepts
    /// any ID and runs an unknown one without catalog info.
    Model { id: String, effort: Option<Effort> },
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct UsageSnapshot {
    /// Input tokens of the latest request, cache writes and reads included. `None` after a
    /// compaction, manual or automatic, until the next request reports usage, which hides the
    /// context segment.
    pub context_tokens: Option<u32>,
    /// Active model context window. `None` hides the percentage.
    pub context_window: Option<u32>,
    /// Running session cost in USD, accumulated across every request, summaries and the AI title
    /// included, and positive when present. A roll starts it over, and a resume restores it from
    /// the session's usage records. `None` hides the cost segment.
    pub estimated_cost_usd: Option<f64>,
}

/// Notifications from the engine to a frontend, which receives them on the `events` channel of the
/// [`EngineHandle`](crate::engine::EngineHandle) it drives.
///
/// Each op the engine takes while idle ends with one [`Self::OpEnded`], the last event the op
/// emits while it runs, unless [`Op::Shutdown`] stops the engine first. Background work the op
/// started, such as the AI title or a background shell, may report afterward. The op's results
/// come before its end, such as [`Self::SessionResumed`] or [`Self::ArchiveUpdated`], and a
/// failure reports its cause there too: [`Self::TurnFailed`], [`Self::RenameFailed`], the error an
/// event such as `ArchiveUpdated` carries, or the notice of a [`Self::ConfigChanged`] that dropped
/// the effort.
#[derive(Debug, Clone, PartialEq, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
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
    /// Op `id` ended with `end`, after every other event the op emitted while it ran. `end` is
    /// never [`OpEnd::Shutdown`], since a shutdown stops the engine without one. A turn cancelled
    /// after it recorded its prompt truncates the reply, so frontends mark where it stopped.
    OpEnded { id: OpId, end: OpEnd },
    /// A refusal rolled back the turn and its drained prompts. The frontend returns the removed
    /// prompts to the user, then [`Self::TurnFailed`] reports the refusal. After
    /// [`CompactTrigger::MidTurn`], only content after the summary is removed.
    TurnRolledBack,
    /// The engine did not record the prompt, because a failure or a cancel stopped the operation
    /// first: automatic compaction failed, or sign-in failed or was interrupted. The frontend
    /// returns the prompt to the user, then the operation's [`Self::OpEnded`] follows.
    PromptUnsent,
    /// Why the in-flight prompt, compaction, resume, archive, sign-in, or sign-out failed. The
    /// operation's [`Self::OpEnded`] follows with [`OpEnd::Failed`].
    TurnFailed(String),
    /// Live usage and estimated cost snapshot for the current process.
    UsageUpdated(UsageSnapshot),
    /// The account's subscription usage limits, which a request just reported.
    RateLimitsUpdated(RateLimits),
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
    /// Why an [`Op::Rename`] could not record its title, so the title stays. The rename's
    /// [`Self::OpEnded`] follows with [`OpEnd::Failed`].
    RenameFailed(String),
    /// An [`Op::SetArchive`] for `target` and `state` completed. Frontends phrase the typed error.
    ArchiveUpdated {
        target: SavedTarget,
        state: ArchiveState,
        result: Result<ArchiveUpdate, SavedError>,
    },
    /// An [`Op::FindSaved`] resolved its prefix.
    SavedFound(Result<Box<SessionInfo>, SavedError>),
    /// An [`Op::DeleteSaved`] of session `id` completed.
    SavedDeleted {
        id: String,
        result: Result<(), SavedError>,
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
    /// Another process wrote to session `from` since this one last saw it, so the conversation
    /// continues as session `id`, a fork holding what this process saw. Leaves any in-flight
    /// operation running, and frontends show [`notice::session_forked`].
    SessionForked { id: String, from: String },
    /// Compaction replaced the transcript with a synthetic continuation. The frontend clears its
    /// chat and shows the summary boundary. Automatic compaction leaves the frontend busy while the
    /// pending prompt or turn continues.
    SessionCompacted {
        summary: String,
        pre_count: u32,
        instructions: Option<String>,
        trigger: CompactTrigger,
    },
    /// Live config after [`Op::SetModel`]. `requested` records the user's pick, `effort` its
    /// resolved value, and `notice` explains the change. Unknown models drop the requested effort,
    /// so an effort-only pick on one ends with [`OpEnd::Failed`].
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

impl Event {
    /// How this event ends op `id`, if it is that op's [`Self::OpEnded`].
    pub fn end_of(&self, id: OpId) -> Option<OpEnd> {
        match self {
            Self::OpEnded { id: ended, end } if *ended == id => Some(*end),
            _ => None,
        }
    }
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// How an op ended, which [`Event::OpEnded`] reports. `OpEnded` never carries `Shutdown`, which
/// the engine returns when a shutdown stops an op and a headless run reports as its stop reason
/// when a signal stops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpEnd {
    Completed,
    Cancelled,
    Failed,
    Shutdown,
}

impl OpEnd {
    /// `Completed` for an op that succeeded, or `Failed`.
    pub(crate) const fn settled(ok: bool) -> Self {
        if ok { Self::Completed } else { Self::Failed }
    }
}

// ── Tool Calls ──

/// A tool call as the row a frontend shows for it, live in [`Event::ToolCallStart`] and resumed in
/// [`Interaction::ToolCall`]. `input` carries the model's raw arguments for consumers of the
/// serialized protocol, and `icon` / `label` are their display form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
    pub title: Option<String>,
    pub view: ToolResultView,
}

// ── History ──

/// A unit of a resumed transcript as a frontend shows it, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Where the user interrupted a turn. Renders like a live turn's [`OpEnd::Cancelled`].
    Interrupted,
}

// ── Op Sender ──

/// The number an [`OpSender`] gives each op it submits, unique among one engine's submissions. The
/// op's [`Event::OpEnded`] carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpId(u64);

impl OpId {
    fn take(next_id: &AtomicU64) -> Self {
        Self(next_id.fetch_add(1, Ordering::Relaxed))
    }
}

/// An op on its way to the engine, under the ID its sender assigned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Submission {
    pub(crate) id: OpId,
    pub(crate) op: Op,
}

/// The frontend's end of the engine's op channel. Each submission takes the next [`OpId`], and
/// clones share the numbering.
#[derive(Debug, Clone)]
pub struct OpSender {
    tx: mpsc::Sender<Submission>,
    next_id: Arc<AtomicU64>,
}

impl OpSender {
    /// Opens a channel that holds up to `capacity` submissions the engine has not read.
    pub(crate) fn channel(capacity: usize) -> (Self, mpsc::Receiver<Submission>) {
        let (tx, rx) = mpsc::channel(capacity);
        let sender = Self {
            tx,
            next_id: Arc::new(AtomicU64::new(0)),
        };
        (sender, rx)
    }

    /// Waits for room and submits `op`, or hands it back once the engine has stopped.
    pub async fn submit(&self, op: Op) -> Result<OpId, SendError<Op>> {
        let Ok(permit) = self.tx.reserve().await else {
            return Err(SendError(op));
        };
        let id = OpId::take(&self.next_id);
        permit.send(Submission { id, op });
        Ok(id)
    }

    /// Submits `op` if the channel has room, or hands it back.
    pub fn try_submit(&self, op: Op) -> Result<OpId, TrySendError<Op>> {
        match self.tx.try_reserve() {
            Ok(permit) => {
                let id = OpId::take(&self.next_id);
                permit.send(Submission { id, op });
                Ok(id)
            }
            Err(TrySendError::Full(())) => Err(TrySendError::Full(op)),
            Err(TrySendError::Closed(())) => Err(TrySendError::Closed(op)),
        }
    }

    /// Holds room for one submission and takes its [`OpId`], so the caller can update its own
    /// state under that ID before it submits. A dropped permit leaves its ID unused.
    pub fn try_reserve(&self) -> Result<OpPermit, TrySendError<()>> {
        let permit = self
            .tx
            .clone()
            .try_reserve_owned()
            .map_err(|error| match error {
                TrySendError::Full(_) => TrySendError::Full(()),
                TrySendError::Closed(_) => TrySendError::Closed(()),
            })?;
        Ok(OpPermit {
            permit,
            id: OpId::take(&self.next_id),
        })
    }
}

/// Room for one submission that [`OpSender::try_reserve`] holds, under the ID it took.
#[derive(Debug)]
pub struct OpPermit {
    permit: mpsc::OwnedPermit<Submission>,
    id: OpId,
}

impl OpPermit {
    /// The ID the held submission takes.
    pub const fn id(&self) -> OpId {
        self.id
    }

    /// Submits `op` into the held room, which cannot fail.
    pub fn submit(self, op: Op) {
        self.permit.send(Submission { id: self.id, op });
    }
}

// ── Event Sink ──

/// Transport from the engine to a frontend. `send` runs for every streamed token, so it must be
/// cheap, and it must not panic once the frontend is gone. Discard its error with `_ =` only for
/// streamed text, where losing a token is harmless.
pub(crate) trait EventSink: Send + Sync {
    fn send(&self, event: Event) -> Result<()>;

    /// Sends `event` and logs its variant at `error!` if the channel rejects it. Use it for an
    /// event whose loss would leave the frontend inconsistent, such as an error or an op's end.
    fn emit(&self, event: Event) {
        let label: &'static str = (&event).into();
        if let Err(e) = self.send(event) {
            error!(label, error = %format_args!("{e:#}"), "event dropped");
        }
    }

    /// Surfaces a session write's failure as [`Event::Error`] and its fork as
    /// [`Event::SessionForked`], each when present.
    fn session_write(&self, report: &WriteReport) {
        if let Some(failure) = &report.failure {
            self.emit(Event::Error(notice::session_write_failed(failure)));
        }
        if let Some(fork) = &report.fork {
            self.emit(Event::SessionForked {
                id: fork.to.clone(),
                from: fork.from.clone(),
            });
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

#[cfg(test)]
pub(crate) mod local_testing {
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, bail};

    use super::{Event, EventSink};

    /// An [`EventSink`] that keeps every event it receives. Clones share one log.
    #[derive(Clone, Default)]
    pub(crate) struct CapturingSink(Arc<Mutex<Vec<Event>>>);

    impl CapturingSink {
        pub(crate) fn events(&self) -> Vec<Event> {
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
    pub(crate) struct FailingSink;

    impl EventSink for FailingSink {
        fn send(&self, _event: Event) -> Result<()> {
            bail!("sink closed")
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::error::TryRecvError;

    use super::{ConfigPick, Op, OpId, OpSender, Submission, ToolCall, ToolResult};
    use crate::model::Effort;
    use crate::tool::view::ToolResultView;

    /// An op channel whose engine end, held by a test standing in for the engine, yields each op.
    pub fn op_channel(capacity: usize) -> (OpSender, OpReceiver) {
        let (ops, rx) = OpSender::channel(capacity);
        (ops, OpReceiver(rx))
    }

    /// The engine's end of an [`op_channel`], yielding the op of each submission.
    pub struct OpReceiver(mpsc::Receiver<Submission>);

    impl OpReceiver {
        pub fn try_recv(&mut self) -> Result<Op, TryRecvError> {
            self.0.try_recv().map(|submission| submission.op)
        }

        pub async fn recv(&mut self) -> Option<Op> {
            self.0.recv().await.map(|submission| submission.op)
        }

        pub fn close(&mut self) {
            self.0.close();
        }
    }

    /// The ID the `n`th submission of a fresh [`OpSender`] takes, counting from 0.
    pub const fn op_id(n: u64) -> OpId {
        OpId(n)
    }

    /// A pick of the model `id`, with `effort` when one was picked alongside.
    pub fn model_pick(id: &str, effort: Option<Effort>) -> ConfigPick {
        ConfigPick::Model {
            id: id.to_owned(),
            effort,
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
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::protocol::local_testing::FailingSink;
    use crate::util::log::testing::LogCapture;

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

    mod op_sender {
        use super::*;

        #[tokio::test]
        async fn numbers_submissions_from_every_clone_and_permit() {
            let (ops, mut rx) = OpSender::channel(4);
            let clone = ops.clone();

            let first = ops.submit(Op::Clear).await.unwrap();
            let second = clone.try_submit(Op::Interrupt).unwrap();
            let permit = ops.try_reserve().unwrap();
            let third = permit.id();
            permit.submit(Op::Logout);
            let fourth = clone.try_submit(Op::Shutdown).unwrap();

            assert_eq!(
                [first, second, third, fourth],
                [OpId(0), OpId(1), OpId(2), OpId(3)]
            );
            let received: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
                .map(|Submission { id, op }| (id, op))
                .collect();
            assert_eq!(
                received,
                [
                    (OpId(0), Op::Clear),
                    (OpId(1), Op::Interrupt),
                    (OpId(2), Op::Logout),
                    (OpId(3), Op::Shutdown),
                ],
            );
        }

        #[test]
        fn skips_a_dropped_permits_id_and_takes_none_for_a_refused_reservation() {
            let (ops, mut rx) = OpSender::channel(1);

            drop(ops.try_reserve().unwrap());
            let after_drop = ops.try_submit(Op::Clear).unwrap();

            assert_eq!(after_drop, OpId(1));

            let refused = ops.try_reserve();
            rx.try_recv().unwrap();
            let after_refusal = ops.try_submit(Op::Logout).unwrap();

            assert!(matches!(refused, Err(TrySendError::Full(()))));
            assert_eq!(after_refusal, OpId(2));
        }

        #[tokio::test]
        async fn hands_back_the_op_a_full_or_closed_channel_refuses() {
            let (ops, rx) = OpSender::channel(1);
            ops.try_submit(Op::Clear).unwrap();

            let full = ops.try_submit(Op::Logout);
            let full_reserve = ops.try_reserve();

            assert_eq!(full, Err(TrySendError::Full(Op::Logout)));
            assert!(matches!(full_reserve, Err(TrySendError::Full(()))));

            drop(rx);
            let closed = ops.try_submit(Op::Logout);
            let closed_reserve = ops.try_reserve();
            let closed_wait = ops.submit(Op::Logout).await;

            assert_eq!(closed, Err(TrySendError::Closed(Op::Logout)));
            assert!(matches!(closed_reserve, Err(TrySendError::Closed(()))));
            assert_eq!(closed_wait, Err(SendError(Op::Logout)));
        }
    }

    mod event_sink {
        use super::*;

        mod emit {
            use super::*;

            #[test]
            fn logs_a_dropped_event_under_its_kebab_case_variant() {
                let logs = LogCapture::default();

                tracing::dispatcher::with_default(&logs.dispatch(), || {
                    FailingSink.emit(Event::AutoCompactionPaused { failures: 3 });
                });

                assert_eq!(
                    logs.events(),
                    [BTreeMap::from([
                        ("error".to_owned(), "sink closed".to_owned()),
                        ("label".to_owned(), "auto-compaction-paused".to_owned()),
                        ("message".to_owned(), "event dropped".to_owned()),
                    ])],
                );
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

                let err = EventSink::send(&tx, Event::AutoCompactionStarted).unwrap_err();

                assert_eq!(err.to_string(), "the frontend stopped reading events");
            }
        }
    }
}
