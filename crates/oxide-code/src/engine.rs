//! The engine: one conversation running as a task that every frontend drives through an
//! [`EngineHandle`], sending ops on a bounded channel and reading events from an unbounded one.

mod abort;
mod compact_boundary;
mod compaction;
mod dispatch;
#[cfg(test)]
pub(crate) mod testing;
mod turn;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use self::abort::{Inbox, TurnAbort};
use self::compact_boundary::Boundary;
use self::compaction::{AutoCompact, AutoCompactFailures};
use self::turn::{TurnOutcome, TurnReport};
use crate::client::TokenUsage;
use crate::client::anthropic::Client;
use crate::config::auth::{self, Auth, AuthSource};
use crate::config::compaction::CompactionConfig;
use crate::config::login::PendingLogin;
use crate::config::oauth;
use crate::config::{Config, Effort};
use crate::file_tracker::FileTracker;
use crate::message::{ContentBlock, Message};
use crate::model::{ModelSpec, ResolvedModelId};
use crate::permission::{Mode, Policy, Workspace};
use crate::prompt::{self, PromptParts, SessionContext, UserContext};
use crate::protocol::{Event, EventSink, Interaction, Op, ResumeTarget, UsageSnapshot};
use crate::session::display::id_prefix;
use crate::session::entry::{CompactInfo, UsageRecord};
use crate::session::handle::{self, LoadedTranscript, RollIntoOutcome, SessionHandle};
use crate::session::history;
use crate::session::resolver::resolve_saved_prefix;
use crate::session::store::SessionStore;
use crate::session::title_generator;
use crate::tool::bash::background::BackgroundShells;
use crate::tool::tasks::Task;
use crate::tool::{ToolMetadata, ToolRegistry};
use crate::util::text::plural;

/// Ops a frontend can queue before the engine reads them. A turn keeps reading ops while it runs,
/// so this only absorbs a burst of queued prompts.
const OP_CHANNEL_CAP: usize = 32;

// ── Engine Handle ──

/// A running [`Engine`]. Closing `ops` asks it to quit.
pub(crate) struct EngineHandle {
    pub(crate) ops: mpsc::Sender<Op>,
    pub(crate) events: mpsc::UnboundedReceiver<Event>,
    pub(crate) task: EngineTask,
}

impl EngineHandle {
    /// Quits the engine and joins its task. Holds `events` open until then, so the engine's final
    /// emits don't fail and log as dropped.
    pub(crate) async fn shutdown(self) -> Option<String> {
        let Self {
            ops,
            events: _events,
            task,
        } = self;
        drop(ops);
        task.join().await
    }
}

pub(crate) struct EngineTask(JoinHandle<Option<String>>);

impl EngineTask {
    /// Waits until the engine has finalized the live session, returning the write failure if any.
    pub(crate) async fn join(self) -> Option<String> {
        self.0.await.unwrap_or_else(|e| {
            warn!("engine task failed: {e}");
            None
        })
    }
}

/// [`EventSink`] feeding [`EngineHandle::events`]. The channel is unbounded, so the engine never
/// waits on or drops events for a frontend that falls behind.
#[derive(Clone)]
struct ChannelSink(mpsc::UnboundedSender<Event>);

impl EventSink for ChannelSink {
    fn send(&self, event: Event) -> Result<()> {
        self.0
            .send(event)
            .map_err(|_| anyhow!("the frontend stopped reading events"))
    }
}

// ── Engine ──

/// The tool registry and the background shells its `bash` tools share with the engine.
pub(crate) struct Toolbox {
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) shells: Arc<BackgroundShells>,
}

/// The live session with its transcript, prompt, and usage accounting.
pub(crate) struct Engine {
    client: Client,
    /// Where `/login` exchanges its authorization code, a mock server in tests.
    oauth_token_url: String,
    tools: Arc<ToolRegistry>,
    store: SessionStore,
    session: SessionHandle,
    messages: Vec<Message>,
    /// Compaction boundary the transcript loaded with, until [`Self::take_history`] projects it.
    compact: Option<CompactInfo>,
    /// Metadata the loaded transcript's tool results persisted, until [`Self::take_history`]
    /// projects it.
    tool_metadata: HashMap<String, ToolMetadata>,
    /// The task list the model keeps with the `tasks` tool, which a compaction carries over.
    tasks: Vec<Task>,
    prompt: PromptParts,
    /// `prompt` changed since the session last recorded it, so the next user message records it.
    prompt_unrecorded: bool,
    /// Warnings about the instruction files the latest user context load could not read.
    instruction_warnings: Vec<String>,
    file_tracker: Arc<FileTracker>,
    /// The background shells the `bash` tools start, which end with the session.
    shells: Arc<BackgroundShells>,
    /// The configured policy plus the session allow rules granted since the live session began.
    policy: Policy,
    /// `None` means the agent loop runs without a per-turn round cap.
    max_tool_rounds: Option<u32>,
    /// Compaction settings resolved for the active model.
    compaction: CompactionConfig,
    /// The launch directory and the workspace roots every tool call runs in.
    workspace: Arc<Workspace>,
    /// A TUI is attached, so titles generate in the background.
    interactive: bool,
    auto_compaction_failures: AutoCompactFailures,
    /// The latest turn's last round usage, `None` when that turn reported none.
    last_usage: Option<TokenUsage>,
    /// The latest usage any turn reported, which the status line shows.
    displayed_usage: Option<TokenUsage>,
    total_estimated_cost_usd: f64,
}

impl Engine {
    /// Continues `session` with a client and the agent settings from `config`. Fails when the
    /// client cannot be built.
    #[expect(
        clippy::too_many_arguments,
        reason = "each part comes from a separate startup step in `main`"
    )]
    pub(crate) async fn new(
        config: &Config,
        device_id: String,
        toolbox: Toolbox,
        store: SessionStore,
        session: SessionHandle,
        transcript: LoadedTranscript,
        file_tracker: Arc<FileTracker>,
        interactive: bool,
    ) -> Result<Self> {
        let client = Client::new(config, Some(session.session_id().to_owned()), device_id)?;
        let Toolbox { tools, shells } = toolbox;
        let LoadedTranscript {
            messages,
            context,
            compact,
            tool_metadata,
            usage,
            tasks,
        } = transcript;
        let prompt_unrecorded = context.is_none();
        let workspace = Arc::new(config.workspace.clone());
        let (prompt, instruction_warnings) =
            session_prompt(context, client.model(), workspace.cwd()).await;
        Ok(Self {
            client,
            oauth_token_url: oauth::OAUTH_TOKEN_URL.to_owned(),
            tools,
            store,
            session,
            messages,
            compact,
            tool_metadata,
            tasks,
            prompt,
            prompt_unrecorded,
            instruction_warnings,
            file_tracker,
            shells,
            policy: config.permission.clone(),
            max_tool_rounds: config.max_tool_rounds,
            compaction: config.compaction,
            workspace,
            interactive,
            auto_compaction_failures: AutoCompactFailures::default(),
            last_usage: None,
            displayed_usage: None,
            total_estimated_cost_usd: estimate_cost_usd(&usage),
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        self.session.session_id()
    }

    pub(crate) fn cwd(&self) -> &Path {
        self.workspace.cwd()
    }

    /// Projects the loaded transcript for a frontend to show. Takes the compaction boundary and
    /// tool metadata it loaded with, so a later call projects without them.
    pub(crate) fn take_history(&mut self) -> Vec<Interaction> {
        let compact = self.compact.take();
        let tool_metadata = std::mem::take(&mut self.tool_metadata);
        history::interactions(
            &self.messages,
            compact.as_ref(),
            &tool_metadata,
            &self.tools,
            self.workspace.cwd(),
        )
    }

    /// Starts the engine task, which first emits what [`Self::emit_session_start`] shows. `drifted`
    /// lists the tracked files that changed on disk since the resumed session read them.
    pub(crate) fn spawn(self, drifted: &[PathBuf]) -> EngineHandle {
        let (ops, op_rx) = mpsc::channel(OP_CHANNEL_CAP);
        let (event_tx, events) = mpsc::unbounded_channel();
        let sink = ChannelSink(event_tx);
        self.shells.attach(Arc::new(sink.clone()));
        self.emit_session_start(&sink, drifted);
        let task = EngineTask(tokio::spawn(self.run(sink, op_rx)));
        EngineHandle { ops, events, task }
    }

    /// Serves ops until `Shutdown` or the op channel closes, then finalizes the live session.
    async fn run(mut self, sink: ChannelSink, mut ops: mpsc::Receiver<Op>) -> Option<String> {
        while let Some(op) = ops.recv().await {
            match op {
                Op::Prompt(text) => {
                    if self.submit_prompt(&text, &sink, &mut ops).await == TurnEnd::Shutdown {
                        break;
                    }
                }
                Op::Compact { instructions } => {
                    if self.compact(&sink, &mut ops, instructions).await == TurnEnd::Shutdown {
                        break;
                    }
                }
                Op::Clear => self.clear(&sink).await,
                Op::Archive => self.archive(&sink).await,
                Op::Resume { target } => self.resume(&sink, target).await,
                Op::Rename { title } => self.rename(&sink, title).await,
                Op::SetModel { model, effort } => self.set_model(&sink, model, effort),
                Op::SetMode(mode) => self.set_mode(&sink, mode),
                Op::Login => {
                    if self.login(&sink, &mut ops).await == TurnEnd::Shutdown {
                        break;
                    }
                }
                Op::Logout => self.logout(&sink).await,
                // A `Queue` here arrived after its turn ended, and the frontend still submits it.
                // An interrupt or approval only matters mid-turn, where `agent_turn` consumes it.
                Op::Queue(_) | Op::Interrupt | Op::Resolve { .. } => {}
                Op::Shutdown => break,
            }
        }

        self.finalize().await
    }

    /// Runs `text` through auto-compaction and one agent turn, reporting how it ended on `sink`.
    async fn submit_prompt<S>(
        &mut self,
        text: &str,
        sink: &S,
        ops: &mut mpsc::Receiver<Op>,
    ) -> TurnEnd
    where
        S: EventSink + Clone + 'static,
    {
        if self.client.auth_source().is_none() {
            return report_abort(sink, signed_out(), "turn-failed");
        }
        let mut inbox = Inbox::default();
        let compacted = self.auto_compact(sink, ops, &mut inbox).await;
        self.adopt_mode(&mut inbox);
        if let Err(abort) = compacted {
            return report_abort(sink, abort, "auto-compact-failed");
        }

        let mut user_msg = Message::user(text);
        let fresh = self.load_user_context(sink).await;
        if let Some(reminder) = self.prompt.catch_up(fresh) {
            user_msg.content.push(ContentBlock::Text { text: reminder });
            self.prompt_unrecorded = true;
        }
        let context =
            std::mem::take(&mut self.prompt_unrecorded).then(|| self.prompt.session_context());
        let outcome = self
            .session
            .record_message_with_context(user_msg.clone(), context)
            .await;
        sink.session_write_error(outcome.failure.as_deref());
        self.messages.push(user_msg);
        turn::record_drained_prompts(
            inbox.prompts.drain(..),
            &mut self.messages,
            &self.session,
            sink,
        )
        .await;

        if self.interactive
            && let Some(seed) = outcome.ai_title_seed
        {
            title_generator::spawn(
                self.client.clone(),
                self.session.clone(),
                sink.clone(),
                seed,
            );
        }

        let mut dispatch_ctx = dispatch::DispatchContext {
            policy: &mut self.policy,
            workspace: Arc::clone(&self.workspace),
            session_id: self.session.session_id().to_owned(),
            tracker: Arc::clone(&self.file_tracker),
        };
        let TurnOutcome { mut report, result } = turn::agent_turn(
            &self.client,
            &self.tools,
            &mut self.messages,
            &self.prompt,
            sink,
            &self.session,
            ops,
            &mut inbox,
            self.max_tool_rounds,
            &mut dispatch_ctx,
        )
        .await;
        self.adopt_mode(&mut inbox);
        self.account_usage(sink, &report).await;
        if let Some(tasks) = report.tasks.take() {
            self.tasks = tasks;
        }
        match result {
            Ok(()) => {
                sink.emit(Event::TurnComplete, "turn-complete");
                TurnEnd::Completed
            }
            Err(abort) => report_abort(sink, abort, "turn-failed"),
        }
    }

    async fn auto_compact(
        &mut self,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
    ) -> Result<(), TurnAbort> {
        let tools = self.tools.definitions();
        let mut spent = Vec::new();
        let compacted = compaction::auto_compact_if_needed(
            &self.client,
            &self.session,
            &mut self.messages,
            sink,
            ops,
            inbox,
            Some(&mut AutoCompact {
                config: self.compaction.auto,
                failures: &mut self.auto_compaction_failures,
                file_tracker: &self.file_tracker,
                prompt: &self.prompt,
                tools: &tools,
                tasks: &self.tasks,
                shells: &self.shells,
                spent: &mut spent,
            }),
            self.last_usage,
        )
        .await;
        self.bill(sink, &spent).await;
        if matches!(compacted, Ok(true)) {
            self.refresh_prompt(sink).await;
            self.reset_context_usage(sink);
        }
        if !spent.is_empty() {
            self.emit_usage_update(sink);
        }
        compacted.map(|_| ())
    }

    /// Replaces the transcript with a summary steered by `instructions`. Failures leave it intact.
    async fn compact(
        &mut self,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        instructions: Option<String>,
    ) -> TurnEnd {
        if self.client.auth_source().is_none() {
            return report_abort(sink, signed_out(), "compact-failed");
        }
        let tools = self.tools.definitions();
        let mut spent = Vec::new();
        let mut inbox = Inbox::default();
        let summary = abort::await_unless_aborted(
            compaction::compact_session(
                &self.client,
                &self.prompt,
                &tools,
                &self.messages,
                instructions.as_deref(),
                sink,
                &mut spent,
            ),
            ops,
            &mut inbox,
            sink,
        )
        .await;
        self.adopt_mode(&mut inbox);
        self.bill(sink, &spent).await;
        let summary = match summary {
            Ok(Ok(summary)) => summary,
            Ok(Err(e)) => {
                let abort = TurnAbort::Failed(anyhow!("Compaction failed: {e:#}"));
                return self.fail_compact(sink, abort);
            }
            Err(abort) => return self.fail_compact(sink, abort),
        };
        let compacted = compact_boundary::replace_session_with_summary(
            &self.session,
            &self.file_tracker,
            &mut self.messages,
            sink,
            Boundary {
                summary,
                instructions,
                tasks: self.tasks.clone(),
                shells: self.shells.list(),
                automatic: false,
            },
        )
        .await;
        if !compacted {
            let abort = TurnAbort::Failed(anyhow!(
                "Compaction was not saved, so the transcript is unchanged"
            ));
            return self.fail_compact(sink, abort);
        }
        self.refresh_prompt(sink).await;
        self.reset_context_usage(sink);
        self.emit_usage_update(sink);
        TurnEnd::Completed
    }

    /// Ends a compaction that left the transcript unchanged, showing the summary's cost first.
    fn fail_compact(&self, sink: &dyn EventSink, abort: TurnAbort) -> TurnEnd {
        self.emit_usage_update(sink);
        report_abort(sink, abort, "compact-failed")
    }

    /// Starts the compacted transcript from the current instructions and date.
    async fn refresh_prompt(&mut self, sink: &dyn EventSink) {
        let fresh = self.load_user_context(sink).await;
        self.prompt.refresh(fresh);
        self.prompt_unrecorded = true;
    }

    /// Reads the user context afresh, showing each instruction file warning the previous load did
    /// not raise.
    async fn load_user_context(&mut self, sink: &dyn EventSink) -> UserContext {
        let (context, warnings) = UserContext::load(self.cwd(), self.client.model()).await;
        let previous = std::mem::replace(&mut self.instruction_warnings, warnings);
        for warning in &self.instruction_warnings {
            if !previous.contains(warning) {
                sink.emit(Event::Notice(warning.clone()), "instruction-warning");
            }
        }
        context
    }

    /// Finalizes the live session and continues in a fresh, empty one.
    async fn clear(&mut self, sink: &dyn EventSink) {
        self.roll_session(sink, false).await;
    }

    /// Archives the live session, then rolls like [`Self::clear`].
    async fn archive(&mut self, sink: &dyn EventSink) {
        if self.messages.is_empty() {
            sink.emit(
                Event::Error("Nothing to archive: this session has no messages yet".to_owned()),
                "archive-empty",
            );
            return;
        }
        if let Some(failure) = self.session.archive().await.failure {
            sink.emit(
                Event::Error(format!("Archive failed: {failure}")),
                "archive-failed",
            );
            return;
        }
        self.roll_session(sink, true).await;
    }

    async fn roll_session(&mut self, sink: &dyn EventSink, archived: bool) {
        let outcome = handle::roll(
            &mut self.session,
            &self.store,
            &self.file_tracker,
            self.client.model().id(),
        )
        .await;
        self.adopt_session(sink, LoadedTranscript::default()).await;
        sink.emit(
            Event::SessionRolled {
                id: outcome.new_id,
                archived,
            },
            "session-rolled",
        );
        report_finalize_failure(sink, outcome.finalize_failure);
        self.emit_session_start(sink, &[]);
    }

    /// Finalizes the live session and continues `target`. On failure the live session stays.
    async fn resume(&mut self, sink: &dyn EventSink, target: ResumeTarget) {
        let outcome = match self.roll_into(target).await {
            Ok(outcome) => outcome,
            Err(e) => {
                sink.emit(
                    Event::TurnFailed(format!(
                        "Resume failed (still on session {}): {e:#}",
                        id_prefix(self.session.session_id()),
                    )),
                    "resume-failed",
                );
                return;
            }
        };
        self.adopt_session(sink, outcome.transcript).await;
        sink.emit(
            Event::SessionResumed {
                id: self.session.session_id().to_owned(),
                title: outcome.title,
                unarchived: outcome.unarchived,
                history: self.take_history(),
            },
            "session-resumed",
        );
        report_finalize_failure(sink, outcome.finalize_failure);
        self.emit_session_start(sink, &outcome.drifted_paths);
    }

    /// Finalizes the live session and swaps to the one `target` names. A prefix must match one
    /// saved session other than the live one.
    async fn roll_into(&mut self, target: ResumeTarget) -> Result<RollIntoOutcome> {
        let path = match target {
            ResumeTarget::Path(path) => path,
            ResumeTarget::Prefix(prefix) => {
                resolve_saved_prefix(&self.store, &prefix, self.session.session_id())?
                    .ok_or_else(|| anyhow!("session prefix `{prefix}` matches the live session"))?
                    .path
            }
        };
        handle::roll_into(&mut self.session, &self.store, &self.file_tracker, &path).await
    }

    async fn adopt_session(&mut self, sink: &dyn EventSink, transcript: LoadedTranscript) {
        self.shells.stop_all().await;
        let LoadedTranscript {
            messages,
            context,
            compact,
            tool_metadata,
            usage,
            tasks,
        } = transcript;
        self.client
            .set_session_id(self.session.session_id().to_owned());
        self.messages = messages;
        self.compact = compact;
        self.tool_metadata = tool_metadata;
        self.tasks = tasks;
        self.prompt_unrecorded = context.is_none();
        (self.prompt, self.instruction_warnings) =
            session_prompt(context, self.client.model(), self.workspace.cwd()).await;
        self.policy.clear_session_allow();
        self.reset_context_usage(sink);
        self.total_estimated_cost_usd = estimate_cost_usd(&usage);
    }

    async fn rename(&self, sink: &dyn EventSink, title: String) {
        let outcome = self.session.set_manual_title(title.clone()).await;
        if let Some(failure) = outcome.failure {
            sink.emit(
                Event::RenameFailed(format!("Session write failed: {failure}")),
                "rename-failed",
            );
            return;
        }
        sink.emit(
            Event::SessionTitleUpdated {
                session_id: self.session.session_id().to_owned(),
                title,
            },
            "session-title-updated",
        );
    }

    /// Applies a model and / or effort pick. The model goes first because it re-resolves effort.
    fn set_model(
        &mut self,
        sink: &dyn EventSink,
        model: Option<ResolvedModelId>,
        effort: Option<Effort>,
    ) {
        if let Some(id) = model {
            self.client.set_model(id.into_inner());
            self.compaction = self
                .compaction
                .for_model(self.client.model(), self.client.max_tokens());
        }
        let resolved = match effort {
            Some(pick) => self.client.set_effort(pick),
            None => self.client.effort(),
        };
        sink.emit(
            Event::ConfigChanged {
                model: self.client.model().clone(),
                effort: resolved,
                requested_effort: effort,
                auto_compaction: self.compaction.auto,
            },
            "config-changed",
        );
        self.auto_compaction_failures.reset(sink);
        self.emit_usage_update(sink);
    }

    /// Switches the permission mode for later calls.
    fn set_mode(&mut self, sink: &dyn EventSink, mode: Mode) {
        self.policy.set_mode(mode);
        sink.emit(Event::ModeChanged(mode), "mode-changed");
    }

    /// Applies the mode switch an operation left in `inbox`.
    fn adopt_mode(&mut self, inbox: &mut Inbox) {
        if let Some(mode) = inbox.mode.take() {
            self.policy.set_mode(mode);
        }
    }

    /// Signs in through the browser and stores the new oxide-code login.
    async fn login(&mut self, sink: &dyn EventSink, ops: &mut mpsc::Receiver<Op>) -> TurnEnd {
        let pending = match PendingLogin::start().await {
            Ok(pending) => pending,
            Err(e) => return report_abort(sink, sign_in_failed(&e), "login-failed"),
        };
        sink.emit(
            Event::LoginStarted {
                url: pending.url().to_owned(),
            },
            "login-started",
        );

        let mut inbox = Inbox::default();
        let finish = pending.finish(&self.oauth_token_url, self.client.extra_ca_certs());
        let signed_in = abort::await_unless_aborted(finish, ops, &mut inbox, sink).await;
        self.adopt_mode(&mut inbox);
        match signed_in {
            Ok(Ok(auth)) => {
                self.adopt_login(sink, auth);
                TurnEnd::Completed
            }
            Ok(Err(e)) => report_abort(sink, sign_in_failed(&e), "login-failed"),
            Err(abort) => report_abort(sink, abort, "login-failed"),
        }
    }

    /// Uses a new oxide-code login for requests unless an env var or `client.api_key` outranks it.
    fn adopt_login(&mut self, sink: &dyn EventSink, auth: Auth) {
        let active = match self.client.auth_source() {
            Some(source) if !source.is_stored_login() => source,
            _ => {
                self.client.set_auth(Some(auth));
                AuthSource::OxLogin
            }
        };
        sink.emit(Event::SignedIn { auth: active }, "signed-in");
    }

    /// Removes the oxide-code login. A session using it, or one signed out, falls back to the
    /// Claude Code login, and stays signed out without a usable one.
    async fn logout(&mut self, sink: &dyn EventSink) {
        let removed = match oauth::delete_login().await {
            Ok(removed) => removed,
            Err(e) => {
                sink.emit(
                    Event::Error(format!("Sign-out failed: {e:#}")),
                    "logout-failed",
                );
                return;
            }
        };
        let mut fallback_error = None;
        if self
            .client
            .auth_source()
            .is_none_or(|source| source == AuthSource::OxLogin)
        {
            let fallback = auth::claude_code_login(self.client.extra_ca_certs())
                .await
                .unwrap_or_else(|e| {
                    fallback_error = Some(format!("{e:#}"));
                    None
                });
            self.client.set_auth(fallback);
        }
        sink.emit(
            Event::SignedOut {
                removed,
                auth: self.client.auth_source(),
                fallback_error,
            },
            "signed-out",
        );
    }

    async fn account_usage(&mut self, sink: &dyn EventSink, report: &TurnReport) {
        self.last_usage = report.usage;
        if let Some(usage) = report.usage {
            self.displayed_usage = Some(usage);
        }
        self.bill(sink, &report.billable_rounds).await;
        if report.usage.is_some() || !report.billable_rounds.is_empty() {
            self.emit_usage_update(sink);
        }
    }

    /// Records each request in `requests` and adds its estimated cost to the session total.
    async fn bill(&mut self, sink: &dyn EventSink, requests: &[TokenUsage]) {
        if requests.is_empty() {
            return;
        }
        let records: Vec<_> = requests
            .iter()
            .map(|&tokens| UsageRecord {
                model: self.client.model().id().to_owned(),
                cache_ttl: self.client.prompt_cache_ttl(),
                tokens,
            })
            .collect();
        self.total_estimated_cost_usd += estimate_cost_usd(&records);
        let outcome = self.session.record_usage(records).await;
        sink.session_write_error(outcome.failure.as_deref());
    }

    /// Forgets the usage and auto-compaction failures of a transcript the session no longer sends.
    fn reset_context_usage(&mut self, sink: &dyn EventSink) {
        self.auto_compaction_failures.reset(sink);
        self.last_usage = None;
        self.displayed_usage = None;
    }

    /// Shows what a session carries as the engine starts on it: its usage and task list, the
    /// warnings its prompt raised, the tracked files in `drifted`, and a transcript that ends on an
    /// interrupted response. Follows the event that swaps the session, which clears the chat.
    fn emit_session_start(&self, sink: &dyn EventSink, drifted: &[PathBuf]) {
        self.emit_usage_update(sink);
        self.emit_restored_tasks(sink);
        for warning in &self.instruction_warnings {
            sink.emit(Event::Notice(warning.clone()), "instruction-warning");
        }
        report_drift(sink, drifted);
        turn::notify_if_interrupted(&self.messages, sink);
    }

    /// Shows the task list a loaded session restored. An empty one needs no event, since the
    /// frontend empties its list when the session changes.
    fn emit_restored_tasks(&self, sink: &dyn EventSink) {
        if !self.tasks.is_empty() {
            sink.emit(Event::TasksUpdated(self.tasks.clone()), "tasks-restored");
        }
    }

    fn emit_usage_update(&self, sink: &dyn EventSink) {
        let estimated_cost_usd =
            (self.total_estimated_cost_usd > 0.0).then_some(self.total_estimated_cost_usd);
        if self.displayed_usage.is_none() && estimated_cost_usd.is_none() {
            return;
        }
        sink.emit(
            Event::UsageUpdated(UsageSnapshot {
                context_tokens: self.displayed_usage.map(TokenUsage::context_tokens),
                context_window: self.client.model().context_window(),
                estimated_cost_usd,
            }),
            "usage-updated",
        );
    }

    /// Ends the background shells, then flushes and closes the live session, returning the write
    /// failure if any.
    async fn finalize(self) -> Option<String> {
        self.shells.stop_all().await;
        self.session
            .finalize(self.file_tracker.snapshot_all())
            .await
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

/// Estimated USD cost of `requests`, each priced by the model and cache TTL it ran with.
fn estimate_cost_usd(requests: &[UsageRecord]) -> f64 {
    requests
        .iter()
        .filter_map(|request| {
            let rates = ModelSpec::resolve(request.model.clone()).cost_rates()?;
            let tokens = request.tokens;
            Some(rates.estimate_usd(
                tokens.input_tokens,
                tokens.cache_creation_input_tokens,
                tokens.cache_read_input_tokens,
                tokens.output_tokens,
                request.cache_ttl,
            ))
        })
        .sum()
}

// ── Reporting ──

/// How an operation ended, after the sink has already shown it. A headless run reports it as its
/// stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnEnd {
    Completed,
    Cancelled,
    Failed,
    Shutdown,
}

impl TurnEnd {
    /// How `op` ended, if `event` is its terminal event. Covers the ops the stdio frontends wait
    /// on. The rest end without an event of their own (`Queue`, `Interrupt`, `Resolve`, and
    /// `Shutdown`) or run only in the TUI, which follows them itself.
    pub(crate) fn of(op: &Op, event: &Event) -> Option<Self> {
        match (op, event) {
            (Op::Prompt(_) | Op::Compact { .. } | Op::Resume { .. }, Event::TurnFailed(_))
            | (Op::Rename { .. }, Event::RenameFailed(_))
            // The model has no tier for the effort, so the pick changed nothing.
            | (
                Op::SetModel {
                    model: None,
                    effort: Some(_),
                },
                Event::ConfigChanged { effort: None, .. },
            ) => Some(Self::Failed),
            (Op::Prompt(_), Event::TurnComplete)
            | (Op::Compact { .. }, Event::SessionCompacted { .. })
            | (Op::Resume { .. }, Event::SessionResumed { .. })
            | (Op::Clear, Event::SessionRolled { .. })
            | (Op::Rename { .. }, Event::SessionTitleUpdated { .. })
            | (Op::SetModel { .. }, Event::ConfigChanged { .. })
            | (Op::SetMode(_), Event::ModeChanged(_)) => Some(Self::Completed),
            (Op::Prompt(_) | Op::Compact { .. }, Event::Cancelled) => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Shows `abort` on `sink` as the operation's one terminal event, logging `label` if a failure
/// event is dropped. `Shutdown` stays silent. See `docs/design/tui/cancellation.md` for the set.
fn report_abort(sink: &dyn EventSink, abort: TurnAbort, label: &'static str) -> TurnEnd {
    match abort {
        TurnAbort::Cancelled => {
            sink.emit(Event::Cancelled, "cancelled");
            TurnEnd::Cancelled
        }
        TurnAbort::Shutdown => TurnEnd::Shutdown,
        TurnAbort::Failed(e) => {
            sink.emit(Event::TurnFailed(format!("{e:#}")), label);
            TurnEnd::Failed
        }
    }
}

fn signed_out() -> TurnAbort {
    TurnAbort::Failed(anyhow!(auth::SIGNED_OUT))
}

fn sign_in_failed(e: &anyhow::Error) -> TurnAbort {
    TurnAbort::Failed(anyhow!("Sign-in failed: {e:#}. Run `/login` to try again."))
}

/// Reports that the session a roll or resume left behind failed to finalize.
fn report_finalize_failure(sink: &dyn EventSink, failure: Option<String>) {
    if let Some(failure) = failure {
        sink.emit(
            Event::Error(format!("Previous session write failed: {failure}")),
            "previous-session-finalize-failed",
        );
    }
}

/// Warns that the tracked files in `drifted` changed on disk since the resumed session read them.
fn report_drift(sink: &dyn EventSink, drifted: &[PathBuf]) {
    if drifted.is_empty() {
        return;
    }
    sink.emit(
        Event::Notice(format_drift_warning(drifted)),
        "resume-drift-warning",
    );
}

fn format_drift_warning(drifted: &[PathBuf]) -> String {
    const PREVIEW_CAP: usize = 3;
    let count = drifted.len();
    let preview: Vec<String> = drifted
        .iter()
        .take(PREVIEW_CAP)
        .map(|p| p.display().to_string())
        .collect();
    let suffix = if count > preview.len() {
        format!(", and {} more", count - preview.len())
    } else {
        String::new()
    };
    format!(
        "{count} tracked {file} changed on disk since the resumed session read {it}, \
         so Edit needs a fresh Read first: {}{suffix}",
        preview.join(", "),
        file = plural(count, "file", "files"),
        it = plural(count, "it", "them"),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use indoc::{formatdoc, indoc};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::{
        MODEL, SESSION_CONTEXT, engine, mount_reply, mount_stalled_reply_once,
        mount_tool_call_once, request_bodies, service_log, system_prompt, tool_heavy_transcript,
        until_requested, with_pinned_home,
    };
    use super::*;
    use crate::client::anthropic::testing::{
        DEVICE_ID, api_key, client_for, completion_body, sse_body, sse_response, test_config,
        text_stream_body,
    };
    use crate::config::PromptCacheTtl;
    use crate::config::compaction::{CompactionConfig, test_thresholds};
    use crate::file_tracker::testing::seed_full_read;
    use crate::message::{Replay, Role};
    use crate::permission::rule::Rule;
    use crate::permission::{Mode, PermissionClass, Workspace};
    use crate::protocol::CapturingSink;
    use crate::session::post_compact::synthesize_post_compact_message;
    use crate::session::store::testing::test_store;
    use crate::tool::bash::background::ShellState;
    use crate::tool::bash::background::testing::{shell, start_shell};
    use crate::tool::tasks::TasksTool;
    use crate::tool::tasks::testing::plan;

    /// A toolbox without tools, for an engine that [`Engine::new`] builds.
    fn toolbox() -> Toolbox {
        Toolbox {
            tools: Arc::new(ToolRegistry::new(Vec::new())),
            shells: Arc::default(),
        }
    }

    /// Next event the engine emits that `pick` accepts, skipping the rest.
    async fn next_matching<T>(
        events: &mut mpsc::UnboundedReceiver<Event>,
        mut pick: impl FnMut(Event) -> Option<T>,
    ) -> T {
        loop {
            let event = events.recv().await.expect("the engine stopped early");
            if let Some(found) = pick(event) {
                return found;
            }
        }
    }

    /// Events up to and including the terminal event of the prompt in flight.
    async fn turn_events(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let prompt = Op::Prompt(String::new());
        let mut seen = Vec::new();
        loop {
            let event = events.recv().await.expect("the engine stopped early");
            let ended = TurnEnd::of(&prompt, &event).is_some();
            seen.push(event);
            if ended {
                return seen;
            }
        }
    }

    /// A prompt that pastes two service logs, long enough that its exchange can compact.
    fn pasted_logs() -> String {
        format!(
            "Why do these logs stop?\n{}{}",
            service_log("alpha"),
            service_log("beta")
        )
    }

    async fn submit(handle: &mut EngineHandle, text: &str) -> Vec<Event> {
        handle.ops.send(Op::Prompt(text.to_owned())).await.unwrap();
        turn_events(&mut handle.events).await
    }

    /// The first text block of each message in `request`, empty for a message that opens with
    /// another block.
    fn texts(request: &serde_json::Value) -> Vec<&str> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["content"][0]["text"].as_str().unwrap_or_default())
            .collect()
    }

    /// Stores an oxide-code login under `data`, standing in for `$XDG_DATA_HOME`.
    fn store_ox_login(data: &Path) -> PathBuf {
        let file = data.join("ox").join("credentials.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, credentials_json("ox-token")).unwrap();
        file
    }

    fn credentials_json(access_token: &str) -> String {
        serde_json::json!({
            "claudeAiOauth": { "accessToken": access_token, "expiresAt": 9_999_999_999_999_u64 },
        })
        .to_string()
    }

    /// The URL `/login` announced, once the engine emits it.
    async fn login_url(events: &mut mpsc::UnboundedReceiver<Event>) -> String {
        next_matching(events, |event| match event {
            Event::LoginStarted { url } => Some(url),
            _ => None,
        })
        .await
    }

    /// Returns to the sign-in listener behind `url` with `query` and its `state`, as the browser
    /// does, and reads the reply.
    async fn return_from_browser(url: &str, query: &str) -> String {
        let url = reqwest::Url::parse(url).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap()
        };
        let redirect = reqwest::Url::parse(&param("redirect_uri")).unwrap();
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
            .await
            .unwrap();
        let state = param("state");
        let request = formatdoc! {"
            GET /callback?{query}&state={state} HTTP/1.1\r
            Host: localhost\r
            \r
        "};
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    /// Writes a `CLAUDE.md` that is not UTF-8 into `dir`, returning the warning its load raises.
    fn write_unreadable_instructions(dir: &Path) -> String {
        let path = dir.join("CLAUDE.md");
        std::fs::write(&path, b"\xff\xfe").unwrap();
        let cause = std::fs::read_to_string(&path).unwrap_err();
        format!("Skipped instruction file {}: {cause}", path.display())
    }

    /// Records one exchange in a new session in `store` and finishes it, returning its id.
    async fn finished_session(store: &SessionStore) -> String {
        let session = handle::start(store, MODEL);
        let id = session.session_id().to_owned();
        session
            .record_message(Message::user("earlier prompt"))
            .await;
        session
            .record_message(Message::assistant("earlier reply"))
            .await;
        session.finish(Vec::new()).await;
        id
    }

    // ── EngineHandle::shutdown ──

    #[tokio::test]
    async fn shutdown_reports_a_session_that_cannot_finish() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.session = handle::testing::dead("dead");

            let failure = engine.spawn(&[]).shutdown().await;

            assert!(
                failure.is_some_and(|f| f.starts_with("session writer task has stopped")),
                "a session that cannot finish reports why"
            );
        })
        .await;
    }

    // ── EngineTask::join ──

    #[tokio::test]
    async fn join_ends_a_panicked_engine_without_a_write_failure() {
        let task = EngineTask(tokio::spawn(async { panic!("engine bug") }));

        assert_eq!(task.join().await, None);
    }

    // ── new ──

    #[tokio::test]
    async fn new_prices_recorded_usage_and_replays_a_recorded_context_or_builds_one() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let recorded = engine(server.uri(), &store, dir.path())
                .await
                .prompt
                .session_context();
            let warning = write_unreadable_instructions(dir.path());
            let usage = [UsageRecord {
                model: MODEL.to_owned(),
                cache_ttl: PromptCacheTtl::FiveMin,
                tokens: TokenUsage::new(1_000, 100),
            }];

            let mut config = test_config(server.uri(), api_key(), "claude-sonnet-5-5");
            config.workspace = Workspace::new(dir.path());
            for (context, replayed) in [(Some(recorded), true), (None, false)] {
                let engine = Engine::new(
                    &config,
                    DEVICE_ID.to_owned(),
                    toolbox(),
                    store.clone(),
                    handle::start(&store, "claude-sonnet-5-5"),
                    LoadedTranscript {
                        context,
                        usage: usage.to_vec(),
                        ..LoadedTranscript::default()
                    },
                    Arc::new(FileTracker::default()),
                    false,
                )
                .await
                .unwrap();

                assert!(
                    (engine.total_estimated_cost_usd - 0.006).abs() < 1e-12,
                    "priced at the recorded model's rates: {}",
                    engine.total_estimated_cost_usd,
                );
                assert_eq!(engine.prompt.user_context == SESSION_CONTEXT, replayed);
                assert_eq!(engine.prompt_unrecorded, !replayed);
                let built_warnings = if replayed {
                    Vec::new()
                } else {
                    vec![warning.clone()]
                };
                assert_eq!(engine.instruction_warnings, built_warnings);
                let model = if replayed { MODEL } else { "claude-sonnet-5-5" };
                assert!(
                    engine
                        .prompt
                        .seen
                        .model
                        .contains(&format!("The exact model ID is {model}.")),
                    "replayed: {replayed}",
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn new_takes_the_agent_settings_from_config_and_binds_the_client_to_the_session() {
        with_pinned_home(async {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let session = handle::start(&store, MODEL);
            let mut config = test_config("http://127.0.0.1:9", api_key(), MODEL);
            config.max_tool_rounds = Some(3);
            config.compaction = CompactionConfig::with_threshold_for_test(50_000);
            config.permission = Policy::new(Mode::Plan, Vec::new(), Vec::new(), Vec::new());
            config.workspace = Workspace::new(dir.path());

            let engine = Engine::new(
                &config,
                DEVICE_ID.to_owned(),
                toolbox(),
                store.clone(),
                session.clone(),
                LoadedTranscript::default(),
                Arc::new(FileTracker::default()),
                false,
            )
            .await
            .unwrap();

            assert_eq!(*engine.workspace, config.workspace);
            assert_eq!(engine.max_tool_rounds, Some(3));
            assert_eq!(engine.compaction, config.compaction);
            assert_eq!(engine.policy.mode(), Mode::Plan);
            assert_eq!(engine.client.session_id(), session.session_id());
        })
        .await;
    }

    // ── spawn ──

    #[tokio::test]
    async fn spawn_reports_the_resumed_cost_tasks_warnings_drift_and_an_interrupted_transcript() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let warning = write_unreadable_instructions(dir.path());
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.messages = vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Thinking {
                        thinking: turn::INTERRUPTED_WORK_SENTINEL.to_owned(),
                        replay: Replay::with_model(MODEL, ""),
                    }],
                },
            ];
            engine.total_estimated_cost_usd = 1.23;
            engine.tasks = plan();

            let mut handle = engine.spawn(&[PathBuf::from("a.rs")]);
            let cost = handle.events.recv().await.unwrap();
            let tasks = handle.events.recv().await.unwrap();
            let instructions = handle.events.recv().await.unwrap();
            let drift = handle.events.recv().await.unwrap();
            let interrupted = handle.events.recv().await.unwrap();
            let failure = handle.shutdown().await;

            assert!(
                matches!(
                    &cost,
                    Event::UsageUpdated(UsageSnapshot {
                        context_tokens: None,
                        estimated_cost_usd: Some(cost),
                        ..
                    }) if (cost - 1.23).abs() < f64::EPSILON
                ),
                "{cost:?}"
            );
            assert_eq!(tasks, Event::TasksUpdated(plan()));
            assert_eq!(instructions, Event::Notice(warning));
            assert_eq!(
                drift,
                Event::Notice(format_drift_warning(&[PathBuf::from("a.rs")]))
            );
            assert!(
                matches!(
                    &interrupted,
                    Event::Error(msg) if msg.ends_with("Send a message to continue.")
                ),
                "{interrupted:?}"
            );
            assert_eq!(failure, None);
        })
        .await;
    }

    // ── run ──

    #[tokio::test]
    async fn run_serves_a_turn_and_saves_it_on_shutdown() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let session_id = engine.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            let events = submit(&mut handle, "hello").await;
            let failure = handle.shutdown().await;

            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::StreamToken(token),
                        Event::UsageUpdated(_),
                        Event::TurnComplete,
                    ] if token == "reply"
                ),
                "{events:?}"
            );
            assert_eq!(failure, None);
            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(
                data.messages,
                [Message::user("hello"), Message::assistant("reply")]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_titles_a_fresh_session_in_the_background_when_interactive() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .and(body_partial_json(
                    serde_json::json!({ "model": "claude-haiku-5-5" }),
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(completion_body(r#"{"title":"Greeting"}"#)),
                )
                .mount(&server)
                .await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.interactive = true;
            let live_id = engine.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            handle
                .ops
                .send(Op::Prompt("hello".to_owned()))
                .await
                .unwrap();
            let titled = next_matching(&mut handle.events, |event| match event {
                Event::SessionTitleUpdated { session_id, title } => Some((session_id, title)),
                _ => None,
            })
            .await;
            handle.shutdown().await;

            assert_eq!(titled, (live_id, "Greeting".to_owned()));
        })
        .await;
    }

    #[tokio::test]
    async fn run_splices_a_queued_prompt_in_at_the_next_round_boundary() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_tool_call_once(&server, "call-1", "missing", "{}").await;
            mount_reply(&server, "done").await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let mut handle = engine.spawn(&[]);

            for op in [
                Op::Queue("early".to_owned()),
                Op::Prompt("first".to_owned()),
                Op::Queue("queued".to_owned()),
            ] {
                handle.ops.send(op).await.unwrap();
            }
            let events = turn_events(&mut handle.events).await;
            handle.shutdown().await;

            let drained: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Event::PromptDrained(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                drained,
                ["queued"],
                "a prompt queued between turns is dropped"
            );
            let tool_end = events
                .iter()
                .position(|event| matches!(event, Event::ToolCallEnd { .. }))
                .unwrap();
            assert!(
                matches!(&events[tool_end + 1], Event::PromptDrained(_)),
                "{events:?}"
            );
            let requests = request_bodies(&server).await;
            assert_eq!(requests.len(), 2);
            let messages = requests[1]["messages"].as_array().unwrap();
            assert_eq!(
                messages[messages.len() - 2]["content"][0]["type"],
                "tool_result"
            );
            assert_eq!(texts(&requests[1]).last(), Some(&"queued"));
        })
        .await;
    }

    #[tokio::test]
    async fn run_compact_replaces_the_transcript_with_the_summary() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let session_id = engine.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            submit(&mut handle, &pasted_logs()).await;
            submit(&mut handle, "two").await;
            handle
                .ops
                .send(Op::Compact {
                    instructions: Some("focus".to_owned()),
                })
                .await
                .unwrap();
            let compacted = next_matching(&mut handle.events, |event| match event {
                Event::SessionCompacted {
                    pre_count,
                    instructions,
                    automatic,
                    ..
                } => Some((pre_count, instructions, automatic)),
                _ => None,
            })
            .await;
            submit(&mut handle, "after").await;
            handle.shutdown().await;

            assert_eq!(compacted, (4, Some("focus".to_owned()), false));
            let requests = request_bodies(&server).await;
            let after = texts(&requests[3]);
            assert_eq!(after.len(), 3, "{after:?}");
            assert!(after[1].contains("reply"), "{after:?}");
            assert_eq!(after[2], "after");
            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(
                data.messages[1..],
                [Message::user("after"), Message::assistant("reply")]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_resume_continues_the_session_an_id_prefix_names() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let target_id = finished_session(&store).await;
            let engine = engine(server.uri(), &store, dir.path()).await;
            let mut handle = engine.spawn(&[]);

            let target = ResumeTarget::Prefix(target_id[..8].to_owned());
            handle.ops.send(Op::Resume { target }).await.unwrap();
            let resumed = next_matching(&mut handle.events, |event| match event {
                Event::SessionResumed { id, history, .. } => Some((id, history)),
                _ => None,
            })
            .await;
            submit(&mut handle, "again").await;
            handle.shutdown().await;

            assert_eq!(
                resumed,
                (
                    target_id.clone(),
                    vec![
                        Interaction::UserText("earlier prompt".to_owned()),
                        Interaction::AssistantText("earlier reply".to_owned()),
                    ],
                )
            );
            let requests = request_bodies(&server).await;
            assert_eq!(
                texts(&requests[0])[1..],
                ["earlier prompt", "earlier reply", "again"]
            );
            let data = store.load_session_data(&target_id).unwrap();
            assert_eq!(
                data.messages,
                [
                    Message::user("earlier prompt"),
                    Message::assistant("earlier reply"),
                    Message::user("again"),
                    Message::assistant("reply"),
                ]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_resume_by_a_prefix_only_the_live_session_matches_fails_and_stays_live() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let live_id = engine.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            submit(&mut handle, "first").await;
            let target = ResumeTarget::Prefix(live_id[..8].to_owned());
            handle.ops.send(Op::Resume { target }).await.unwrap();
            let failure = next_matching(&mut handle.events, |event| match event {
                Event::TurnFailed(message) => Some(message),
                _ => None,
            })
            .await;
            submit(&mut handle, "second").await;
            handle.shutdown().await;

            assert_eq!(
                failure,
                format!(
                    "Resume failed (still on session {prefix}): \
                     session prefix `{prefix}` matches the live session",
                    prefix = &live_id[..8],
                ),
            );
            let data = store.load_session_data(&live_id).unwrap();
            assert_eq!(data.messages.len(), 4, "the live session keeps recording");
        })
        .await;
    }

    #[tokio::test]
    async fn run_records_every_request_so_a_resume_restores_the_session_cost() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let session_id = engine.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            submit(&mut handle, &pasted_logs()).await;
            submit(&mut handle, "two").await;
            handle
                .ops
                .send(Op::Compact { instructions: None })
                .await
                .unwrap();
            next_matching(&mut handle.events, |event| {
                matches!(event, Event::SessionCompacted { .. }).then_some(())
            })
            .await;
            submit(&mut handle, "after").await;
            handle.ops.send(Op::Clear).await.unwrap();
            next_matching(&mut handle.events, |event| {
                matches!(event, Event::SessionRolled { .. }).then_some(())
            })
            .await;
            let fresh = submit(&mut handle, "fresh").await;
            let target = ResumeTarget::Path(store.find_session_path(&session_id).unwrap());
            handle.ops.send(Op::Resume { target }).await.unwrap();
            next_matching(&mut handle.events, |event| {
                matches!(event, Event::SessionResumed { .. }).then_some(())
            })
            .await;
            let restored = handle.events.recv().await.unwrap();
            handle.shutdown().await;

            assert!(
                matches!(
                    fresh.as_slice(),
                    [
                        Event::StreamToken(_),
                        Event::UsageUpdated(UsageSnapshot {
                            estimated_cost_usd: Some(cost),
                            ..
                        }),
                        Event::TurnComplete,
                    ] if (cost - 0.006).abs() < 1e-12
                ),
                "a cleared session starts its own cost: {fresh:?}"
            );
            assert!(
                matches!(
                    &restored,
                    Event::UsageUpdated(UsageSnapshot {
                        context_tokens: None,
                        estimated_cost_usd: Some(cost),
                        ..
                    }) if (cost - 4.0 * 0.006).abs() < 1e-12
                ),
                "three turns and the summary: {restored:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_archive_rolls_away_and_a_resume_unarchives_the_session() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let archived_id = engine.session.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            submit(&mut handle, "first").await;
            handle.ops.send(Op::Archive).await.unwrap();
            let (fresh_id, archived) = next_matching(&mut handle.events, |event| match event {
                Event::SessionRolled { id, archived } => Some((id, archived)),
                _ => None,
            })
            .await;
            assert_ne!(fresh_id, archived_id);
            assert!(archived);
            assert!(store.load_session_data(&archived_id).unwrap().archived);

            let target = ResumeTarget::Path(store.find_session_path(&archived_id).unwrap());
            handle.ops.send(Op::Resume { target }).await.unwrap();
            let resumed = next_matching(&mut handle.events, |event| match event {
                Event::SessionResumed { id, unarchived, .. } => Some((id, unarchived)),
                _ => None,
            })
            .await;
            handle.shutdown().await;

            assert_eq!(resumed, (archived_id.clone(), true));
            assert!(!store.load_session_data(&archived_id).unwrap().archived);
        })
        .await;
    }

    #[tokio::test]
    async fn run_applies_rename_and_config_ops_until_shutdown() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let live_id = engine.session_id().to_owned();
            let EngineHandle {
                ops,
                mut events,
                task,
            } = engine.spawn(&[]);

            for op in [
                Op::Rename {
                    title: "Fix login".to_owned(),
                },
                Op::SetModel {
                    model: None,
                    effort: Some(Effort::Low),
                },
                Op::SetMode(Mode::Plan),
                Op::Shutdown,
            ] {
                ops.send(op).await.unwrap();
            }
            let failure = tokio::time::timeout(Duration::from_secs(5), task.join())
                .await
                .expect("Shutdown stops the engine while its op channel is still open");
            let mut seen = Vec::new();
            while let Ok(event) = events.try_recv() {
                seen.push(event);
            }

            assert_eq!(failure, None);
            assert!(
                matches!(
                    seen.as_slice(),
                    [
                        Event::SessionTitleUpdated { session_id, title },
                        Event::ConfigChanged {
                            requested_effort: Some(Effort::Low),
                            ..
                        },
                        Event::ModeChanged(Mode::Plan),
                    ] if *session_id == live_id && title == "Fix login"
                ),
                "{seen:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_shutdown_before_any_prompt_leaves_no_session_file() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let handle = engine(server.uri(), &store, dir.path()).await.spawn(&[]);

            handle.ops.send(Op::Shutdown).await.unwrap();
            assert_eq!(handle.task.join().await, None);

            assert!(store.list().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn run_finalizes_the_live_session_after_a_clear() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let tracker = Arc::clone(&engine.file_tracker);
            let read_path = dir.path().join("read.txt");
            std::fs::write(&read_path, "contents").unwrap();
            let mut handle = engine.spawn(&[]);

            handle.ops.send(Op::Clear).await.unwrap();
            let live_id = next_matching(&mut handle.events, |event| match event {
                Event::SessionRolled {
                    id,
                    archived: false,
                } => Some(id),
                _ => None,
            })
            .await;
            seed_full_read(&tracker, &read_path);
            submit(&mut handle, "after clear").await;
            handle.shutdown().await;

            let snapshots = store.load_session_data(&live_id).unwrap().file_snapshots;
            let paths: Vec<_> = snapshots.iter().map(|snapshot| &snapshot.path).collect();
            assert_eq!(paths, [&read_path]);
        })
        .await;
    }

    #[tokio::test]
    async fn run_reports_the_background_shells_and_ends_them_on_shutdown() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let background = Arc::downgrade(&engine.shells);
            let mut handle = engine.spawn(&[]);
            let leaked = dir.path().join("leaked");
            let command = format!("sleep 2 && touch '{}'", leaked.display());

            start_shell(&background.upgrade().unwrap(), &command);
            let listed = next_matching(&mut handle.events, |event| match event {
                Event::BackgroundShellsUpdated(list) => Some(list),
                _ => None,
            })
            .await;
            handle.shutdown().await;

            assert_eq!(listed, [shell("shell-1", &command, ShellState::Running)]);
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            assert!(!leaked.exists(), "the shell outlived the session");
        })
        .await;
    }

    #[tokio::test]
    async fn run_interrupts_mark_each_abandoned_prompt_before_the_next_one() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_stalled_reply_once(&server).await;
            mount_stalled_reply_once(&server).await;
            mount_reply(&server, "42").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let live_id = engine.session_id().to_owned();
            let mut handle = engine.spawn(&[]);

            let mut cancelled = Vec::new();
            for (count, prompt) in [(1, "write a story"), (2, "write a poem")] {
                handle
                    .ops
                    .send(Op::Prompt(prompt.to_owned()))
                    .await
                    .unwrap();
                until_requested(&server, count).await;
                handle.ops.send(Op::Interrupt).await.unwrap();
                cancelled.extend(turn_events(&mut handle.events).await);
            }
            let next = submit(&mut handle, "what is 7 times 6?").await;
            handle.shutdown().await;

            assert_eq!(cancelled, [Event::Cancelled, Event::Cancelled]);
            assert!(matches!(next.last(), Some(Event::TurnComplete)));
            let requests = request_bodies(&server).await;
            assert_eq!(
                texts(&requests[2])[1..],
                [
                    "write a story",
                    "[Request interrupted by user]",
                    "write a poem",
                    "[Request interrupted by user]",
                    "what is 7 times 6?",
                ]
            );
            let data = store.load_session_data(&live_id).unwrap();
            assert_eq!(
                data.messages,
                [
                    Message::user("write a story"),
                    Message::interruption(),
                    Message::user("write a poem"),
                    Message::interruption(),
                    Message::user("what is 7 times 6?"),
                    Message::assistant("42"),
                ]
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_interrupt_during_auto_compaction_ends_the_prompt_before_the_model_sees_it() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_stalled_reply_once(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            engine.compaction = CompactionConfig::with_threshold_for_test(50_000);
            engine.messages = tool_heavy_transcript();
            engine.last_usage = Some(TokenUsage::new(50_000, 1));
            let mut handle = engine.spawn(&[]);

            handle
                .ops
                .send(Op::Prompt("five".to_owned()))
                .await
                .unwrap();
            until_requested(&server, 1).await;
            handle.ops.send(Op::Interrupt).await.unwrap();
            let events = turn_events(&mut handle.events).await;
            handle.shutdown().await;

            assert!(
                matches!(
                    events.as_slice(),
                    [Event::AutoCompactionStarted, Event::Cancelled]
                ),
                "{events:?}"
            );
            assert_eq!(
                request_bodies(&server).await.len(),
                1,
                "the prompt never reaches the model"
            );
            assert!(
                store.list().unwrap().is_empty(),
                "neither the prompt nor an interruption marker is recorded",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn run_shutdown_mid_request_stops_without_a_terminal_event() {
        with_pinned_home(async {
            for (label, op) in [
                ("prompt", Op::Prompt("mid stream".to_owned())),
                ("compaction", Op::Compact { instructions: None }),
            ] {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.messages = tool_heavy_transcript();
                let EngineHandle {
                    ops,
                    mut events,
                    task,
                } = engine.spawn(&[]);

                ops.send(op).await.unwrap();
                until_requested(&server, 1).await;
                ops.send(Op::Shutdown).await.unwrap();
                let failure = tokio::time::timeout(Duration::from_secs(5), task.join())
                    .await
                    .unwrap_or_else(|_| {
                        panic!("Shutdown stops a {label} while the op channel is open")
                    });

                assert_eq!(failure, None, "{label}");
                assert!(events.try_recv().is_err(), "{label}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn run_finalizes_when_the_op_channel_closes_mid_stream() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_stalled_reply_once(&server).await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let engine = engine(server.uri(), &store, dir.path()).await;
            let live_id = engine.session_id().to_owned();
            let read_path = dir.path().join("read.txt");
            std::fs::write(&read_path, "contents").unwrap();
            seed_full_read(&engine.file_tracker, &read_path);
            let handle = engine.spawn(&[]);

            handle
                .ops
                .send(Op::Prompt("mid stream".to_owned()))
                .await
                .unwrap();
            until_requested(&server, 1).await;
            tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
                .await
                .expect("the engine must stop waiting on the stream");

            let data = store.load_session_data(&live_id).unwrap();
            assert_eq!(
                data.messages,
                [Message::user("mid stream"), Message::interruption()]
            );
            let paths: Vec<_> = data
                .file_snapshots
                .iter()
                .map(|snapshot| &snapshot.path)
                .collect();
            assert_eq!(paths, [&read_path]);
        })
        .await;
    }

    #[tokio::test]
    async fn run_serves_login_and_logout_and_ends_a_sign_in_on_interrupt_or_shutdown() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let EngineHandle {
                    ops,
                    mut events,
                    task,
                } = engine.spawn(&[]);

                ops.send(Op::Login).await.unwrap();
                let url = login_url(&mut events).await;
                ops.send(Op::Interrupt).await.unwrap();
                let cancelled = events.recv().await;
                ops.send(Op::Logout).await.unwrap();
                let signed_out = events.recv().await;
                ops.send(Op::Login).await.unwrap();
                login_url(&mut events).await;
                ops.send(Op::Shutdown).await.unwrap();
                let failure = tokio::time::timeout(Duration::from_secs(5), task.join())
                    .await
                    .expect("Shutdown stops the engine mid sign-in");

                assert!(
                    url.starts_with("https://claude.ai/oauth/authorize?"),
                    "{url}"
                );
                assert!(matches!(cancelled, Some(Event::Cancelled)), "{cancelled:?}");
                assert!(
                    matches!(
                        signed_out,
                        Some(Event::SignedOut {
                            removed: false,
                            auth: Some(AuthSource::ApiKeyEnv),
                            fallback_error: None,
                        })
                    ),
                    "{signed_out:?}"
                );
                assert_eq!(failure, None);
            })
            .await;
        })
        .await;
    }

    // ── submit_prompt ──

    #[tokio::test]
    async fn submit_prompt_keeps_the_prefix_and_sends_a_date_change_as_a_reminder() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let today = engine.prompt.seen.date.clone();
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.submit_prompt("first", &sink, &mut ops).await;
            assert_eq!(end, TurnEnd::Completed);
            "2000-01-01".clone_into(&mut engine.prompt.seen.date);
            let end = engine.submit_prompt("second", &sink, &mut ops).await;
            assert_eq!(end, TurnEnd::Completed);

            let turns = request_bodies(&server).await;
            assert_eq!(
                turns.len(),
                2,
                "non-interactive turns must not request a title"
            );
            assert_eq!(system_prompt(&turns[1]), system_prompt(&turns[0]));
            let (first, second) = (&turns[0]["messages"], &turns[1]["messages"]);
            assert_eq!(first[0]["content"][0]["text"], SESSION_CONTEXT);
            assert_eq!(second[0], first[0], "the first message must not change");
            assert_eq!(second[1]["content"][0]["text"], "first");
            assert_eq!(second[2]["content"][0]["text"], "reply");
            let prompt = second[3]["content"].as_array().unwrap();
            assert_eq!(prompt[0]["text"], "second");
            let reminder = prompt[1]["text"].as_str().unwrap();
            assert!(
                reminder.contains(&format!("Today's date is {today}.")),
                "{reminder}"
            );
            assert!(!reminder.contains("# CLAUDE.md"), "{reminder}");

            let id = engine.session.session_id().to_owned();
            let expected = engine.prompt.session_context();
            engine.finalize().await;
            let data = store.load_session_data(&id).unwrap();
            assert_eq!(data.context, Some(expected));
            assert!(matches!(
                &data.messages[2].content[..],
                [_, ContentBlock::Text { text }] if text == reminder
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_shows_an_instruction_warning_each_time_it_appears() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();
            let warning = write_unreadable_instructions(dir.path());

            engine.submit_prompt("broken", &sink, &mut ops).await;
            engine.submit_prompt("still broken", &sink, &mut ops).await;
            std::fs::write(dir.path().join("CLAUDE.md"), "Fixed rules.").unwrap();
            engine.submit_prompt("fixed", &sink, &mut ops).await;
            write_unreadable_instructions(dir.path());
            engine.submit_prompt("broken again", &sink, &mut ops).await;

            let notices: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Notice(notice) => Some(notice),
                    _ => None,
                })
                .collect();
            assert_eq!(notices, [warning.clone(), warning]);
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_reports_usage_and_accumulates_cost_across_turns() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            for text in ["first", "second"] {
                engine.submit_prompt(text, &sink, &mut ops).await;
            }

            let usage = sink
                .events()
                .into_iter()
                .rev()
                .find_map(|event| match event {
                    Event::UsageUpdated(usage) => Some(usage),
                    _ => None,
                });
            let usage = usage.expect("each turn reports usage");
            assert_eq!(usage.context_tokens, Some(1_000));
            assert_eq!(usage.context_window, Some(1_000_000));
            let cost = usage.estimated_cost_usd.unwrap();
            assert!(
                (cost - 0.012).abs() < 1e-12,
                "two turns at $0.006 each: {cost}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_adopts_the_task_list_a_tasks_call_sets() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let input = serde_json::json!({ "tasks": plan() }).to_string();
            mount_tool_call_once(&server, "call-1", "tasks", &input).await;
            mount_reply(&server, "done").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.submit_prompt("plan it", &sink, &mut ops).await;

            assert_eq!(end, TurnEnd::Completed);
            assert_eq!(engine.tasks, plan());
            let events = sink.events();
            let updated = events
                .iter()
                .position(|event| matches!(event, Event::TasksUpdated(_)))
                .expect("the tasks call reports the new list");
            assert!(
                matches!(&events[updated - 1], Event::ToolCallEnd(result) if result.id == "call-1"),
                "{events:?}"
            );
            assert_eq!(events[updated], Event::TasksUpdated(plan()));
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_keeps_the_list_through_a_later_round_that_sets_none() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let planned = serde_json::json!({ "tasks": plan() }).to_string();
            let rejected = serde_json::json!({ "tasks": [
                {"content": "Fix the parser", "status": "in_progress"},
                {"content": "Fix the lexer", "status": "in_progress"},
            ]})
            .to_string();
            mount_tool_call_once(&server, "call-1", "tasks", &planned).await;
            mount_tool_call_once(&server, "call-2", "tasks", &rejected).await;
            mount_reply(&server, "done").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine
                .submit_prompt("plan it", &CapturingSink::new(), &mut ops)
                .await;

            assert_eq!(end, TurnEnd::Completed);
            assert_eq!(server.received_requests().await.unwrap().len(), 3);
            assert_eq!(engine.tasks, plan());
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_adopts_the_list_of_a_turn_that_fails_after_the_call() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let input = serde_json::json!({ "tasks": plan() }).to_string();
            mount_tool_call_once(&server, "call-1", "tasks", &input).await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
            engine.max_tool_rounds = Some(1);
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine
                .submit_prompt("plan it", &CapturingSink::new(), &mut ops)
                .await;

            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.tasks, plan());
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_auto_compacts_before_recording_the_new_prompt() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "auto summary").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.compaction = CompactionConfig::with_threshold_for_test(50_000);
            engine.messages = tool_heavy_transcript();
            engine.last_usage = Some(TokenUsage::new(50_000, 1));
            engine.displayed_usage = engine.last_usage;
            engine.total_estimated_cost_usd = 1.23;
            engine.auto_compaction_failures = AutoCompactFailures::new(2);
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.submit_prompt("five", &sink, &mut ops).await;

            assert_eq!(end, TurnEnd::Completed);
            assert_eq!(
                engine.auto_compaction_failures,
                AutoCompactFailures::default(),
                "a success resets the failure count"
            );
            let requests = request_bodies(&server).await;
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[0]["messages"][0]["content"][0]["text"], SESSION_CONTEXT,
                "the summary forks the engine's own prompt",
            );
            let turn = requests[1]["messages"].as_array().unwrap();
            assert!(
                turn[1]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("auto summary"),
                "the turn must start from the summary: {turn:?}",
            );
            assert_eq!(turn.last().unwrap()["content"][0]["text"], "five");
            assert!(matches!(
                &engine.messages[0].content[0],
                ContentBlock::Text { text } if text.contains("auto summary")
            ));
            assert_eq!(engine.messages.len(), 3);
            assert_eq!(engine.last_usage, Some(TokenUsage::new(1_000, 100)));
            assert!(
                (engine.total_estimated_cost_usd - (1.23 + 0.006 + 0.006)).abs() < 1e-12,
                "the session cost keeps earlier turns and adds the summary and the turn: {}",
                engine.total_estimated_cost_usd,
            );
            let events = sink.events();
            let compacted = events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        Event::SessionCompacted {
                            automatic: true,
                            ..
                        }
                    )
                })
                .unwrap();
            let Event::UsageUpdated(usage) = &events[compacted + 1] else {
                panic!("the summary's cost shows right after the compaction: {events:?}");
            };
            assert_eq!(usage.context_tokens, None);
            assert!((usage.estimated_cost_usd.unwrap() - (1.23 + 0.006)).abs() < 1e-12);
            let completed = events
                .iter()
                .position(|event| matches!(event, Event::TurnComplete));
            assert!(compacted < completed.unwrap());
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_fails_without_sending_the_prompt_when_auto_compaction_fails() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "   ").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            engine.compaction = CompactionConfig::with_threshold_for_test(50_000);
            engine.messages = tool_heavy_transcript();
            engine.last_usage = Some(TokenUsage::new(50_000, 1));
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.submit_prompt("five", &sink, &mut ops).await;

            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.messages, tool_heavy_transcript());
            assert_eq!(engine.auto_compaction_failures, AutoCompactFailures::new(1));
            assert_eq!(
                request_bodies(&server).await.len(),
                1,
                "only the summary request goes out"
            );
            let data = store.load_session_data(engine.session_id()).unwrap();
            assert_eq!(data.messages, [] as [Message; 0]);
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::AutoCompactionStarted,
                        Event::SummaryToken(token),
                        Event::SummaryDiscarded,
                        Event::UsageUpdated(_),
                        Event::TurnFailed(error),
                    ] if token == "   "
                        && error == "Auto-compaction failed: the model returned an empty summary. \
                                     The prompt was not sent, so send it again to retry."
                ),
                "{events:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_keeps_a_mode_switch_sent_during_a_failed_auto_compaction() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "   ").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.compaction = CompactionConfig::with_threshold_for_test(50_000);
            engine.messages = tool_heavy_transcript();
            engine.last_usage = Some(TokenUsage::new(50_000, 1));
            let sink = CapturingSink::new();
            let (ops_tx, mut ops) = mpsc::channel(1);
            ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();

            let end = engine.submit_prompt("five", &sink, &mut ops).await;

            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.policy.mode(), Mode::Plan);
        })
        .await;
    }

    #[tokio::test]
    async fn submit_prompt_fails_while_signed_out_without_sending_a_request() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.client.set_auth(None);
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.submit_prompt("hello", &sink, &mut ops).await;

            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.messages, [] as [Message; 0]);
            assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::TurnFailed(msg)] if msg == "Not signed in. Run /login to sign in."
                ),
                "{events:?}"
            );
        })
        .await;
    }

    // ── compact ──

    #[tokio::test]
    async fn compact_replaces_the_transcript_bills_the_summary_and_resumes_auto_compaction() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "manual summary").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.messages = tool_heavy_transcript();
            engine.displayed_usage = Some(TokenUsage::new(90_000, 9));
            engine.total_estimated_cost_usd = 1.23;
            engine.auto_compaction_failures = AutoCompactFailures::new(3);
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine
                .compact(&sink, &mut ops, Some("focus".to_owned()))
                .await;

            assert_eq!(end, TurnEnd::Completed);
            assert!(matches!(
                engine.messages.as_slice(),
                [Message { content, .. }] if matches!(
                    &content[0],
                    ContentBlock::Text { text } if text.contains("manual summary")
                )
            ));
            assert_eq!(engine.displayed_usage, None);
            assert!(
                (engine.total_estimated_cost_usd - (1.23 + 0.006)).abs() < 1e-12,
                "the session cost keeps earlier turns and adds the summary: {}",
                engine.total_estimated_cost_usd,
            );
            assert_ne!(
                engine.prompt.user_context, SESSION_CONTEXT,
                "the compacted transcript starts from a fresh user context"
            );
            assert!(engine.prompt_unrecorded);
            assert_eq!(
                engine.auto_compaction_failures,
                AutoCompactFailures::default()
            );
            let events = sink.events();
            let [
                Event::SummaryToken(streamed),
                Event::SessionCompacted {
                    automatic: false,
                    instructions: Some(focus),
                    ..
                },
                Event::AutoCompactionResumed,
                Event::UsageUpdated(usage),
            ] = events.as_slice()
            else {
                panic!("expected the summary, compaction, resume, then cost: {events:?}");
            };
            assert_eq!(streamed, "manual summary");
            assert_eq!(focus, "focus");
            assert_eq!(usage.context_tokens, None);
            assert!((usage.estimated_cost_usd.unwrap() - (1.23 + 0.006)).abs() < 1e-12);
        })
        .await;
    }

    #[tokio::test]
    async fn compact_carries_the_task_list_into_the_summary_and_through_a_resume() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "manual summary").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let session_id = engine.session_id().to_owned();
            engine.messages = tool_heavy_transcript();
            engine.tasks = plan();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.compact(&CapturingSink::new(), &mut ops, None).await;

            assert_eq!(end, TurnEnd::Completed);
            assert_eq!(
                engine.messages,
                [synthesize_post_compact_message(
                    "manual summary",
                    &plan(),
                    &[]
                )]
            );
            assert_eq!(engine.tasks, plan());

            engine.clear(&CapturingSink::new()).await;
            let compacted = store.find_session_path(&session_id).unwrap();
            engine
                .resume(&CapturingSink::new(), ResumeTarget::Path(compacted))
                .await;

            assert_eq!(engine.session_id(), session_id);
            assert_eq!(engine.tasks, plan());
        })
        .await;
    }

    #[tokio::test]
    async fn compact_lists_the_open_background_shells_in_the_summary_message() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "manual summary").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            engine.messages = tool_heavy_transcript();
            start_shell(&engine.shells, "sleep 30");
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.compact(&CapturingSink::new(), &mut ops, None).await;

            assert_eq!(end, TurnEnd::Completed);
            assert_eq!(
                engine.messages,
                [synthesize_post_compact_message(
                    "manual summary",
                    &[],
                    &[shell("shell-1", "sleep 30", ShellState::Running)],
                )]
            );
            engine.shells.stop_all().await;
        })
        .await;
    }

    #[tokio::test]
    async fn compact_failure_or_cancel_keeps_the_transcript() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "summary").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (ops_tx, mut ops) = mpsc::channel(1);

            engine.messages = vec![Message::user("too short")];
            let end = engine.compact(&sink, &mut ops, None).await;
            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.messages, [Message::user("too short")]);

            engine.messages = tool_heavy_transcript();
            ops_tx.send(Op::Interrupt).await.unwrap();
            let end = engine.compact(&sink, &mut ops, None).await;
            assert_eq!(end, TurnEnd::Cancelled);
            assert_eq!(engine.messages, tool_heavy_transcript());

            engine.session = handle::testing::dead("dead");
            let end = engine.compact(&sink, &mut ops, None).await;
            assert_eq!(
                end,
                TurnEnd::Failed,
                "an unwritten boundary is a failed compaction"
            );
            assert_eq!(engine.messages, tool_heavy_transcript());

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::TurnFailed(short),
                        Event::Cancelled,
                        Event::SummaryToken(_),
                        Event::Error(usage_write),
                        Event::SummaryDiscarded,
                        Event::Error(boundary_write),
                        Event::UsageUpdated(UsageSnapshot {
                            context_tokens: None,
                            estimated_cost_usd: Some(cost),
                            ..
                        }),
                        Event::TurnFailed(unsaved),
                    ] if short == "Compaction failed: the conversation has about 15 tokens, \
                                   below the 1000-token minimum"
                        && usage_write.starts_with("Session write failed")
                        && boundary_write == "Session write failed: session writer task has stopped"
                        && (cost - 0.006).abs() < 1e-12
                        && unsaved == "Compaction was not saved, so the transcript is unchanged"
                ),
                "{events:?}",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn compact_fails_while_signed_out_without_sending_a_request() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "summary").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.client.set_auth(None);
            engine.messages = tool_heavy_transcript();
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.compact(&sink, &mut ops, None).await;

            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.messages, tool_heavy_transcript());
            assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::TurnFailed(msg)] if msg == "Not signed in. Run /login to sign in."
                ),
                "{events:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn compact_keeps_a_mode_switch_sent_before_a_cancel() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.messages = tool_heavy_transcript();
            let sink = CapturingSink::new();
            let (ops_tx, mut ops) = mpsc::channel(2);
            ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
            ops_tx.try_send(Op::Interrupt).unwrap();

            let end = engine.compact(&sink, &mut ops, None).await;

            assert_eq!(end, TurnEnd::Cancelled);
            assert_eq!(engine.policy.mode(), Mode::Plan);
        })
        .await;
    }

    #[tokio::test]
    async fn compact_cancelled_mid_summary_discards_it_before_the_cancel() {
        with_pinned_home(async {
            let base_url = serve_stalled_summary("partial").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(base_url, &test_store(dir.path()), dir.path()).await;
            engine.messages = tool_heavy_transcript();
            let sink = CapturingSink::new();
            let (ops_tx, mut ops) = mpsc::channel(1);
            let interrupt_once_streaming = async {
                while sink.events().is_empty() {
                    tokio::task::yield_now().await;
                }
                ops_tx.send(Op::Interrupt).await.unwrap();
            };

            let (end, ()) = tokio::join!(
                engine.compact(&sink, &mut ops, None),
                interrupt_once_streaming,
            );

            assert_eq!(end, TurnEnd::Cancelled);
            assert_eq!(engine.messages, tool_heavy_transcript());
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::SummaryToken(partial),
                        Event::SummaryDiscarded,
                        Event::UsageUpdated(_),
                        Event::Cancelled,
                    ] if partial == "partial"
                ),
                "{events:?}",
            );
        })
        .await;
    }

    /// Serves one summary that streams `text` and then stalls with the connection open. Returns
    /// the base URL.
    async fn serve_stalled_summary(text: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let body = sse_body(&[
            serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL, "usage": {"input_tokens": 1_000, "output_tokens": 0}}}),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        ]);
        let declared = body.len() + 1;
        let response = formatdoc! {"
            HTTP/1.1 200 OK\r
            content-type: text/event-stream\r
            content-length: {declared}\r
            \r
            {body}"
        };
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            _ = socket.read(&mut [0; 1024]).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            std::future::pending::<()>().await;
        });
        base_url
    }

    #[tokio::test]
    async fn compact_failure_bills_and_shows_the_summary_cost() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(
                    text_stream_body("", "partial").replace("end_turn", "max_tokens"),
                ))
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.messages = tool_heavy_transcript();
            engine.displayed_usage = Some(TokenUsage::new(50_000, 1));
            engine.total_estimated_cost_usd = 1.23;
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            let end = engine.compact(&sink, &mut ops, None).await;

            assert_eq!(end, TurnEnd::Failed);
            assert_eq!(engine.messages, tool_heavy_transcript());
            assert_eq!(engine.displayed_usage, Some(TokenUsage::new(50_000, 1)));
            let expected = 1.23 + 0.006;
            assert!(
                (engine.total_estimated_cost_usd - expected).abs() < 1e-12,
                "a rejected summary is still billed: {}",
                engine.total_estimated_cost_usd,
            );
            let events = sink.events();
            let [
                Event::SummaryToken(partial),
                Event::SummaryDiscarded,
                Event::UsageUpdated(usage),
                Event::TurnFailed(msg),
            ] = events.as_slice()
            else {
                panic!(
                    "expected the discarded summary and its cost before the failure: {events:?}"
                );
            };
            assert_eq!(partial, "partial");
            assert_eq!(usage.context_tokens, Some(50_000));
            assert!((usage.estimated_cost_usd.unwrap() - expected).abs() < 1e-12);
            assert!(
                msg.starts_with("Compaction failed") && msg.contains("max_tokens"),
                "{msg}"
            );
        })
        .await;
    }

    // ── clear ──

    #[tokio::test]
    async fn clear_and_resume_rebuild_session_prompt_and_drop_session_rules() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let sink = CapturingSink::new();
            let target_id = finished_session(&store).await;
            engine.messages = vec![Message::user("before clear")];
            let rule = Rule::parse(PermissionClass::Exec, "cargo test *").unwrap();
            engine.policy.allow_for_session(rule.clone());

            engine.clear(&sink).await;
            assert_eq!(engine.messages, []);
            assert_eq!(engine.policy.session_allow(), []);
            assert_ne!(engine.prompt.user_context, SESSION_CONTEXT);
            assert!(engine.prompt.user_context.contains("# Current date"));
            assert_eq!(engine.client.session_id(), engine.session.session_id());

            SESSION_CONTEXT.clone_into(&mut engine.prompt.user_context);
            engine.total_estimated_cost_usd = 1.23;
            engine.policy.allow_for_session(rule);
            let target_path = store.find_session_path(&target_id).unwrap();
            engine.resume(&sink, ResumeTarget::Path(target_path)).await;
            assert_eq!(engine.policy.session_allow(), []);
            assert_eq!(engine.session.session_id(), target_id);
            assert_eq!(engine.client.session_id(), target_id);
            assert_eq!(engine.messages.len(), 2);
            assert_ne!(engine.prompt.user_context, SESSION_CONTEXT);
            assert!(
                engine.total_estimated_cost_usd.abs() < f64::EPSILON,
                "a session that recorded no usage resumes at zero"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clear_reports_the_finalize_failure_and_instruction_warnings_after_rolling() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.session = handle::testing::dead("dead");
            let warning = write_unreadable_instructions(dir.path());
            let sink = CapturingSink::new();

            engine.clear(&sink).await;

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::SessionRolled {
                            archived: false,
                            ..
                        },
                        Event::Error(failure),
                        Event::Notice(notice),
                    ] if failure == "Previous session write failed: \
                                     session writer task has stopped. \
                                     Conversation history may be incomplete."
                        && *notice == warning
                ),
                "{events:?}",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clear_and_resume_end_a_paused_auto_compaction() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let target_id = finished_session(&store).await;
            let target_path = store.find_session_path(&target_id).unwrap();
            let clear_sink = CapturingSink::new();
            let resume_sink = CapturingSink::new();

            engine.auto_compaction_failures = AutoCompactFailures::new(3);
            engine.clear(&clear_sink).await;
            let after_clear = engine.auto_compaction_failures;
            engine.auto_compaction_failures = AutoCompactFailures::new(3);
            engine
                .resume(&resume_sink, ResumeTarget::Path(target_path))
                .await;

            assert_eq!(after_clear, AutoCompactFailures::default());
            assert_eq!(
                engine.auto_compaction_failures,
                AutoCompactFailures::default()
            );
            let cleared = clear_sink.events();
            assert!(
                matches!(
                    cleared.as_slice(),
                    [
                        Event::AutoCompactionResumed,
                        Event::SessionRolled {
                            archived: false,
                            ..
                        },
                    ]
                ),
                "{cleared:?}",
            );
            let resumed = resume_sink.events();
            assert!(
                matches!(
                    resumed.as_slice(),
                    [Event::AutoCompactionResumed, Event::SessionResumed { id, .. }]
                        if *id == target_id
                ),
                "{resumed:?}",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn clear_drops_the_task_list_and_resume_restores_it_after_the_session() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let input = serde_json::json!({ "tasks": plan() }).to_string();
            mount_tool_call_once(&server, "call-1", "tasks", &input).await;
            mount_reply(&server, "done").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path())
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
            let planned_id = engine.session_id().to_owned();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();
            engine
                .submit_prompt("plan it", &CapturingSink::new(), &mut ops)
                .await;

            engine.clear(&CapturingSink::new()).await;
            assert_eq!(engine.tasks, []);

            let sink = CapturingSink::new();
            let planned_path = store.find_session_path(&planned_id).unwrap();
            engine.resume(&sink, ResumeTarget::Path(planned_path)).await;
            assert_eq!(engine.tasks, plan());
            let events = sink.events();
            let resumed = events
                .iter()
                .position(|event| matches!(event, Event::SessionResumed { .. }))
                .expect("the resume succeeds");
            let restored = events
                .iter()
                .position(|event| *event == Event::TasksUpdated(plan()))
                .expect("the resume shows the restored list");
            assert!(resumed < restored, "{events:?}");
        })
        .await;
    }

    #[tokio::test]
    async fn clear_and_resume_end_the_background_shells() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let target_id = finished_session(&store).await;
            let target_path = store.find_session_path(&target_id).unwrap();

            start_shell(&engine.shells, "sleep 30");
            engine.clear(&CapturingSink::new()).await;
            let after_clear = engine.shells.list();
            start_shell(&engine.shells, "sleep 30");
            engine
                .resume(&CapturingSink::new(), ResumeTarget::Path(target_path))
                .await;

            assert_eq!(after_clear, []);
            assert_eq!(engine.shells.list(), []);
        })
        .await;
    }

    // ── archive ──

    #[tokio::test]
    async fn archive_keeps_the_session_without_messages_or_after_each_failed_write() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let live_id = engine.session.session_id().to_owned();
            let sink = CapturingSink::new();

            engine.archive(&sink).await;
            assert_eq!(engine.session.session_id(), live_id);
            engine.messages = vec![Message::user("unsaved")];
            engine.session = handle::testing::dead("dead");
            engine.archive(&sink).await;
            engine.archive(&sink).await;
            assert_eq!(engine.session.session_id(), "dead");

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::Error(empty), Event::Error(first), Event::Error(again)]
                        if empty.starts_with("Nothing to archive")
                            && first.starts_with("Archive failed: ")
                            && again.starts_with("Archive failed: ")
                ),
                "{events:?}",
            );
        })
        .await;
    }

    // ── resume ──

    #[tokio::test]
    async fn resume_replays_the_recorded_prompt() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let recorded_id = engine.session.session_id().to_owned();
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();
            engine.submit_prompt("first", &sink, &mut ops).await;
            engine.clear(&sink).await;

            let recorded_path = store.find_session_path(&recorded_id).unwrap();
            engine
                .resume(&sink, ResumeTarget::Path(recorded_path))
                .await;
            assert_eq!(engine.prompt.user_context, SESSION_CONTEXT);
            assert!(!engine.prompt_unrecorded);
            engine.submit_prompt("second", &sink, &mut ops).await;

            let turns = request_bodies(&server).await;
            assert_eq!(system_prompt(&turns[1]), system_prompt(&turns[0]));
            assert_eq!(turns[1]["messages"][0], turns[0]["messages"][0]);
            assert_eq!(turns[1]["messages"][1]["content"][0]["text"], "first");
        })
        .await;
    }

    #[tokio::test]
    async fn resume_shows_and_replays_an_interruption() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "42").await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let interrupted_id = engine.session_id().to_owned();
            let sink = CapturingSink::new();
            let (ops_tx, mut ops) = mpsc::channel(1);
            ops_tx.send(Op::Interrupt).await.unwrap();
            engine.submit_prompt("write a story", &sink, &mut ops).await;
            engine.clear(&sink).await;
            let path = store.find_session_path(&interrupted_id).unwrap();

            engine.resume(&sink, ResumeTarget::Path(path)).await;
            engine
                .submit_prompt("what is 7 times 6?", &sink, &mut ops)
                .await;

            let history = sink.events().into_iter().find_map(|event| match event {
                Event::SessionResumed { history, .. } => Some(history),
                _ => None,
            });
            assert_eq!(
                history,
                Some(vec![
                    Interaction::UserText("write a story".to_owned()),
                    Interaction::Interrupted,
                ])
            );
            let request = &request_bodies(&server).await[0];
            let replayed: Vec<&str> = request["messages"][1]["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|block| block["text"].as_str().unwrap())
                .collect();
            assert_eq!(replayed, ["write a story", "[Request interrupted by user]"]);
            assert_eq!(texts(request)[2..], ["what is 7 times 6?"]);
        })
        .await;
    }

    #[tokio::test]
    async fn resume_reports_the_old_session_finalize_failure_after_switching() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let target_id = finished_session(&store).await;
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            engine.session = handle::testing::dead("dead");
            let sink = CapturingSink::new();

            let target_path = store.find_session_path(&target_id).unwrap();
            engine.resume(&sink, ResumeTarget::Path(target_path)).await;

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::SessionResumed { id, .. }, Event::Error(msg)]
                        if *id == target_id
                            && msg == "Previous session write failed: \
                                       session writer task has stopped. \
                                       Conversation history may be incomplete."
                ),
                "{events:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn resume_failure_preserves_current_session_state() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let original_id = engine.session.session_id().to_owned();
            let last_usage = TokenUsage::new(100_000, 10);
            let displayed_usage = TokenUsage::new(90_000, 9);
            engine.messages = vec![Message::user("still here")];
            engine.auto_compaction_failures = AutoCompactFailures::new(2);
            engine.last_usage = Some(last_usage);
            engine.displayed_usage = Some(displayed_usage);
            engine.total_estimated_cost_usd = 1.23;
            let rule = Rule::parse(PermissionClass::Exec, "cargo test *").unwrap();
            engine.policy.allow_for_session(rule.clone());
            start_shell(&engine.shells, "sleep 30");
            let sink = CapturingSink::new();

            engine
                .resume(&sink, ResumeTarget::Path("missing-target.jsonl".into()))
                .await;

            assert_eq!(engine.session.session_id(), original_id);
            assert_eq!(engine.messages.len(), 1);
            assert_eq!(engine.auto_compaction_failures, AutoCompactFailures::new(2));
            assert_eq!(engine.last_usage, Some(last_usage));
            assert_eq!(engine.displayed_usage, Some(displayed_usage));
            assert!((engine.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON);
            assert_eq!(engine.policy.session_allow(), [rule]);
            assert_eq!(
                engine.shells.list(),
                [shell("shell-1", "sleep 30", ShellState::Running)]
            );
            assert_eq!(
                sink.events(),
                [Event::TurnFailed(format!(
                    "Resume failed (still on session {}): \
                     session not found: missing-target.jsonl: {}",
                    id_prefix(&original_id),
                    std::fs::metadata("missing-target.jsonl").unwrap_err(),
                ))]
            );
            engine.shells.stop_all().await;
        })
        .await;
    }

    // ── rename ──

    #[tokio::test]
    async fn rename_announces_only_a_persisted_title() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let live_id = engine.session.session_id().to_owned();
            let sink = CapturingSink::new();

            engine.rename(&sink, "Fix login".to_owned()).await;
            assert!(engine.session.manual_title_set());
            engine.session = handle::testing::dead("dead");
            engine.rename(&sink, "Lost".to_owned()).await;

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::SessionTitleUpdated { session_id, title },
                        Event::RenameFailed(write),
                    ] if *session_id == live_id && title == "Fix login"
                        && write.starts_with("Session write failed")
                ),
                "{events:?}",
            );
        })
        .await;
    }

    // ── set_model ──

    #[tokio::test]
    async fn set_model_swap_keeps_the_system_prompt_and_names_the_model_next_turn() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            engine.submit_prompt("first", &sink, &mut ops).await;
            engine.set_model(
                &sink,
                Some(ResolvedModelId::new("claude-sonnet-5-5".to_owned())),
                None,
            );
            engine.submit_prompt("second", &sink, &mut ops).await;

            let turns = request_bodies(&server).await;
            assert_eq!(turns[1]["model"], "claude-sonnet-5-5");
            assert_eq!(system_prompt(&turns[1]), system_prompt(&turns[0]));
            let (first, second) = (&turns[0]["messages"], &turns[1]["messages"]);
            assert_eq!(second[0], first[0], "the first message must not change");
            let prompt = second[3]["content"].as_array().unwrap();
            assert_eq!(prompt[0]["text"], "second");
            let reminder = prompt[1]["text"].as_str().unwrap();
            assert!(
                reminder.contains(indoc! {"
                    # Model

                    You are powered by the model named Claude Sonnet 5.5. The exact model ID is claude-sonnet-5-5."
                }),
                "{reminder}"
            );
            assert!(!reminder.contains("# CLAUDE.md"), "{reminder}");
        })
        .await;
    }

    #[tokio::test]
    async fn set_model_swap_keeps_sending_back_the_reasoning_of_the_previous_model() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let reply = sse_body(&[
                serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL}}),
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "plan"}}),
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
                serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": "reply"}}),
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                serde_json::json!({"type": "message_stop"}),
            ]);
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(reply))
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (_ops_tx, mut ops) = crate::protocol::inert_op_channel();

            engine.submit_prompt("first", &sink, &mut ops).await;
            engine.set_model(
                &sink,
                Some(ResolvedModelId::new("claude-sonnet-5-5".to_owned())),
                None,
            );
            engine.submit_prompt("second", &sink, &mut ops).await;

            let turns = request_bodies(&server).await;
            let assistant_turns: Vec<&serde_json::Value> = turns[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "assistant")
                .collect();
            assert_eq!(turns[1]["model"], "claude-sonnet-5-5");
            assert_eq!(
                assistant_turns,
                [&serde_json::json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan", "signature": "sig"},
                    {"type": "text", "text": "reply"},
                ]})],
            );
        })
        .await;
    }

    #[tokio::test]
    async fn set_model_resolves_the_compaction_threshold_for_the_new_model() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let mut config = test_config(server.uri(), api_key(), "custom-model");
            let sink = CapturingSink::new();
            let unwindowed = CompactionConfig::default_for_test(&config.model, 16_000);
            assert!(!unwindowed.auto.enabled, "precondition: no window");
            let mut resolved = Vec::new();

            for (max_tokens, configured, threshold) in [
                (None, unwindowed, test_thresholds::WINDOW_1M),
                (
                    None,
                    CompactionConfig::with_threshold_for_test(1_500_000),
                    test_thresholds::WINDOW_1M,
                ),
                (Some(8_000), unwindowed, 979_000),
            ] {
                config.max_tokens = max_tokens;
                engine.client = client_for(&config);
                engine.compaction = configured;

                engine.set_model(
                    &sink,
                    Some(ResolvedModelId::new("claude-sonnet-5-5".to_owned())),
                    None,
                );

                assert_eq!(
                    engine.compaction.auto.threshold_tokens,
                    Some(threshold),
                    "{max_tokens:?}, {configured:?}",
                );
                resolved.push(engine.compaction.auto);
            }
            let announced: Vec<_> = sink
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    Event::ConfigChanged {
                        auto_compaction, ..
                    } => Some(auto_compaction),
                    _ => None,
                })
                .collect();
            assert_eq!(announced, resolved);
        })
        .await;
    }

    #[tokio::test]
    async fn set_model_resumes_paused_auto_compaction() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            engine.auto_compaction_failures = AutoCompactFailures::new(3);
            engine.last_usage = Some(TokenUsage::new(100_000, 1));
            engine.total_estimated_cost_usd = 1.23;
            let sink = CapturingSink::new();

            engine.set_model(&sink, None, Some(Effort::Xhigh));

            assert_eq!(
                engine.auto_compaction_failures,
                AutoCompactFailures::default()
            );
            assert_eq!(engine.last_usage, Some(TokenUsage::new(100_000, 1)));
            assert!(
                (engine.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON,
                "session cost must accumulate across swaps: {}",
                engine.total_estimated_cost_usd,
            );
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::ConfigChanged { .. },
                        Event::AutoCompactionResumed,
                        Event::UsageUpdated(_),
                    ]
                ),
                "{events:?}",
            );
        })
        .await;
    }

    // ── set_mode ──

    #[tokio::test]
    async fn set_mode_switches_the_gate_and_confirms() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();

            engine.set_mode(&sink, Mode::Yolo);

            assert_eq!(engine.policy.mode(), Mode::Yolo);
            assert!(
                matches!(sink.events().as_slice(), [Event::ModeChanged(Mode::Yolo)]),
                "{:?}",
                sink.events()
            );
        })
        .await;
    }

    // ── adopt_mode ──

    #[tokio::test]
    async fn adopt_mode_keeps_a_switch_that_arrives_mid_turn() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (tx, mut ops) = mpsc::channel(1);
            tx.try_send(Op::SetMode(Mode::Plan)).unwrap();

            engine.submit_prompt("hello", &sink, &mut ops).await;

            assert_eq!(engine.policy.mode(), Mode::Plan);
            assert!(
                matches!(sink.events().first(), Some(Event::ModeChanged(Mode::Plan))),
                "{:?}",
                sink.events()
            );
            drop(tx);
        })
        .await;
    }

    // ── login ──

    #[tokio::test]
    async fn login_stores_the_browser_s_login_and_sends_it_with_the_next_prompt() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .and(body_partial_json(
                    serde_json::json!({ "code": "auth-code" }),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "fresh-access",
                    "refresh_token": "fresh-refresh",
                    "expires_in": 3600,
                    "scope": "user:inference",
                })))
                .expect(1)
                .mount(&server)
                .await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.client.set_auth(None);
                engine.oauth_token_url = format!("{}/token", server.uri());
                let mut handle = engine.spawn(&[]);

                handle.ops.send(Op::Login).await.unwrap();
                let url = login_url(&mut handle.events).await;
                let reply = return_from_browser(&url, "code=auth-code").await;
                let signed_in = handle.events.recv().await;
                let turn = submit(&mut handle, "hi").await;
                handle.shutdown().await;

                assert!(reply.starts_with("HTTP/1.1 302 Found\r\n"), "{reply}");
                assert!(
                    matches!(
                        signed_in,
                        Some(Event::SignedIn {
                            auth: AuthSource::OxLogin
                        })
                    ),
                    "{signed_in:?}"
                );
                assert!(matches!(turn.last(), Some(Event::TurnComplete)), "{turn:?}");
                let requests = server.received_requests().await.unwrap();
                let prompt = requests
                    .iter()
                    .find(|request| request.url.path() == "/v1/messages")
                    .expect("the prompt reaches the API");
                assert_eq!(prompt.headers["authorization"], "Bearer fresh-access");
                assert!(data.path().join("ox").join("credentials.json").exists());
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn login_fails_the_sign_in_when_the_browser_returns_a_refusal() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let mut handle = engine.spawn(&[]);

            handle.ops.send(Op::Login).await.unwrap();
            let url = login_url(&mut handle.events).await;
            let reply = return_from_browser(&url, "error=access_denied").await;
            let failed = handle.events.recv().await;
            handle.shutdown().await;

            assert!(reply.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{reply}");
            assert!(
                matches!(
                    &failed,
                    Some(Event::TurnFailed(msg))
                        if msg == "Sign-in failed: authorization was refused (access_denied). \
                                   Run `/login` to try again."
                ),
                "{failed:?}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn login_keeps_a_mode_switch_sent_before_a_cancel() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let (ops_tx, mut ops) = mpsc::channel(2);
            ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
            ops_tx.try_send(Op::Interrupt).unwrap();

            let end = engine.login(&sink, &mut ops).await;

            assert_eq!(end, TurnEnd::Cancelled);
            assert_eq!(engine.policy.mode(), Mode::Plan);
        })
        .await;
    }

    // ── adopt_login ──

    #[tokio::test]
    async fn adopt_login_replaces_a_stored_login_or_none_but_not_an_explicit_credential() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let ox_login = || Auth::new(AuthSource::OxLogin, "ox-token");

            engine.adopt_login(&sink, ox_login());
            assert_eq!(engine.client.auth_source(), Some(AuthSource::ApiKeyEnv));
            for before in [
                None,
                Some(Auth::new(AuthSource::ClaudeCodeLogin, "claude-code-token")),
            ] {
                engine.client.set_auth(before);
                engine.adopt_login(&sink, ox_login());
                assert_eq!(engine.client.auth().unwrap().secret(), "ox-token");
            }

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::SignedIn {
                            auth: AuthSource::ApiKeyEnv
                        },
                        Event::SignedIn {
                            auth: AuthSource::OxLogin
                        },
                        Event::SignedIn {
                            auth: AuthSource::OxLogin
                        },
                    ]
                ),
                "{events:?}"
            );
        })
        .await;
    }

    // ── logout ──

    #[tokio::test]
    async fn logout_removes_the_oxide_code_login_and_keeps_an_explicit_credential() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            let file = store_ox_login(data.path());
            temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::new();

                engine.logout(&sink).await;

                assert!(!file.exists());
                assert_eq!(engine.client.auth_source(), Some(AuthSource::ApiKeyEnv));
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [Event::SignedOut {
                            removed: true,
                            auth: Some(AuthSource::ApiKeyEnv),
                            fallback_error: None,
                        }]
                    ),
                    "{events:?}"
                );
            })
            .await;
        })
        .await;
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn logout_falls_back_to_the_claude_code_login_or_stays_signed_out() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            store_ox_login(data.path());
            temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine
                    .client
                    .set_auth(Some(Auth::new(AuthSource::OxLogin, "ox-token")));
                let sink = CapturingSink::new();
                let claude_code_file = std::env::home_dir()
                    .unwrap()
                    .join(".claude")
                    .join(".credentials.json");

                engine.logout(&sink).await;
                std::fs::create_dir(claude_code_file.parent().unwrap()).unwrap();
                std::fs::write(&claude_code_file, "not json").unwrap();
                engine.logout(&sink).await;
                std::fs::write(&claude_code_file, credentials_json("claude-code-token")).unwrap();
                engine.logout(&sink).await;

                assert_eq!(engine.client.auth().unwrap().secret(), "claude-code-token");
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SignedOut {
                                removed: true,
                                auth: None,
                                fallback_error: None,
                            },
                            Event::SignedOut {
                                removed: false,
                                auth: None,
                                fallback_error: Some(unreadable),
                            },
                            Event::SignedOut {
                                removed: false,
                                auth: Some(AuthSource::ClaudeCodeLogin),
                                fallback_error: None,
                            },
                        ] if unreadable.ends_with("Run `claude` to sign in again.")
                    ),
                    "{events:?}"
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn logout_reports_a_login_it_cannot_remove() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(data.path().join("ox").join("credentials.json")).unwrap();
            temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::new();

                engine.logout(&sink).await;

                assert_eq!(engine.client.auth_source(), Some(AuthSource::ApiKeyEnv));
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [Event::Error(msg)] if msg.starts_with("Sign-out failed: failed to remove ")
                    ),
                    "{events:?}"
                );
            })
            .await;
        })
        .await;
    }

    // ── estimate_cost_usd ──

    #[test]
    fn estimate_cost_usd_prices_each_request_by_its_own_model_and_cache_ttl() {
        let request = |model: &str, cache_ttl: &str| -> UsageRecord {
            serde_json::from_value(serde_json::json!({
                "model": model,
                "cache_ttl": cache_ttl,
                "input_tokens": 0,
                "cache_creation_input_tokens": 1_000_000,
                "cache_read_input_tokens": 0,
                "output_tokens": 0,
            }))
            .unwrap()
        };

        let cost = estimate_cost_usd(&[
            request("claude-opus-5-5", "1h"),
            request("claude-sonnet-5-5", "5m"),
            request("unpriced-model", "5m"),
        ]);

        assert!(
            (cost - (8.0 + 2.5)).abs() < 1e-9,
            "a 1h write on Opus, a 5m write on Sonnet, and nothing for an unknown model: {cost}"
        );
    }

    // ── TurnEnd::of ──

    #[test]
    fn of_reads_only_the_terminal_events_of_the_op_in_flight() {
        let prompt = Op::Prompt("hi".to_owned());
        let compact = Op::Compact { instructions: None };
        let resume = Op::Resume {
            target: ResumeTarget::Prefix("1a2b".to_owned()),
        };
        let rename = Op::Rename {
            title: "Fix login".to_owned(),
        };
        let set_model = Op::SetModel {
            model: None,
            effort: Some(Effort::Low),
        };
        let set_mode = Op::SetMode(Mode::Plan);
        let compacted = |automatic| Event::SessionCompacted {
            summary: "summary".to_owned(),
            pre_count: 4,
            instructions: None,
            automatic,
        };
        let rolled = Event::SessionRolled {
            id: "new".to_owned(),
            archived: false,
        };
        let notice = Event::Error("Session write failed".to_owned());
        let failed = Event::TurnFailed("failure".to_owned());

        for (op, event, end) in [
            (&prompt, Event::TurnComplete, Some(TurnEnd::Completed)),
            (&prompt, Event::Cancelled, Some(TurnEnd::Cancelled)),
            (&prompt, failed.clone(), Some(TurnEnd::Failed)),
            (&prompt, compacted(true), None),
            (&prompt, notice.clone(), None),
            (&compact, compacted(false), Some(TurnEnd::Completed)),
            (&compact, Event::Cancelled, Some(TurnEnd::Cancelled)),
            (&compact, Event::TurnComplete, None),
            (
                &resume,
                Event::SessionResumed {
                    id: "1a2b".to_owned(),
                    title: None,
                    unarchived: false,
                    history: Vec::new(),
                },
                Some(TurnEnd::Completed),
            ),
            (&resume, failed.clone(), Some(TurnEnd::Failed)),
            (&Op::Clear, notice.clone(), None),
            (&Op::Clear, rolled.clone(), Some(TurnEnd::Completed)),
            (
                &rename,
                Event::SessionTitleUpdated {
                    session_id: "live".to_owned(),
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
                    model: ModelSpec::resolve(MODEL.to_owned()),
                    effort: Some(Effort::Low),
                    requested_effort: Some(Effort::Low),
                    auto_compaction: CompactionConfig::with_threshold_for_test(50_000).auto,
                },
                Some(TurnEnd::Completed),
            ),
            (
                &set_model,
                Event::ConfigChanged {
                    model: ModelSpec::resolve("custom-model".to_owned()),
                    effort: None,
                    requested_effort: Some(Effort::Low),
                    auto_compaction: CompactionConfig::with_threshold_for_test(50_000).auto,
                },
                Some(TurnEnd::Failed),
            ),
            (&set_model, failed, None),
            (
                &set_mode,
                Event::ModeChanged(Mode::Plan),
                Some(TurnEnd::Completed),
            ),
            (&set_mode, notice.clone(), None),
            (&Op::Archive, rolled, None),
        ] {
            assert_eq!(TurnEnd::of(op, &event), end, "{op:?} then {event:?}");
        }
    }

    // ── report_abort ──

    #[test]
    fn report_abort_shows_cancel_and_failure_but_keeps_shutdown_silent() {
        let sink = CapturingSink::new();

        assert_eq!(
            report_abort(&sink, TurnAbort::Cancelled, "label"),
            TurnEnd::Cancelled
        );
        assert_eq!(
            report_abort(&sink, TurnAbort::Shutdown, "label"),
            TurnEnd::Shutdown
        );
        assert_eq!(
            report_abort(&sink, TurnAbort::Failed(anyhow!("boom")), "label"),
            TurnEnd::Failed
        );

        let events = sink.events();
        assert!(
            matches!(
                events.as_slice(),
                [Event::Cancelled, Event::TurnFailed(msg)] if msg == "boom"
            ),
            "{events:?}",
        );
    }

    // ── format_drift_warning ──

    #[test]
    fn format_drift_warning_names_a_single_file() {
        let warning = format_drift_warning(&[PathBuf::from("a.rs")]);

        assert_eq!(
            warning,
            "1 tracked file changed on disk since the resumed session read it, \
             so Edit needs a fresh Read first: a.rs",
        );
    }

    #[test]
    fn format_drift_warning_previews_three_paths_and_counts_the_rest() {
        let drifted: Vec<PathBuf> = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let warning = format_drift_warning(&drifted);

        assert_eq!(
            warning,
            "5 tracked files changed on disk since the resumed session read them, \
             so Edit needs a fresh Read first: a.rs, b.rs, c.rs, and 2 more",
        );
    }
}
