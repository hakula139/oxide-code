//! The live session and the transcript it records, which change together.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::compaction::request_tokens;
use crate::file_tracker::{FileSnapshot, FileTracker};
use crate::message::Message;
use crate::model::ModelSpec;
use crate::prompt::{self, PromptParts, SessionContext, UserContext};
use crate::protocol::ResumeTarget;
use crate::session::entry::{CompactBoundary, CompactInfo, UsageRecord};
use crate::session::handle::{
    self, Checkpoint, CompactOutcome, LoadedTranscript, RecordOutcome, RollIntoOutcome,
    RollOutcome, SessionHandle,
};
use crate::session::store::SessionStore;
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
}

/// The transcript state before a turn's prompt: its length, what the model had seen of the user
/// context and whether the prompt context awaits recording, and the tracked files.
pub(super) struct TranscriptStart {
    len: usize,
    seen: UserContext,
    prompt_unrecorded: bool,
    files: Vec<FileSnapshot>,
}

impl LiveTranscript {
    /// Builds the live transcript of `transcript` on `session`, restoring the file tracker and
    /// building the prompt. Also returns the loaded session's usage, which seeds its cost, and the
    /// warnings about instruction files a fresh prompt could not read.
    pub(super) async fn load(
        session: SessionHandle,
        transcript: LoadedTranscript,
        model: &ModelSpec,
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
        let (prompt, warnings) = session_prompt(context, model, cwd).await;
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
        };
        (live, usage, warnings)
    }

    /// Continues the same session with `transcript`, replacing what this one held. Also returns the
    /// loaded session's usage, which seeds its cost, and the instruction-file warnings.
    pub(super) async fn adopt(
        &mut self,
        transcript: LoadedTranscript,
        model: &ModelSpec,
        cwd: &Path,
    ) -> (Vec<UsageRecord>, Vec<String>) {
        let (live, usage, warnings) =
            Self::load(self.session.clone(), transcript, model, cwd).await;
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

    /// The state [`Self::roll_back`] returns to before a turn's prompt.
    pub(super) fn start_of_turn(&self) -> TranscriptStart {
        TranscriptStart {
            len: self.messages.len(),
            seen: self.prompt.seen.clone(),
            prompt_unrecorded: self.prompt_unrecorded,
            files: self.file_tracker.snapshot_all(),
        }
    }

    /// Returns the transcript and the session to `start` and `checkpoint`, dropping what the turn
    /// after them added. Returns the session's write failure.
    pub(super) async fn roll_back(
        &mut self,
        start: TranscriptStart,
        checkpoint: Option<Checkpoint>,
    ) -> Option<String> {
        self.file_tracker.drain_writes().await;
        self.messages.truncate(start.len);
        self.prompt.seen = start.seen;
        self.prompt_unrecorded = start.prompt_unrecorded;
        self.file_tracker.reset_to(start.files);
        match checkpoint {
            Some(checkpoint) => self.session.rewind(checkpoint).await.failure,
            None => None,
        }
    }

    /// Moves the prompt to `fresh`, returning the reminder that explains the change to the model.
    /// The next user message records the prompt again.
    pub(super) fn catch_up(&mut self, fresh: UserContext) -> Option<String> {
        let reminder = self.prompt.catch_up(fresh);
        if reminder.is_some() {
            self.prompt_unrecorded = true;
        }
        reminder
    }

    /// Records `msg` with the prompt context it may carry, then appends it.
    pub(super) async fn record_prompt(
        &mut self,
        msg: Message,
        title: Option<String>,
    ) -> RecordOutcome {
        let context =
            std::mem::take(&mut self.prompt_unrecorded).then(|| self.prompt.session_context());
        let outcome = self
            .session
            .record_prompt(msg.clone(), context, title)
            .await;
        self.messages.push(msg);
        outcome
    }

    /// Records `msg`, then appends it. Returns the session's write failure.
    pub(super) async fn record_message(&mut self, msg: Message) -> Option<String> {
        let outcome = self.session.record_message(msg.clone()).await;
        self.messages.push(msg);
        outcome.failure
    }

    /// Records one tool round, then appends the assistant message and the results answering it.
    /// Returns the session's write failure.
    pub(super) async fn record_round(
        &mut self,
        assistant: Message,
        tool_results: Message,
        metadata: Vec<(String, ToolMetadata)>,
    ) -> Option<String> {
        let outcome = self
            .session
            .record_round(assistant.clone(), tool_results.clone(), metadata)
            .await;
        self.messages.push(assistant);
        self.messages.push(tool_results);
        outcome.failure
    }

    /// Records `records` against the session without changing the messages. Returns the session's
    /// write failure.
    pub(super) async fn record_usage(&self, records: Vec<UsageRecord>) -> Option<String> {
        self.session.record_usage(records).await.failure
    }

    /// Renames the session, returning its write failure.
    pub(super) async fn set_manual_title(&self, title: String) -> Option<String> {
        self.session.set_manual_title(title).await.failure
    }

    /// Archives the session, returning its write failure.
    pub(super) async fn archive(&self) -> Option<String> {
        self.session.archive().await.failure
    }

    /// Saves `boundary` and swaps the transcript to `synthetic`, which adopts the boundary's
    /// prompt context. Clears the tracked files with the transcript.
    pub(super) async fn compact(
        &mut self,
        boundary: CompactBoundary,
        synthetic: Message,
    ) -> CompactOutcome {
        self.file_tracker.drain_writes().await;
        let context = boundary.context.clone();
        let outcome = self.session.compact(boundary, synthetic.clone()).await;
        if outcome.checkpoint.is_some() {
            self.messages = vec![synthetic];
            self.prompt.adopt(context);
            self.prompt_unrecorded = false;
            self.file_tracker.clear();
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
    /// `target` names, without swapping when it cannot be resolved or loaded.
    pub(super) async fn roll_into(
        &mut self,
        store: &SessionStore,
        target: ResumeTarget,
    ) -> Result<RollIntoOutcome> {
        self.file_tracker.drain_writes().await;
        handle::roll_into(
            &mut self.session,
            store,
            self.file_tracker.snapshot_all(),
            target,
        )
        .await
    }

    /// Closes the session with a snapshot of the files the transcript read. Returns the session's
    /// write failure.
    pub(super) async fn finalize(self) -> Option<String> {
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
        }
    }

    /// Replaces the messages without writing the session.
    pub(super) fn set_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Replaces the session without finalizing the one the transcript holds.
    pub(super) fn set_session(&mut self, session: SessionHandle) {
        self.session = session;
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

/// Replays the prompt a session recorded, or builds a fresh one for a session without one. Also
/// returns the warnings about instruction files a fresh build could not read.
async fn session_prompt(
    context: Option<SessionContext>,
    model: &ModelSpec,
    cwd: &Path,
) -> (PromptParts, Vec<String>) {
    match context {
        Some(context) => (prompt::restore_prompt(context, cwd).await, Vec::new()),
        None => prompt::build_prompt(model, cwd).await,
    }
}
