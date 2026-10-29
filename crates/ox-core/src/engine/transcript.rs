//! The live session and the transcript it records, which change together.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::estimate::request_tokens;
use crate::file_tracker::{FileSnapshot, FileTracker};
use crate::message::{ContentBlock, Message, TextOrigin};
use crate::model::ModelSpec;
use crate::permission::Mode;
use crate::prompt::{self, PromptParts, SessionContext, UserContext};
use crate::protocol::SessionRef;
use crate::session::handle::{
    self, Checkpoint, CompactBoundary, CompactOutcome, LoadedTranscript, RecordOutcome,
    RollIntoOutcome, RollOutcome, SessionHandle, WriteReport,
};
use crate::session::store::{CompactInfo, SessionStore, UsageRecord};
use crate::tool::ToolRegistry;
use crate::tool::tasks::Task;
use crate::tool::{ToolDefinition, ToolMetadata};

/// The live session and the transcript it holds. Only its methods write either one, so the
/// in-memory messages always match what the session recorded, and a resumed session reads what the
/// turn showed. Methods that snapshot, reset, or close the tracked files first wait for pending
/// workspace writes.
pub(super) struct LiveTranscript {
    session: SessionHandle,
    messages: Vec<Message>,
    /// Compaction boundary the transcript loaded with, until the engine projects it as history.
    pub(super) compact: Option<CompactInfo>,
    /// Metadata the loaded transcript's tool results persisted, until the engine projects it.
    pub(super) tool_metadata: HashMap<String, ToolMetadata>,
    /// The task list the model keeps with the `tasks` tool, which a compaction carries over.
    pub(super) tasks: Vec<Task>,
    prompt: PromptParts,
    /// Whether `prompt` changed since the session last recorded it, so the next user message
    /// records it.
    prompt_unrecorded: bool,
    pub(super) file_tracker: Arc<FileTracker>,
    /// The tracked files the loaded session read that changed on disk since, until
    /// [`Engine::emit_session_start`](super::Engine::emit_session_start) reports them.
    pub(super) drifted: Vec<PathBuf>,
    /// Where the open turn rolls back to, `None` between turns.
    turn: Option<RollbackPoint>,
}

/// Where a refused turn rolls back to: the transcript before its prompt, or right after its last
/// compaction, and the session's checkpoint at the same point.
struct RollbackPoint {
    transcript: TranscriptStart,
    checkpoint: Checkpoint,
}

/// The transcript state at a turn's rollback point: its length, what the model had seen of the
/// user context and whether the prompt context awaits recording, and the tracked files.
struct TranscriptStart {
    len: usize,
    seen: UserContext,
    prompt_unrecorded: bool,
    files: Vec<FileSnapshot>,
}

impl LiveTranscript {
    /// Builds the live transcript of `transcript` on `session`, restoring the file tracker and
    /// building the prompt, which a fresh session starts in `mode` with `tools`. Also returns the
    /// loaded session's usage, which seeds its cost, and the warnings about instruction files a
    /// fresh prompt could not read.
    pub(super) async fn load(
        session: SessionHandle,
        transcript: LoadedTranscript,
        model: &ModelSpec,
        mode: Mode,
        tools: &ToolRegistry,
        cwd: &Path,
    ) -> (Self, Vec<UsageRecord>, Vec<String>) {
        let LoadedTranscript {
            messages,
            context,
            compact,
            tool_metadata,
            usage,
            tasks,
            file_snapshots,
        } = transcript;
        let (file_tracker, drifted) = FileTracker::restored(file_snapshots).await;
        let prompt_unrecorded = context.is_none();
        let (prompt, warnings) = session_prompt(context, model, mode, tools, cwd).await;
        let live = Self {
            session,
            messages,
            compact,
            tool_metadata,
            tasks,
            prompt,
            prompt_unrecorded,
            file_tracker: Arc::new(file_tracker),
            drifted,
            turn: None,
        };
        (live, usage, warnings)
    }

    /// Continues the same session with `transcript`, replacing what this one held. Also returns the
    /// loaded session's usage, which seeds its cost, and the instruction-file warnings.
    pub(super) async fn adopt(
        &mut self,
        transcript: LoadedTranscript,
        model: &ModelSpec,
        mode: Mode,
        tools: &ToolRegistry,
        cwd: &Path,
    ) -> (Vec<UsageRecord>, Vec<String>) {
        let (live, usage, warnings) =
            Self::load(self.session.clone(), transcript, model, mode, tools, cwd).await;
        *self = live;
        (usage, warnings)
    }

    pub(super) fn session(&self) -> &SessionHandle {
        &self.session
    }

    pub(super) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(super) fn prompt(&self) -> &PromptParts {
        &self.prompt
    }

    /// The context the parts of the next request fill, which stands in until a turn reports usage.
    pub(super) fn request_estimate(&self, tools: &[&ToolDefinition]) -> u32 {
        request_tokens(&self.prompt, tools, &self.messages)
    }

    /// Opens a turn with the user message `text`: moves the prompt to `fresh`, telling the model
    /// what changed in a reminder on the message, records the message, and keeps the point the turn
    /// rolls back to. A fresh session takes `title` as its first-prompt title.
    pub(super) async fn begin_turn(
        &mut self,
        text: &str,
        fresh: UserContext,
        title: Option<String>,
    ) -> RecordOutcome {
        let transcript = self.start_of_turn();
        let mut msg = Message::user(text);
        if let Some(reminder) = self.catch_up(fresh) {
            msg.content.push(ContentBlock::generated_text(
                reminder,
                TextOrigin::SystemReminder,
            ));
        }
        let outcome = self.record_prompt(msg, title).await;
        self.turn = Some(RollbackPoint {
            transcript,
            checkpoint: outcome.checkpoint,
        });
        outcome
    }

    /// Closes the open turn, which then can no longer roll back.
    pub(super) fn end_turn(&mut self) {
        self.turn = None;
    }

    /// Returns the transcript and the session to the open turn's rollback point, dropping what the
    /// turn added after it, and closes the turn. Returns the session's write report.
    ///
    /// # Panics
    ///
    /// Panics when no turn is open.
    pub(super) async fn roll_back(&mut self) -> WriteReport {
        let RollbackPoint {
            transcript: start,
            checkpoint,
        } = self.turn.take().expect("only an open turn rolls back");
        self.file_tracker.drain_writes().await;
        self.messages.truncate(start.len);
        self.prompt.seen = start.seen;
        self.prompt_unrecorded = start.prompt_unrecorded;
        self.file_tracker.reset_to(start.files);
        self.session.rewind(checkpoint).await
    }

    fn start_of_turn(&self) -> TranscriptStart {
        TranscriptStart {
            len: self.messages.len(),
            seen: self.prompt.seen.clone(),
            prompt_unrecorded: self.prompt_unrecorded,
            files: self.file_tracker.snapshot_all(),
        }
    }

    /// Moves the prompt to `fresh`, returning the reminder that explains the change to the model.
    /// The next user message records the prompt again.
    fn catch_up(&mut self, fresh: UserContext) -> Option<String> {
        let reminder = self.prompt.catch_up(fresh);
        self.prompt_unrecorded |= reminder.is_some();
        reminder
    }

    /// Moves the prompt to `mode`, where `plan` runs calls to `plan_tools`, returning the reminder
    /// the next user content carries when the model has seen another mode. The next recorded user
    /// message or tool round records the prompt again.
    pub(super) fn catch_up_mode(&mut self, mode: Mode, plan_tools: &[&str]) -> Option<String> {
        let reminder = self.prompt.catch_up_mode(mode, plan_tools);
        self.prompt_unrecorded |= reminder.is_some();
        reminder
    }

    /// Records `msg` with the prompt context it may carry, then appends it.
    async fn record_prompt(&mut self, msg: Message, title: Option<String>) -> RecordOutcome {
        let context =
            std::mem::take(&mut self.prompt_unrecorded).then(|| self.prompt.session_context());
        let outcome = self
            .session
            .record_prompt(msg.clone(), context, title)
            .await;
        self.messages.push(msg);
        outcome
    }

    /// Records `msg`, then appends it. Returns the session's write report.
    pub(super) async fn record_message(&mut self, msg: Message) -> WriteReport {
        let outcome = self.session.record_message(msg.clone()).await;
        self.messages.push(msg);
        outcome.report
    }

    /// Records one tool round, with the prompt context the results may carry, then appends the
    /// assistant message and the results answering it. Returns the session's write report.
    pub(super) async fn record_round(
        &mut self,
        assistant: Message,
        tool_results: Message,
        metadata: Vec<(String, ToolMetadata)>,
    ) -> WriteReport {
        let context =
            std::mem::take(&mut self.prompt_unrecorded).then(|| self.prompt.session_context());
        let report = self
            .session
            .record_round(assistant.clone(), tool_results.clone(), context, metadata)
            .await;
        self.messages.push(assistant);
        self.messages.push(tool_results);
        report
    }

    /// Records `records` against the session without changing the messages. Returns the session's
    /// write report.
    pub(super) async fn record_usage(&self, records: Vec<UsageRecord>) -> WriteReport {
        self.session.record_usage(records).await
    }

    /// Renames the session, returning its write report.
    pub(super) async fn set_manual_title(&self, title: String) -> WriteReport {
        self.session.set_manual_title(title).await
    }

    /// Archives the session, returning its write report.
    pub(super) async fn archive(&self) -> WriteReport {
        self.session.archive().await
    }

    /// Saves `boundary` and swaps the transcript to `synthetic`, which adopts the boundary's
    /// prompt context. Clears the tracked files with the transcript, and moves an open turn's
    /// rollback point to the summary.
    pub(super) async fn compact(
        &mut self,
        boundary: CompactBoundary,
        synthetic: Message,
    ) -> CompactOutcome {
        self.file_tracker.drain_writes().await;
        let context = boundary.context.clone();
        let outcome = self.session.compact(boundary, synthetic.clone()).await;
        self.messages = vec![synthetic];
        self.prompt.adopt(context);
        self.prompt_unrecorded = false;
        self.file_tracker.clear();
        if self.turn.is_some() {
            self.turn = Some(RollbackPoint {
                transcript: self.start_of_turn(),
                checkpoint: outcome.checkpoint,
            });
        }
        outcome
    }

    /// Finalizes the session with the read files and continues in a fresh one on `git_branch`.
    pub(super) async fn roll(
        &mut self,
        store: &SessionStore,
        model: &str,
        git_branch: Option<String>,
    ) -> RollOutcome {
        self.file_tracker.drain_writes().await;
        handle::roll(
            &mut self.session,
            store,
            self.file_tracker.snapshot_all(),
            model,
            git_branch,
        )
        .await
    }

    /// Finalizes the session with the files the transcript read and swaps in the saved session
    /// `target` names, without swapping when it cannot be resolved or loaded. A fork of the resumed
    /// session records `model`.
    pub(super) async fn roll_into(
        &mut self,
        store: &SessionStore,
        target: SessionRef,
        model: &str,
    ) -> Result<RollIntoOutcome> {
        self.file_tracker.drain_writes().await;
        handle::roll_into(
            &mut self.session,
            store,
            self.file_tracker.snapshot_all(),
            target,
            model,
        )
        .await
    }

    /// Closes the session with a snapshot of the files the transcript read. Returns the session's
    /// write report.
    pub(super) async fn finalize(self) -> WriteReport {
        self.file_tracker.drain_writes().await;
        self.session
            .finalize(self.file_tracker.snapshot_all())
            .await
    }
}

#[cfg(test)]
impl LiveTranscript {
    /// A transcript holding `messages`, with no compaction state, prompt, or tracked files.
    pub(super) fn with_messages(session: &SessionHandle, messages: Vec<Message>) -> Self {
        Self {
            session: session.clone(),
            messages,
            compact: None,
            tool_metadata: HashMap::new(),
            tasks: Vec::new(),
            prompt: PromptParts::default(),
            prompt_unrecorded: false,
            file_tracker: Arc::new(FileTracker::default()),
            drifted: Vec::new(),
            turn: None,
        }
    }

    /// Replaces the messages without writing the session.
    pub(super) fn set_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Whether the next user message records the prompt.
    pub(super) fn prompt_unrecorded(&self) -> bool {
        self.prompt_unrecorded
    }
}

#[cfg(any(test, feature = "testing"))]
impl LiveTranscript {
    /// The prompt, which a test fixture may change as another project's context would.
    pub(super) fn prompt_mut(&mut self) -> &mut PromptParts {
        &mut self.prompt
    }
}

/// Replays the prompt a session recorded, or builds a fresh one in `mode` with `tools` for a
/// session without one. Also returns the warnings about instruction files a fresh build could not
/// read.
async fn session_prompt(
    context: Option<SessionContext>,
    model: &ModelSpec,
    mode: Mode,
    tools: &ToolRegistry,
    cwd: &Path,
) -> (PromptParts, Vec<String>) {
    match context {
        Some(context) => (prompt::restore_prompt(context, cwd).await, Vec::new()),
        None => prompt::build_prompt(model, mode, &tools.plan_tools(), cwd).await,
    }
}
