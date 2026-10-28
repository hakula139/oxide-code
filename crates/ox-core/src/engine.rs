//! The engine: one conversation running as a task that every frontend drives through an
//! [`EngineHandle`], sending ops on a bounded channel and reading events from an unbounded one.

mod abort;
mod compaction;
mod dispatch;
#[cfg(test)]
mod local_testing;
mod login;
mod sessions;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod transcript;
mod turn;
mod usage;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};

use self::abort::{Inbox, TurnAbort};
use self::transcript::{LiveTranscript, TranscriptStart};
use self::turn::{TurnContext, TurnOutcome, TurnReport, TurnStop};
use self::usage::UsageLedger;
use crate::client::anthropic::Client;
use crate::config::Config;
use crate::config::compaction::CompactionConfig;
use crate::config::oauth::{self, ClaudeCodeKeychain};
use crate::message::{ContentBlock, Message, TextOrigin};
use crate::permission::{Mode, Policy, Workspace};
use crate::prompt::UserContext;
use crate::protocol::notice::{self, ConfigSettings};
use crate::protocol::{ConfigPick, Event, EventSink, Interaction, Op, TurnEnd};
use crate::session::handle::{Checkpoint, LoadedTranscript, SessionHandle};
use crate::session::store::SessionStore;
use crate::session::{history, title_generator};
use crate::tool::ToolRegistry;
use crate::tool::bash::background::BackgroundShells;
use crate::util::tls::Trust;

/// Ops a frontend can queue before the engine reads them. A turn keeps reading ops while it runs,
/// so this only absorbs a burst of queued prompts.
const OP_CHANNEL_CAP: usize = 32;

// ── Engine Handle ──

/// A running [`Engine`]. Closing `ops` asks it to quit.
pub struct EngineHandle {
    /// Frontend submissions, subject to [`Op`]'s idle-state requirements. Enqueueing an op does
    /// not acknowledge its execution.
    pub ops: mpsc::Sender<Op>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub task: EngineTask,
}

impl EngineHandle {
    /// Quits the engine and joins its task. Holds `events` open until then, so the engine's final
    /// emits don't fail and log as dropped.
    pub async fn shutdown(self) -> Result<Option<String>, EnginePanic> {
        let Self {
            ops,
            events: _events,
            task,
        } = self;
        drop(ops);
        task.join().await
    }
}

pub struct EngineTask(JoinHandle<Option<String>>);

impl EngineTask {
    /// Waits until the engine has finalized the live session, returning the write failure if any,
    /// or the panic of an engine that never finalized it.
    pub async fn join(self) -> Result<Option<String>, EnginePanic> {
        self.0.await.map_err(EnginePanic)
    }
}

/// The panic that ended the engine task. The frontend finishes its output first, then `main`
/// re-raises it with [`Self::resume`], so `ox` exits with the panic status.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct EnginePanic(JoinError);

impl EnginePanic {
    pub fn resume(self) -> ! {
        std::panic::resume_unwind(self.0.into_panic())
    }
}

// ── Engine ──

/// The session an [`Engine`] continues: the store it rolls new sessions into, and the live handle
/// and the transcript it loaded.
pub struct SessionParts {
    pub store: SessionStore,
    pub handle: SessionHandle,
    pub transcript: LoadedTranscript,
}

/// The live session with its transcript, prompt, and usage accounting.
pub struct Engine {
    client: Client,
    /// Where `/login` exchanges its authorization code, a mock server in tests.
    oauth_token_url: String,
    /// The Keychain `/logout` checks for the Claude Code login before its file, none in tests.
    claude_code_keychain: ClaudeCodeKeychain,
    /// Whether the next prompt or compaction loads the stored login again, since a transient
    /// failure signed the session out.
    retry_login: bool,
    tools: Arc<ToolRegistry>,
    store: SessionStore,
    /// The live session, its transcript, and what the model has seen of the prompt.
    transcript: LiveTranscript,
    /// The config's startup notices, which [`Self::spawn`] shows once.
    startup_notices: Vec<String>,
    /// Warnings about the instruction files the latest user context load could not read.
    instruction_warnings: Vec<String>,
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
    /// Whether fresh sessions get an AI title in the background, which only the TUI shows.
    generates_titles: bool,
    /// What the live session has billed and the context usage the status line shows.
    usage: UsageLedger,
}

impl Engine {
    /// Continues `session` with a client built from `config` and `trust`, and the built-in tools
    /// and agent settings from `config`. Fails when the client cannot be built.
    pub async fn new(
        config: &Config,
        trust: impl Trust + 'static,
        device_id: String,
        session: SessionParts,
        generates_titles: bool,
    ) -> Result<Self> {
        let SessionParts {
            store,
            handle,
            transcript,
        } = session;
        let client = Client::new(config, trust, device_id)?;
        let shells = Arc::new(BackgroundShells::default());
        let read_filter = config.permission.read_filter();
        let tools = Arc::new(ToolRegistry::built_in(&read_filter, &shells));
        let workspace = Arc::new(config.workspace.clone());
        let (transcript, usage, instruction_warnings) =
            LiveTranscript::load(handle, transcript, client.model(), workspace.cwd()).await;
        let context_estimate = transcript.request_estimate(&tools.definitions());
        Ok(Self {
            client,
            oauth_token_url: oauth::OAUTH_TOKEN_URL.to_owned(),
            claude_code_keychain: ClaudeCodeKeychain::SYSTEM,
            retry_login: config
                .signed_out
                .as_ref()
                .is_some_and(|signed_out| signed_out.retry),
            tools,
            store,
            transcript,
            startup_notices: config.notices(),
            instruction_warnings,
            shells,
            policy: config.permission.clone(),
            max_tool_rounds: config.max_tool_rounds,
            compaction: config.compaction,
            workspace,
            generates_titles,
            usage: UsageLedger::for_session(&usage, context_estimate),
        })
    }

    pub fn session_id(&self) -> &str {
        self.transcript.session().session_id()
    }

    pub fn cwd(&self) -> &Path {
        self.workspace.cwd()
    }

    /// The project the session store is scoped to, where `/init` writes instruction files.
    pub fn project_root(&self) -> &Path {
        self.store.project_root()
    }

    /// Whether a person can answer an approval request in this run, which
    /// [`Config::set_unattended`](crate::config::Config::set_unattended) rules out.
    pub const fn attended(&self) -> bool {
        self.policy.attended()
    }

    /// Projects the loaded transcript for a frontend to show. Takes the compaction boundary and
    /// tool metadata it loaded with, so a later call projects without them.
    pub fn take_history(&mut self) -> Vec<Interaction> {
        let compact = self.transcript.compact.take();
        let tool_metadata = std::mem::take(&mut self.transcript.tool_metadata);
        history::interactions(
            self.transcript.messages(),
            compact.as_ref(),
            &tool_metadata,
            &self.tools,
            self.workspace.cwd(),
        )
    }

    /// Starts the engine task, which first emits the config's startup notices, then what
    /// [`Self::emit_session_start`] shows.
    pub fn spawn(mut self) -> EngineHandle {
        let (ops, op_rx) = mpsc::channel(OP_CHANNEL_CAP);
        // Unbounded, so the engine never waits on or drops events for a frontend that falls behind.
        let (sink, events) = mpsc::unbounded_channel();
        self.shells.attach(Arc::new(sink.clone()));
        for notice in std::mem::take(&mut self.startup_notices) {
            sink.emit(Event::Notice(notice), "startup-notice");
        }
        self.emit_session_start(&sink);
        let task = EngineTask(tokio::spawn(self.run(sink, op_rx)));
        EngineHandle { ops, events, task }
    }

    /// Serves ops until `Shutdown` or the op channel closes, then finalizes the live session.
    async fn run(
        mut self,
        sink: mpsc::UnboundedSender<Event>,
        mut ops: mpsc::Receiver<Op>,
    ) -> Option<String> {
        loop {
            let op = tokio::select! {
                biased;
                usage = async { self.usage.title_usage.as_mut()?.await.ok() },
                    if self.usage.title_usage.is_some() =>
                {
                    if self.account_title_usage(usage) {
                        self.emit_usage_update(&sink);
                    }
                    continue;
                }
                op = ops.recv() => match op {
                    Some(op) => op,
                    None => break,
                },
            };
            let mut end = None;
            match op {
                Op::Prompt { text, title } => {
                    end = Some(self.submit_prompt(&text, title, &sink, &mut ops).await);
                }
                Op::Compact { instructions } => {
                    end = Some(self.compact(&sink, &mut ops, instructions).await);
                }
                Op::Clear => self.clear(&sink).await,
                Op::Archive => self.archive(&sink).await,
                Op::SetArchive { target, state } => self.set_archive(&sink, target, state).await,
                Op::FindSaved { prefix } => self.find_saved(&sink, prefix).await,
                Op::DeleteSaved { session_id } => self.delete_saved(&sink, session_id).await,
                Op::Resume { target } => self.resume(&sink, target).await,
                Op::Rename { title } => self.rename(&sink, title).await,
                Op::SetModel(pick) => self.set_model(&sink, pick),
                Op::SetMode(mode) => self.set_mode(&sink, mode),
                Op::Login => end = Some(self.login(&sink, &mut ops).await),
                Op::Logout => self.logout(&sink).await,
                // A `Queue` here arrived after its turn ended, and the frontend still submits it.
                // An interrupt or approval only matters mid-turn, where `agent_turn` consumes it,
                // and a sign-in code only while `login` waits for one.
                Op::Queue(_) | Op::Interrupt | Op::Resolve { .. } | Op::LoginCode(_) => {}
                Op::Shutdown => break,
            }
            if end == Some(TurnEnd::Shutdown) {
                break;
            }
        }

        self.finalize().await
    }

    /// Runs `text` through auto-compaction and one agent turn, rolling back a refused turn, and
    /// reports how it ended on `sink`. A fresh session takes `title` as its first-prompt title. The
    /// turn compacts at each round boundary that leaves the context at the threshold, then goes on
    /// from the summary.
    async fn submit_prompt<S>(
        &mut self,
        text: &str,
        title: Option<String>,
        sink: &S,
        ops: &mut mpsc::Receiver<Op>,
    ) -> TurnEnd
    where
        S: EventSink + Clone + 'static,
    {
        let mut inbox = Inbox::default();
        if let Err(abort) = self.ensure_signed_in(sink, ops, &mut inbox).await {
            self.adopt_mode(&mut inbox);
            return report_unsent(sink, abort, "turn-failed");
        }
        let compacted = self.auto_compact(sink, ops, &mut inbox).await;
        self.adopt_mode(&mut inbox);
        if let Err(abort) = compacted {
            return report_unsent(sink, abort, "auto-compact-failed");
        }

        let mut start = self.transcript.start_of_turn();
        let mut user_msg = Message::user(text);
        let fresh = self.load_user_context(sink).await;
        if let Some(reminder) = self.transcript.catch_up(fresh) {
            user_msg.content.push(ContentBlock::generated_text(
                reminder,
                TextOrigin::SystemReminder,
            ));
        }
        let outcome = self.transcript.record_prompt(user_msg, title).await;
        sink.session_write_error(outcome.failure.as_deref());
        turn::record_drained_prompts(inbox.prompts.drain(..), &mut self.transcript, sink).await;

        if self.generates_titles
            && let Some(seed) = outcome.ai_title_seed
        {
            self.usage.title_usage = Some(title_generator::spawn(
                self.client.clone(),
                self.transcript.session().clone(),
                sink.clone(),
                seed,
            ));
        }

        let mut checkpoint = outcome.checkpoint;
        let mut rounds_run = 0;
        let result = loop {
            let TurnOutcome { mut report, result } =
                self.run_turn(sink, ops, &mut inbox, rounds_run).await;
            if let Err(TurnAbort::Refused(notice)) = result {
                self.roll_back(sink, start, checkpoint, &report).await;
                break Err(TurnAbort::Refused(notice));
            }
            self.account_usage(sink, &report).await;
            if let Some(tasks) = report.tasks.take() {
                self.transcript.tasks = tasks;
            }
            let Ok(TurnStop::Compact { rounds_run: run }) = result else {
                break result;
            };

            let compacted = self.compact_mid_turn(sink, ops, &mut inbox).await;
            self.adopt_mode(&mut inbox);
            match compacted {
                Ok(summary) => checkpoint = Some(summary),
                Err(abort) => break Err(abort),
            }
            start = self.transcript.start_of_turn();
            rounds_run = run;
            turn::record_drained_prompts(inbox.prompts.drain(..), &mut self.transcript, sink).await;
        };
        match result {
            Ok(_) => {
                sink.emit(Event::TurnComplete, "turn-complete");
                TurnEnd::Completed
            }
            Err(abort) => report_abort(sink, abort, "turn-failed"),
        }
    }

    /// Runs [`turn::agent_turn`] on the live transcript from round `rounds_run`, then applies any
    /// mode switch the turn received.
    async fn run_turn(
        &mut self,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
        rounds_run: u32,
    ) -> TurnOutcome {
        let auto_compaction = self.auto_compaction();
        let mut dispatch_ctx = dispatch::DispatchContext {
            policy: &mut self.policy,
            workspace: Arc::clone(&self.workspace),
            session: self.transcript.session().clone(),
            tracker: Arc::clone(&self.transcript.file_tracker),
        };
        let outcome = turn::agent_turn(
            TurnContext {
                client: &self.client,
                tools: &self.tools,
                sink,
                max_tool_rounds: self.max_tool_rounds,
                auto_compaction,
                rounds_run,
            },
            &mut self.transcript,
            ops,
            inbox,
            &mut dispatch_ctx,
        )
        .await;
        self.adopt_mode(inbox);
        outcome
    }

    /// Returns the transcript and the session to `start` and `checkpoint` after the model refused
    /// the turn `report` describes. Keeps context usage from before the turn, as the transcript its
    /// requests measured is gone.
    async fn roll_back(
        &mut self,
        sink: &dyn EventSink,
        start: TranscriptStart,
        checkpoint: Option<Checkpoint>,
        report: &TurnReport,
    ) {
        self.transcript.file_tracker.drain_writes().await;
        self.bill(sink, &report.billable_rounds).await;
        let failure = self.transcript.roll_back(start, checkpoint).await;
        sink.session_write_error(failure.as_deref());
        if report.tasks.is_some() {
            let tasks = self.transcript.tasks.clone();
            sink.emit(Event::TasksUpdated(tasks), "tasks-rolled-back");
        }
        self.emit_usage_update(sink);
        sink.emit(Event::TurnRolledBack, "turn-rolled-back");
    }

    /// Reads the user context afresh with [`Self::show_instruction_warnings`].
    async fn load_user_context(&mut self, sink: &dyn EventSink) -> UserContext {
        let (context, warnings) = UserContext::load(self.cwd(), self.client.model()).await;
        self.show_instruction_warnings(sink, warnings);
        context
    }

    /// Keeps the instruction file `warnings` of the latest load, showing each one the previous load
    /// did not raise.
    fn show_instruction_warnings(&mut self, sink: &dyn EventSink, warnings: Vec<String>) {
        let previous = std::mem::replace(&mut self.instruction_warnings, warnings);
        for warning in &self.instruction_warnings {
            if !previous.contains(warning) {
                sink.emit(Event::Notice(warning.clone()), "instruction-warning");
            }
        }
    }

    /// Applies a model and / or effort pick. The model goes first because it re-resolves effort.
    fn set_model(&mut self, sink: &dyn EventSink, pick: ConfigPick) {
        let before = ConfigSettings {
            model: self.client.model().clone(),
            effort: self.client.effort(),
            auto_compaction: self.compaction.auto,
        };
        if let ConfigPick::Model { id, .. } = &pick {
            self.client.set_model(id.clone().into_inner());
            self.compaction = self
                .compaction
                .for_model(self.client.model(), self.client.max_tokens());
        }
        let resolved = match pick.effort() {
            Some(effort) => self.client.set_effort(effort),
            None => self.client.effort(),
        };
        self.usage.auto_compaction_failures.reset(sink);
        self.emit_usage_update(sink);
        let after = ConfigSettings {
            model: self.client.model().clone(),
            effort: resolved,
            auto_compaction: self.compaction.auto,
        };
        let notice = notice::config_changed(&before, &after, &pick);
        let ConfigSettings {
            model,
            effort,
            auto_compaction,
        } = after;
        sink.emit(
            Event::ConfigChanged {
                model,
                effort,
                requested: pick,
                auto_compaction,
                notice,
            },
            "config-changed",
        );
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

    /// Settles workspace writes, ends background shells, and closes the live session, then waits
    /// for stored-login reloads still in flight so exiting cannot cancel their write-back.
    /// Returns the session's write failure, if any.
    async fn finalize(self) -> Option<String> {
        self.transcript.file_tracker.drain_writes().await;
        self.shells.stop_all().await;
        let failure = self.transcript.finalize().await;
        self.client.finish_reloads().await;
        failure
    }
}

// ── Reporting ──

/// Shows `abort` on `sink` as the operation's one terminal event, logging `label` if a failure
/// event is dropped. `Shutdown` stays silent. [`Event`] lists each op's terminal events.
fn report_abort(sink: &dyn EventSink, abort: TurnAbort, label: &'static str) -> TurnEnd {
    match abort {
        TurnAbort::Cancelled => {
            sink.emit(Event::Cancelled, "cancelled");
            TurnEnd::Cancelled
        }
        TurnAbort::Shutdown => TurnEnd::Shutdown,
        TurnAbort::Refused(notice) => {
            sink.emit(Event::TurnFailed(notice), label);
            TurnEnd::Failed
        }
        TurnAbort::Failed(e) => {
            sink.emit(Event::TurnFailed(format!("{e:#}")), label);
            TurnEnd::Failed
        }
    }
}

/// Reports an abort that stopped the operation before it recorded its prompt, handing the prompt
/// back for any outcome but shutdown. The frontend returns it to the user, and the abort's terminal
/// event follows.
fn report_unsent(sink: &dyn EventSink, abort: TurnAbort, label: &'static str) -> TurnEnd {
    if !matches!(abort, TurnAbort::Shutdown) {
        sink.emit(Event::PromptUnsent, "prompt-unsent");
    }
    report_abort(sink, abort, label)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::Duration;

    use anyhow::anyhow;
    use indoc::indoc;
    use tokio::sync::Notify;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::local_testing::{
        TITLE_MODEL, dead_test_session, finished_session, greeting_title, inert_ops, login_urls,
        mount_title, next_matching, service_log, session_ids, submit, system_prompt, texts,
        tool_heavy_transcript, tracked_paths, turn_events, write_unreadable_instructions,
    };
    use super::sessions::format_drift_warning;
    use super::testing::{
        MODEL, SESSION_CONTEXT, engine, mount_reply, mount_stalled_reply_once,
        mount_tool_call_once, panic_message, request_bodies, until_requested, with_pinned_home,
    };
    use super::*;
    use crate::client::anthropic::local_testing::{COMPLETION_USAGE, client_for};
    use crate::client::anthropic::testing::{
        DEVICE_ID, api_key, sse_body, sse_response, text_stream_body,
    };
    use crate::config::auth::{AuthSource, LoginUnavailable};
    use crate::config::compaction::CompactionConfig;
    use crate::config::compaction::testing::WINDOW_1M_THRESHOLD;
    use crate::config::testing::test_config;
    use crate::engine::compaction::AutoCompactFailures;
    use crate::file_tracker::testing::{
        assert_writes_draining, blocked_write, record_tracked_file, seed_full_read,
    };
    use crate::file_tracker::{FileSnapshot, FileTracker, GateError, GatePurpose};
    use crate::message::{Replay, Role};
    use crate::model;
    use crate::model::{Effort, ModelSpec, PromptCacheTtl, ResolvedModelId, TokenUsage};
    use crate::permission::PermissionClass;
    use crate::permission::rule::Rule;
    use crate::protocol::testing::CapturingSink;
    use crate::protocol::{CompactTrigger, ResumeTarget, SavedTarget, UsageSnapshot};
    use crate::session::entry::{ArchiveState, UsageRecord};
    use crate::session::handle;
    use crate::session::handle::testing::record_write_call;
    use crate::session::post_compact::synthesize_post_compact_message;
    use crate::session::saved::ArchiveUpdate;
    use crate::session::store::testing::{WriterBarrier, seed_test_session, test_store};
    use crate::tool::bash::background::ShellState;
    use crate::tool::bash::background::local_testing::start_shell;
    use crate::tool::bash::background::testing::shell;
    use crate::tool::local_testing::ctx;
    use crate::tool::read::ReadTool;
    use crate::tool::tasks::TasksTool;
    use crate::tool::tasks::testing::plan;
    use crate::tool::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec};
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::tls::testing::NoRoots;

    /// What each reply from [`mount_reply`] bills, as [`text_stream_body`] reports it.
    const REPLY_USAGE: TokenUsage = TokenUsage::new(1_000, 100);

    /// Estimated cost of one request on `model` that reported `tokens` and wrote no cache.
    fn request_cost(model: &str, tokens: TokenUsage) -> f64 {
        model::cost_rates(model)
            .unwrap()
            .estimate_usd(tokens, PromptCacheTtl::OneHour)
    }

    mod engine_handle {
        use super::*;

        mod shutdown {
            use super::*;

            #[tokio::test]
            async fn reports_a_session_that_cannot_finish() {
                with_pinned_home(async {
                    let server = MockServer::start().await;
                    let dir = tempfile::tempdir().unwrap();
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    engine.transcript.set_session(dead_test_session());

                    let failure = engine.spawn().shutdown().await.unwrap();

                    assert_eq!(
                        failure.as_deref(),
                        Some(
                            "session writer task has stopped. \
                             Conversation history may be incomplete."
                        ),
                    );
                })
                .await;
            }
        }
    }

    mod engine_task {
        use super::*;

        mod join {
            use super::*;

            #[tokio::test]
            async fn hands_back_the_panic_of_a_panicked_engine() {
                let task = EngineTask(tokio::spawn(async { panic!("engine bug") }));

                let panic = task.join().await.unwrap_err();

                assert_eq!(panic_message(panic), "engine bug");
            }
        }
    }

    mod new {
        use super::*;

        /// Tracker snapshots of `kept.txt` and `drifted.txt` in `dir`, after which `drifted.txt`
        /// changes on disk. Returns the snapshots and both paths.
        fn tracked_files(dir: &Path) -> (Vec<FileSnapshot>, PathBuf, PathBuf) {
            let tracker = FileTracker::default();
            let kept = dir.join("kept.txt");
            let drifted = dir.join("drifted.txt");
            record_tracked_file(&tracker, &kept, b"kept", "toolu_kept");
            record_tracked_file(&tracker, &drifted, b"before", "toolu_drifted");
            std::fs::write(&drifted, b"after").unwrap();
            (tracker.snapshot_all(), kept, drifted)
        }

        #[tokio::test]
        async fn prices_recorded_usage_and_replays_a_recorded_context_or_builds_one() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let recorded = engine(server.uri(), &store, dir.path())
                    .await
                    .transcript
                    .prompt()
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
                        NoRoots,
                        DEVICE_ID.to_owned(),
                        SessionParts {
                            store: store.clone(),
                            handle: handle::start(&store, "claude-sonnet-5-5", None),
                            transcript: LoadedTranscript {
                                context,
                                usage: usage.to_vec(),
                                ..LoadedTranscript::default()
                            },
                        },
                        false,
                    )
                    .await
                    .unwrap();

                    assert!(
                        (engine.usage.total_estimated_cost_usd - 0.006).abs() < 1e-12,
                        "priced at the recorded model's rates: {}",
                        engine.usage.total_estimated_cost_usd,
                    );
                    assert_eq!(
                        engine.transcript.prompt().user_context == SESSION_CONTEXT,
                        replayed
                    );
                    assert_eq!(engine.transcript.prompt_unrecorded(), !replayed);
                    let built_warnings = if replayed {
                        Vec::new()
                    } else {
                        vec![warning.clone()]
                    };
                    assert_eq!(engine.instruction_warnings, built_warnings);
                    let model = if replayed { MODEL } else { "claude-sonnet-5-5" };
                    assert!(
                        engine
                            .transcript
                            .prompt()
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
        async fn takes_the_agent_settings_from_config() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut config = test_config("http://127.0.0.1:9", api_key(), MODEL);
                config.max_tool_rounds = Some(3);
                config.compaction = CompactionConfig::with_threshold(50_000);
                config.permission.set_mode(Mode::Plan);
                config.workspace = Workspace::new(dir.path());
                config.signed_out = Some(LoginUnavailable {
                    notice: "Not signed in. Run `/login` to sign in.".to_owned(),
                    retry: true,
                });

                let engine = Engine::new(
                    &config,
                    NoRoots,
                    DEVICE_ID.to_owned(),
                    SessionParts {
                        store: store.clone(),
                        handle: handle::start(&store, MODEL, None),
                        transcript: LoadedTranscript::default(),
                    },
                    false,
                )
                .await
                .unwrap();

                assert_eq!(*engine.workspace, config.workspace);
                assert_eq!(engine.max_tool_rounds, Some(3));
                assert_eq!(engine.compaction, config.compaction);
                assert_eq!(engine.policy.mode(), Mode::Plan);
                assert_eq!(
                    engine.startup_notices,
                    ["Not signed in. Run `/login` to sign in."],
                );
                assert!(engine.retry_login);
            })
            .await;
        }

        #[tokio::test]
        async fn hands_its_background_shells_and_the_read_rules_to_the_built_in_tools() {
            with_pinned_home(async {
                let dir = canonical_tempdir();
                std::fs::write(dir.path().join("notes.txt"), "").unwrap();
                std::fs::write(dir.path().join("secret.txt"), "").unwrap();
                let store = test_store(dir.path());
                let mut config = test_config("http://127.0.0.1:9", api_key(), MODEL);
                let deny = vec![Rule::parse(PermissionClass::Read, "secret.txt").unwrap()];
                config.permission = Policy::new(Mode::Manual, deny, Vec::new(), Vec::new());
                config.workspace = Workspace::new(dir.path());
                let ctx = ctx(dir.path());

                let engine = Engine::new(
                    &config,
                    NoRoots,
                    DEVICE_ID.to_owned(),
                    SessionParts {
                        store: store.clone(),
                        handle: handle::start(&store, MODEL, None),
                        transcript: LoadedTranscript::default(),
                    },
                    false,
                )
                .await
                .unwrap();

                let globbed = engine
                    .tools
                    .run_input("glob", serde_json::json!({"pattern": "*.txt"}), &ctx)
                    .await;
                let started = engine
                    .tools
                    .run_input(
                        "bash",
                        serde_json::json!({"command": "sleep 30", "run_in_background": true}),
                        &ctx,
                    )
                    .await;
                assert_eq!(
                    globbed.content,
                    "notes.txt\n\n(1 file withheld by read deny or ask rules)"
                );
                assert!(!started.is_error, "{}", started.content);
                assert_eq!(
                    engine.shells.list(),
                    [shell("shell-1", "sleep 30", ShellState::Running)]
                );
                engine.shells.stop_all().await;
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_the_tracked_files_that_match_disk_and_holds_back_the_drift() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let (snapshots, kept, drifted) = tracked_files(dir.path());
                let mut config = test_config("http://127.0.0.1:9", api_key(), MODEL);
                config.workspace = Workspace::new(dir.path());

                let engine = Engine::new(
                    &config,
                    NoRoots,
                    DEVICE_ID.to_owned(),
                    SessionParts {
                        store: store.clone(),
                        handle: handle::start(&store, MODEL, None),
                        transcript: LoadedTranscript {
                            file_snapshots: snapshots,
                            ..LoadedTranscript::default()
                        },
                    },
                    false,
                )
                .await
                .unwrap();

                assert_eq!(tracked_paths(&engine), [kept]);
                assert_eq!(engine.transcript.drifted, [drifted]);
            })
            .await;
        }
    }

    mod spawn {
        use super::*;

        #[tokio::test]
        async fn reports_the_startup_notices_resumed_cost_tasks_warnings_drift_and_interruption() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let warning = write_unreadable_instructions(dir.path());
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.startup_notices = vec!["Not signed in. Run `/login` to sign in.".to_owned()];
                engine.transcript.set_messages(vec![
                    Message::user("hi"),
                    Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Thinking {
                            thinking: turn::INTERRUPTED_WORK_SENTINEL.to_owned(),
                            replay: Replay::with_model(MODEL, ""),
                        }],
                    },
                ]);
                engine.usage.total_estimated_cost_usd = 1.23;
                engine.transcript.tasks = plan();
                engine.transcript.drifted = vec![PathBuf::from("a.rs")];

                let mut handle = engine.spawn();
                let notice = handle.events.recv().await.unwrap();
                let cost = handle.events.recv().await.unwrap();
                let tasks = handle.events.recv().await.unwrap();
                let instructions = handle.events.recv().await.unwrap();
                let drift = handle.events.recv().await.unwrap();
                let interrupted = handle.events.recv().await.unwrap();
                let failure = handle.shutdown().await.unwrap();

                assert_eq!(
                    notice,
                    Event::Notice("Not signed in. Run `/login` to sign in.".to_owned()),
                );
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
    }

    mod run {
        use super::*;

        /// Events until the turn ends and its title and combined usage arrive, in any order.
        async fn titled_turn_events(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
            let prompt = Op::prompt("");
            let expected =
                request_cost(MODEL, REPLY_USAGE) + request_cost(TITLE_MODEL, COMPLETION_USAGE);
            let (mut ended, mut titled, mut billed) = (false, false, false);
            let mut seen = Vec::new();
            while !(ended && titled && billed) {
                let event = events.recv().await.expect("the engine stopped early");
                ended |= TurnEnd::of(&prompt, &event).is_some();
                titled |= matches!(event, Event::SessionTitleUpdated { .. });
                billed |= matches!(
                    &event,
                    Event::UsageUpdated(UsageSnapshot {
                        estimated_cost_usd: Some(cost),
                        ..
                    }) if (cost - expected).abs() < 1e-12
                );
                seen.push(event);
            }
            seen
        }

        /// A prompt that pastes two service logs, long enough that its exchange can compact.
        fn pasted_logs() -> String {
            format!(
                "Why do these logs stop?\n{}{}",
                service_log("alpha"),
                service_log("beta")
            )
        }

        #[tokio::test]
        async fn serves_a_turn_and_saves_it_on_shutdown() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id().to_owned();
                let mut handle = engine.spawn();

                let events = submit(&mut handle, "hello").await;
                let failure = handle.shutdown().await.unwrap();

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
        async fn titles_a_fresh_session_and_counts_the_title_cost() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_title(&server, greeting_title()).await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.generates_titles = true;
                let live_id = engine.session_id().to_owned();
                let mut handle = engine.spawn();

                handle.ops.send(Op::prompt("hello")).await.unwrap();
                let first = titled_turn_events(&mut handle.events).await;
                let second = submit(&mut handle, "again").await;
                handle.shutdown().await.unwrap();

                let titled = first.iter().find_map(|event| match event {
                    Event::SessionTitleUpdated { id, title } => Some((id.as_str(), title.as_str())),
                    _ => None,
                });
                assert_eq!(titled, Some((live_id.as_str(), "Greeting")));
                let expected = 2.0 * request_cost(MODEL, REPLY_USAGE)
                    + request_cost(TITLE_MODEL, COMPLETION_USAGE);
                assert!(
                    matches!(
                        second.as_slice(),
                        [
                            Event::StreamToken(_),
                            Event::UsageUpdated(UsageSnapshot {
                                estimated_cost_usd: Some(cost),
                                ..
                            }),
                            Event::TurnComplete,
                        ] if (cost - expected).abs() < 1e-12
                    ),
                    "two turns and the title: {second:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn counts_a_late_title_request_while_idle() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_title(&server, greeting_title()).await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let client = engine.client.clone();
                let session = engine.transcript.session().clone();
                let session_id = engine.session_id().to_owned();
                let (billed, usage) = oneshot::channel();
                engine.usage.title_usage = Some(usage);
                let (ops, op_rx) = mpsc::channel(OP_CHANNEL_CAP);
                let (sink, events) = mpsc::unbounded_channel();
                let task = EngineTask(tokio::spawn(engine.run(sink.clone(), op_rx)));
                let mut handle = EngineHandle { ops, events, task };

                let first = submit(&mut handle, "hello").await;
                assert!(matches!(first.last(), Some(Event::TurnComplete)));
                let title = title_generator::spawn(client, session, sink, "hello".to_owned());
                billed.send(title.await.unwrap()).unwrap();
                let late = tokio::time::timeout(Duration::from_secs(5), async {
                    let (mut title, mut usage) = (None, None);
                    while title.is_none() || usage.is_none() {
                        match handle.events.recv().await.unwrap() {
                            Event::SessionTitleUpdated { id, title: value } => {
                                title = Some((id, value));
                            }
                            Event::UsageUpdated(value) => usage = Some(value),
                            event => panic!("unexpected idle event: {event:?}"),
                        }
                    }
                    (title.unwrap(), usage.unwrap())
                })
                .await;
                let persisted = store.load_session_data(&session_id).unwrap();
                let second = submit(&mut handle, "again").await;
                handle.shutdown().await.unwrap();

                let (title, usage) = late.expect("idle title usage must arrive without another op");
                assert_eq!(title, (session_id, "Greeting".to_owned()));
                assert_eq!(usage.context_tokens, Some(REPLY_USAGE.context_tokens()));
                assert_eq!(usage.context_window, Some(1_000_000));
                let expected =
                    request_cost(MODEL, REPLY_USAGE) + request_cost(TITLE_MODEL, COMPLETION_USAGE);
                assert!((usage.estimated_cost_usd.unwrap() - expected).abs() < 1e-12);
                assert_eq!(persisted.title.unwrap().title, "Greeting");
                assert!((usage::estimate_cost_usd(&persisted.usage) - expected).abs() < 1e-12);
                let costs: Vec<_> = second
                    .iter()
                    .filter_map(|event| match event {
                        Event::UsageUpdated(usage) => usage.estimated_cost_usd,
                        _ => None,
                    })
                    .collect();
                assert_eq!(costs.len(), 1, "{second:?}");
                assert!((costs[0] - expected - request_cost(MODEL, REPLY_USAGE)).abs() < 1e-12);
            })
            .await;
        }

        #[tokio::test]
        async fn serves_prompts_after_a_title_request_ends_without_usage() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let (billed, usage) = oneshot::channel();
                engine.usage.title_usage = Some(usage);
                drop(billed);
                let mut handle = engine.spawn();

                let events = submit(&mut handle, "hello").await;
                handle.shutdown().await.unwrap();

                assert_eq!(
                    events,
                    [
                        Event::StreamToken("reply".to_owned()),
                        Event::UsageUpdated(UsageSnapshot {
                            context_tokens: Some(REPLY_USAGE.context_tokens()),
                            context_window: Some(1_000_000),
                            estimated_cost_usd: Some(request_cost(MODEL, REPLY_USAGE)),
                        }),
                        Event::TurnComplete,
                    ]
                );
            })
            .await;
        }

        #[tokio::test]
        async fn saves_the_title_usage_so_a_resume_restores_its_cost() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_title(&server, greeting_title()).await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.generates_titles = true;
                let session_id = engine.session_id().to_owned();
                let cache_ttl = engine.client.prompt_cache_ttl();
                let mut handle = engine.spawn();

                handle.ops.send(Op::prompt("hello")).await.unwrap();
                titled_turn_events(&mut handle.events).await;
                handle.ops.send(Op::Clear).await.unwrap();
                next_matching(&mut handle.events, |event| {
                    matches!(event, Event::SessionReady).then_some(())
                })
                .await;
                let target = ResumeTarget::Path(store.find_session_path(&session_id).unwrap());
                handle.ops.send(Op::Resume { target }).await.unwrap();
                next_matching(&mut handle.events, |event| {
                    matches!(event, Event::SessionResumed { .. }).then_some(())
                })
                .await;
                let restored = handle.events.recv().await.unwrap();
                handle.shutdown().await.unwrap();

                let expected =
                    request_cost(MODEL, REPLY_USAGE) + request_cost(TITLE_MODEL, COMPLETION_USAGE);
                assert!(
                    matches!(
                        &restored,
                        Event::UsageUpdated(UsageSnapshot {
                            context_tokens: None,
                            estimated_cost_usd: Some(cost),
                            ..
                        }) if (cost - expected).abs() < 1e-12
                    ),
                    "the turn and the title: {restored:?}"
                );
                let usage = store.load_session_data(&session_id).unwrap().usage;
                let title_usage: Vec<_> = usage
                    .iter()
                    .filter(|record| record.model == TITLE_MODEL)
                    .collect();
                assert_eq!(
                    title_usage,
                    [&UsageRecord {
                        model: TITLE_MODEL.to_owned(),
                        cache_ttl,
                        tokens: COMPLETION_USAGE,
                    }]
                );
            })
            .await;
        }

        #[tokio::test]
        async fn splices_a_queued_prompt_in_at_the_next_round_boundary() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_tool_call_once(&server, "call-1", "missing", "{}").await;
                mount_reply(&server, "done").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let mut handle = engine.spawn();

                for op in [
                    Op::Queue("early".to_owned()),
                    Op::prompt("first"),
                    Op::Queue("queued".to_owned()),
                ] {
                    handle.ops.send(op).await.unwrap();
                }
                let events = turn_events(&mut handle.events).await;
                handle.shutdown().await.unwrap();

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
        async fn compact_replaces_the_transcript_with_the_summary() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id().to_owned();
                let mut handle = engine.spawn();

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
                        trigger,
                        ..
                    } => Some((pre_count, instructions, trigger)),
                    _ => None,
                })
                .await;
                submit(&mut handle, "after").await;
                handle.shutdown().await.unwrap();

                assert_eq!(
                    compacted,
                    (4, Some("focus".to_owned()), CompactTrigger::Manual)
                );
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
        async fn resume_continues_the_session_an_id_prefix_names() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let target_id = finished_session(&store).await;
                let engine = engine(server.uri(), &store, dir.path()).await;
                let mut handle = engine.spawn();

                let target = ResumeTarget::Prefix(target_id[..8].to_owned());
                handle.ops.send(Op::Resume { target }).await.unwrap();
                let resumed = next_matching(&mut handle.events, |event| match event {
                    Event::SessionResumed { id, history, .. } => Some((id, history)),
                    _ => None,
                })
                .await;
                submit(&mut handle, "again").await;
                handle.shutdown().await.unwrap();

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
        async fn resume_by_a_prefix_only_the_live_session_matches_fails_and_stays_live() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let live_id = engine.session_id().to_owned();
                let mut handle = engine.spawn();

                submit(&mut handle, "first").await;
                let target = ResumeTarget::Prefix(live_id[..8].to_owned());
                handle.ops.send(Op::Resume { target }).await.unwrap();
                let failure = next_matching(&mut handle.events, |event| match event {
                    Event::TurnFailed(message) => Some(message),
                    _ => None,
                })
                .await;
                submit(&mut handle, "second").await;
                handle.shutdown().await.unwrap();

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
        async fn records_every_request_so_a_resume_restores_the_session_cost() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id().to_owned();
                let mut handle = engine.spawn();

                submit(&mut handle, &pasted_logs()).await;
                submit(&mut handle, "two").await;
                handle
                    .ops
                    .send(Op::Compact { instructions: None })
                    .await
                    .unwrap();
                next_matching(&mut handle.events, |event| {
                    matches!(event, Event::SessionReady).then_some(())
                })
                .await;
                submit(&mut handle, "after").await;
                handle.ops.send(Op::Clear).await.unwrap();
                next_matching(&mut handle.events, |event| {
                    matches!(event, Event::SessionReady).then_some(())
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
                handle.shutdown().await.unwrap();

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
        async fn archive_rolls_away_and_a_resume_unarchives_the_session() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let archived_id = engine.transcript.session().session_id().to_owned();
                let mut handle = engine.spawn();

                submit(&mut handle, "first").await;
                handle.ops.send(Op::Archive).await.unwrap();
                let (fresh_id, old_session) =
                    next_matching(&mut handle.events, |event| match event {
                        Event::SessionRolled { id, old_session } => Some((id, old_session)),
                        _ => None,
                    })
                    .await;
                assert_ne!(fresh_id, archived_id);
                assert_eq!(old_session, ArchiveState::Archived);
                assert_eq!(
                    store.load_session_data(&archived_id).unwrap().archive_state,
                    ArchiveState::Archived,
                );

                let target = ResumeTarget::Path(store.find_session_path(&archived_id).unwrap());
                handle.ops.send(Op::Resume { target }).await.unwrap();
                let resumed = next_matching(&mut handle.events, |event| match event {
                    Event::SessionResumed { id, prior, .. } => Some((id, prior)),
                    _ => None,
                })
                .await;
                handle.shutdown().await.unwrap();

                assert_eq!(resumed, (archived_id.clone(), ArchiveState::Archived));
                assert_eq!(
                    store.load_session_data(&archived_id).unwrap().archive_state,
                    ArchiveState::Active,
                );
            })
            .await;
        }

        #[tokio::test]
        async fn finishes_a_saved_archive_before_the_queued_resume() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                seed_test_session(
                    &store,
                    "saved",
                    Some("Saved"),
                    Some(1),
                    time::macros::datetime!(2026-04-18 09:00:00 UTC),
                );
                let path = store.find_session_path("saved").unwrap();
                let engine = engine(server.uri(), &store, dir.path()).await;
                let barrier = WriterBarrier::hold(&path);
                let mut handle = engine.spawn();

                handle
                    .ops
                    .send(Op::SetArchive {
                        target: SavedTarget::Id("saved".to_owned()),
                        state: ArchiveState::Archived,
                    })
                    .await
                    .unwrap();
                handle
                    .ops
                    .send(Op::Resume {
                        target: ResumeTarget::Id("saved".to_owned()),
                    })
                    .await
                    .unwrap();
                let independent = tokio::spawn(async { "runtime event" });

                assert_eq!(independent.await.unwrap(), "runtime event");
                assert_eq!(
                    store.load_session_data("saved").unwrap().archive_state,
                    ArchiveState::Active,
                );
                barrier.release();
                let archived = next_matching(&mut handle.events, |event| {
                    matches!(
                        event,
                        Event::ArchiveUpdated { .. } | Event::SessionResumed { .. }
                    )
                    .then_some(event)
                })
                .await;
                assert_eq!(
                    archived,
                    Event::ArchiveUpdated {
                        target: SavedTarget::Id("saved".to_owned()),
                        state: ArchiveState::Archived,
                        result: Ok(ArchiveUpdate {
                            id: "saved".to_owned(),
                            changed: true,
                        }),
                    }
                );
                let resumed = next_matching(&mut handle.events, |event| match event {
                    Event::SessionResumed { id, prior, .. } => Some((id, prior)),
                    _ => None,
                })
                .await;
                handle.shutdown().await.unwrap();

                assert_eq!(resumed, ("saved".to_owned(), ArchiveState::Archived));
                assert_eq!(
                    store.load_session_data("saved").unwrap().archive_state,
                    ArchiveState::Active,
                );
            })
            .await;
        }

        #[tokio::test]
        async fn finds_a_saved_session_by_prefix_and_deletes_it_only_on_the_delete_op() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                seed_test_session(
                    &store,
                    "saved-session",
                    Some("Saved"),
                    Some(1),
                    time::macros::datetime!(2026-04-18 09:00:00 UTC),
                );
                let path = store.find_session_path("saved-session").unwrap();
                let engine = engine(server.uri(), &store, dir.path()).await;
                let mut handle = engine.spawn();

                handle
                    .ops
                    .send(Op::FindSaved {
                        prefix: "saved".to_owned(),
                    })
                    .await
                    .unwrap();
                let found = next_matching(&mut handle.events, |event| {
                    matches!(event, Event::SavedFound(_) | Event::SavedDeleted { .. })
                        .then_some(event)
                })
                .await;

                assert!(
                    matches!(&found, Event::SavedFound(Ok(info)) if info.session_id == "saved-session"),
                    "{found:?}"
                );
                assert!(path.exists(), "a lookup leaves the session in place");

                handle
                    .ops
                    .send(Op::DeleteSaved {
                        session_id: "saved-session".to_owned(),
                    })
                    .await
                    .unwrap();
                let deleted = next_matching(&mut handle.events, |event| {
                    matches!(event, Event::SavedFound(_) | Event::SavedDeleted { .. })
                        .then_some(event)
                })
                .await;
                handle.shutdown().await.unwrap();

                assert_eq!(
                    deleted,
                    Event::SavedDeleted {
                        id: "saved-session".to_owned(),
                        result: Ok(()),
                    }
                );
                assert!(!path.exists());
            })
            .await;
        }

        #[tokio::test]
        async fn resumes_a_prefix_in_the_current_project_over_another_projects_match() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let other =
                    SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
                for (store, id, at) in [
                    (
                        &store,
                        "shared-local",
                        time::macros::datetime!(2026-04-18 09:00:00 UTC),
                    ),
                    (
                        &other,
                        "shared-other",
                        time::macros::datetime!(2026-04-18 10:00:00 UTC),
                    ),
                ] {
                    seed_test_session(store, id, Some(id), Some(1), at);
                }
                let engine = engine(server.uri(), &store, dir.path()).await;
                let mut handle = engine.spawn();

                handle
                    .ops
                    .send(Op::Resume {
                        target: ResumeTarget::Prefix("shared".to_owned()),
                    })
                    .await
                    .unwrap();
                let resumed = next_matching(&mut handle.events, |event| match event {
                    Event::SessionResumed { id, .. } => Some(Ok(id)),
                    Event::TurnFailed(msg) => Some(Err(msg)),
                    _ => None,
                })
                .await;
                handle.shutdown().await.unwrap();

                assert_eq!(resumed, Ok("shared-local".to_owned()));
            })
            .await;
        }

        #[tokio::test]
        async fn applies_rename_and_config_ops_until_shutdown() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let live_id = engine.session_id().to_owned();
                let EngineHandle {
                    ops,
                    mut events,
                    task,
                } = engine.spawn();

                for op in [
                    Op::Rename {
                        title: "Fix login".to_owned(),
                    },
                    Op::SetModel(ConfigPick::Effort(Effort::Low)),
                    Op::SetMode(Mode::Plan),
                    Op::Shutdown,
                ] {
                    ops.send(op).await.unwrap();
                }
                let failure = tokio::time::timeout(Duration::from_secs(5), task.join())
                    .await
                    .expect("Shutdown stops the engine while its op channel is still open")
                    .unwrap();
                let mut seen = Vec::new();
                while let Ok(event) = events.try_recv() {
                    seen.push(event);
                }

                assert_eq!(failure, None);
                assert!(
                    matches!(
                        seen.as_slice(),
                        [
                            Event::SessionTitleUpdated { id, title },
                            Event::ConfigChanged {
                                requested: ConfigPick::Effort(Effort::Low),
                                ..
                            },
                            Event::ModeChanged(Mode::Plan),
                        ] if *id == live_id && title == "Fix login"
                    ),
                    "{seen:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn shutdown_before_any_prompt_leaves_no_session_file() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let handle = engine(server.uri(), &store, dir.path()).await.spawn();

                handle.ops.send(Op::Shutdown).await.unwrap();
                assert_eq!(handle.task.join().await.unwrap(), None);

                assert_eq!(session_ids(&store), [] as [String; 0]);
            })
            .await;
        }

        #[tokio::test]
        async fn reports_the_background_shells_and_ends_them_on_shutdown() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let shells = Arc::clone(&engine.shells);
                let mut handle = engine.spawn();

                start_shell(&shells, "sleep 30").await;
                let listed = next_matching(&mut handle.events, |event| match event {
                    Event::BackgroundShellsUpdated(list) => Some(list),
                    _ => None,
                })
                .await;
                handle.shutdown().await.unwrap();

                assert_eq!(listed, [shell("shell-1", "sleep 30", ShellState::Running)]);
                assert_eq!(shells.list(), []);
            })
            .await;
        }

        #[tokio::test]
        async fn interrupts_mark_each_abandoned_prompt_before_the_next_one() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                mount_stalled_reply_once(&server).await;
                mount_reply(&server, "42").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let live_id = engine.session_id().to_owned();
                let mut handle = engine.spawn();

                let mut cancelled = Vec::new();
                for (count, prompt) in [(1, "write a story"), (2, "write a poem")] {
                    handle.ops.send(Op::prompt(prompt)).await.unwrap();
                    until_requested(&server, count).await;
                    handle.ops.send(Op::Interrupt).await.unwrap();
                    cancelled.extend(turn_events(&mut handle.events).await);
                }
                let next = submit(&mut handle, "what is 7 times 6?").await;
                handle.shutdown().await.unwrap();

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
        async fn interrupt_during_auto_compaction_ends_the_prompt_before_the_model_sees_it() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.compaction = CompactionConfig::with_threshold(50_000);
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.context_estimate = 50_000;
                let mut handle = engine.spawn();

                handle.ops.send(Op::prompt("five")).await.unwrap();
                until_requested(&server, 1).await;
                handle.ops.send(Op::Interrupt).await.unwrap();
                let events = turn_events(&mut handle.events).await;
                handle.shutdown().await.unwrap();

                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::AutoCompactionStarted,
                            Event::PromptUnsent,
                            Event::Cancelled
                        ]
                    ),
                    "{events:?}"
                );
                assert_eq!(
                    request_bodies(&server).await.len(),
                    1,
                    "the prompt never reaches the model"
                );
                assert_eq!(
                    session_ids(&store),
                    [] as [String; 0],
                    "neither the prompt nor an interruption marker is recorded",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn shutdown_mid_request_stops_without_a_terminal_event() {
            with_pinned_home(async {
                for (label, op) in [
                    ("prompt", Op::prompt("mid stream")),
                    ("compaction", Op::Compact { instructions: None }),
                ] {
                    let server = MockServer::start().await;
                    mount_stalled_reply_once(&server).await;
                    let dir = tempfile::tempdir().unwrap();
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    engine.transcript.set_messages(tool_heavy_transcript());
                    let EngineHandle {
                        ops,
                        mut events,
                        task,
                    } = engine.spawn();

                    ops.send(op).await.unwrap();
                    until_requested(&server, 1).await;
                    ops.send(Op::Shutdown).await.unwrap();
                    let failure = tokio::time::timeout(Duration::from_secs(5), task.join())
                        .await
                        .unwrap_or_else(|_| {
                            panic!("Shutdown stops a {label} while the op channel is open")
                        })
                        .unwrap();

                    assert_eq!(failure, None, "{label}");
                    assert!(events.try_recv().is_err(), "{label}");
                }
            })
            .await;
        }

        #[tokio::test]
        async fn finalizes_when_the_op_channel_closes_mid_stream() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let live_id = engine.session_id().to_owned();
                let read_path = dir.path().join("read.txt");
                std::fs::write(&read_path, "contents").unwrap();
                seed_full_read(&engine.transcript.file_tracker, &read_path);
                let handle = engine.spawn();

                handle.ops.send(Op::prompt("mid stream")).await.unwrap();
                until_requested(&server, 1).await;
                tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
                    .await
                    .expect("the engine must stop waiting on the stream")
                    .unwrap();

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
        async fn serves_login_and_logout_and_ends_a_sign_in_on_interrupt_or_shutdown() {
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
                    } = engine.spawn();

                    ops.send(Op::Login).await.unwrap();
                    let (url, _) = login_urls(&mut events).await;
                    ops.send(Op::Interrupt).await.unwrap();
                    let cancelled = events.recv().await;
                    ops.send(Op::Logout).await.unwrap();
                    let signed_out = events.recv().await;
                    ops.send(Op::Login).await.unwrap();
                    login_urls(&mut events).await;
                    ops.send(Op::Shutdown).await.unwrap();
                    let failure = tokio::time::timeout(Duration::from_secs(5), task.join())
                        .await
                        .expect("Shutdown stops the engine mid sign-in")
                        .unwrap();

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
    }

    mod submit_prompt {
        use super::*;

        struct BlockedWriteTool {
            path: PathBuf,
            started: mpsc::UnboundedSender<CancellationToken>,
            release: Arc<Notify>,
        }

        fn assert_workspace_write(path: &Path, tracker: &FileTracker, expected: &[u8]) {
            assert_eq!(std::fs::read(path).unwrap(), expected);
            assert_eq!(
                tracker.verify_current_content(path, expected, GatePurpose::Write),
                Ok(())
            );
        }

        fn assert_resumed_write(sink: &CapturingSink) {
            let events = sink.events();
            let result = events
                .iter()
                .find_map(|event| match event {
                    Event::ToolCallEnd(result) if result.id == "toolu_next" => Some(result),
                    _ => None,
                })
                .unwrap();

            assert!(!result.is_error, "{}", result.content);
        }

        impl Tool for BlockedWriteTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    definition: ToolDefinition {
                        name: "blocked-write".to_owned(),
                        description: "Controlled workspace write.".to_owned(),
                        input_schema: serde_json::json!({
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        }),
                    },
                    class: PermissionClass::Read,
                    parallel_safe: false,
                }
            }

            fn run<'a>(
                &'a self,
                prepared: Prepared,
                ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(async move {
                    match crate::tool::write::testing::write_with_blocked_parent(
                        &self.path,
                        prepared.input["text"].as_str().unwrap(),
                        ctx,
                        self.started.clone(),
                        Arc::clone(&self.release),
                    )
                    .await
                    {
                        Ok(()) => ToolOutput::success("wrote"),
                        Err(error) => ToolOutput::error(error),
                    }
                })
            }
        }

        async fn assert_workspace_writes_drained(name: &str, op: Op, expected: TurnEnd) {
            let server = MockServer::start().await;
            mount_tool_call_once(
                &server,
                "toolu_write",
                "blocked-write",
                r#"{"text":"committed"}"#,
            )
            .await;
            let dir = canonical_tempdir();
            let store = test_store(dir.path());
            let mut engine = engine(server.uri(), &store, dir.path()).await;
            let session_id = engine.session_id().to_owned();
            let parent = dir.path().join("nested");
            let path = parent.join("notes.txt");
            let tracker = Arc::clone(&engine.transcript.file_tracker);
            let (started, mut started_rx) = mpsc::unbounded_channel();
            let release = Arc::new(Notify::new());
            engine.tools = Arc::new(ToolRegistry::new(vec![Box::new(BlockedWriteTool {
                path: path.clone(),
                started,
                release: Arc::clone(&release),
            })]));
            let sink = CapturingSink::default();
            let (tx, mut ops) = mpsc::channel(1);
            let ended = {
                let mut turn = std::pin::pin!(engine.submit_prompt("write", None, &sink, &mut ops));
                let cancel = tokio::select! {
                    token = started_rx.recv() => token.unwrap(),
                    end = turn.as_mut() => panic!("turn ended before its write started: {end:?}"),
                };

                tx.try_send(op).unwrap();
                assert!(futures::poll!(turn.as_mut()).is_pending(), "{name}");
                assert_writes_draining(&tracker);
                assert!(cancel.is_cancelled(), "{name}");
                assert!(!parent.exists(), "{name}");
                assert_eq!(tracker.snapshot_all(), [], "{name}");
                assert!(
                    !sink
                        .events()
                        .iter()
                        .any(|event| { matches!(event, Event::ToolCallEnd(_) | Event::Cancelled) }),
                    "{name}"
                );
                release.notify_one();
                turn.await
            };
            assert_eq!(ended, expected, "{name}");

            assert_workspace_write(&path, &tracker, b"committed");
            assert_eq!(
                sink.events()
                    .iter()
                    .filter(|event| matches!(event, Event::Cancelled))
                    .count(),
                usize::from(expected == TurnEnd::Cancelled),
                "{name}",
            );
            if expected == TurnEnd::Cancelled {
                server.reset().await;
                mount_tool_call_once(
                    &server,
                    "toolu_next",
                    "blocked-write",
                    r#"{"text":"later"}"#,
                )
                .await;
                mount_reply(&server, "done").await;
                let mut next =
                    std::pin::pin!(engine.submit_prompt("write again", None, &sink, &mut ops));
                let token = tokio::select! {
                    token = started_rx.recv() => token.unwrap(),
                    end = next.as_mut() => panic!("next turn ended before its write started: {end:?}"),
                };
                assert!(!token.is_cancelled(), "{name}");
                assert_workspace_write(&path, &tracker, b"committed");
                release.notify_one();
                assert_eq!(next.await, TurnEnd::Completed, "{name}");
                assert_resumed_write(&sink);
                assert_workspace_write(&path, &tracker, b"later");
            }
            assert_eq!(engine.finalize().await, None, "{name}");
            assert_eq!(
                store.load_session_data(&session_id).unwrap().file_snapshots,
                tracker.snapshot_all(),
                "{name}",
            );
        }

        #[tokio::test]
        async fn interrupt_and_shutdown_wait_for_workspace_writes_before_ending() {
            with_pinned_home(async {
                for (name, op, expected) in [
                    ("interrupt", Op::Interrupt, TurnEnd::Cancelled),
                    ("shutdown", Op::Shutdown, TurnEnd::Shutdown),
                ] {
                    Box::pin(assert_workspace_writes_drained(name, op, expected)).await;
                }
            })
            .await;
        }

        /// Records the session ID and artifact directory each of its calls runs with.
        #[derive(Default)]
        struct SessionProbeTool {
            seen: Arc<Mutex<Vec<(String, PathBuf)>>>,
        }

        impl Tool for SessionProbeTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    definition: ToolDefinition {
                        name: "probe".to_owned(),
                        description: "Report the call's session.".to_owned(),
                        input_schema: serde_json::json!({"type": "object"}),
                    },
                    class: PermissionClass::Read,
                    parallel_safe: false,
                }
            }

            fn run<'a>(
                &'a self,
                _prepared: Prepared,
                ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                self.seen
                    .lock()
                    .unwrap()
                    .push((ctx.session_id.clone(), ctx.artifact_dir.to_path_buf()));
                Box::pin(async { ToolOutput::success("probed") })
            }
        }

        #[tokio::test]
        async fn runs_every_round_and_turn_in_the_live_session() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let probe = SessionProbeTool::default();
                let seen = Arc::clone(&probe.seen);
                let mut engine = engine(server.uri(), &store, dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(probe)]));
                let sink = CapturingSink::default();
                let live = |engine: &Engine| {
                    let session = engine.transcript.session();
                    (
                        session.session_id().to_owned(),
                        session.artifact_dir().to_path_buf(),
                    )
                };
                mount_tool_call_once(&server, "toolu_1", "probe", "{}").await;
                mount_tool_call_once(&server, "toolu_2", "probe", "{}").await;
                mount_reply_once(&server, "done").await;

                engine
                    .submit_prompt("probe twice", None, &sink, &mut inert_ops())
                    .await;
                let first = live(&engine);
                engine.clear(&sink).await;
                mount_tool_call_once(&server, "toolu_3", "probe", "{}").await;
                mount_reply_once(&server, "done").await;
                engine
                    .submit_prompt("probe again", None, &sink, &mut inert_ops())
                    .await;
                let cleared = live(&engine);

                assert_ne!(first, cleared);
                assert_eq!(*seen.lock().unwrap(), [first.clone(), first, cleared]);
            })
            .await;
        }

        /// Answers the next Messages request with `text`.
        async fn mount_reply_once(server: &MockServer, text: &str) {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(text_stream_body("", text)))
                .up_to_n_times(1)
                .mount(server)
                .await;
        }

        /// Answers the next Messages request with a refusal that reports `input_tokens`.
        async fn mount_refusal_once(server: &MockServer, input_tokens: u64) {
            let start = serde_json::json!({
                "type": "message_start",
                "message": {"id": "msg_1", "model": MODEL, "usage": {"input_tokens": input_tokens}},
            });
            let stop = serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "refusal"},
                "usage": {"output_tokens": 1},
            });
            let end = serde_json::json!({"type": "message_stop"});
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(sse_body(&[start, stop, end])))
                .up_to_n_times(1)
                .mount(server)
                .await;
        }

        /// Answers the next Messages request with a `tasks` call that sets [`plan`], billed at
        /// 60,000 input tokens.
        async fn mount_heavy_tasks_call_once(server: &MockServer) {
            let input = serde_json::json!({ "tasks": plan() }).to_string();
            let frames = [
                serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "model": MODEL, "usage": {"input_tokens": 60_000, "output_tokens": 0}}}),
                serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call-1", "name": "tasks"}}),
                serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input}}),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 100}}),
                serde_json::json!({"type": "message_stop"}),
            ];
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(sse_body(&frames)))
                .up_to_n_times(1)
                .mount(server)
                .await;
        }

        /// Answers the next Messages request with `text` after `delay`, so an op sent meanwhile
        /// reaches the engine mid-request.
        async fn mount_delayed_reply_once(server: &MockServer, text: &str, delay: Duration) {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(sse_response(text_stream_body("", text)).set_delay(delay))
                .up_to_n_times(1)
                .mount(server)
                .await;
        }

        /// Engine past the compaction floor that compacts at 50,000 tokens, with the `tasks` tool,
        /// whose next prompt's first round is [`mount_heavy_tasks_call_once`].
        async fn mid_turn_engine(server: &MockServer, store: &SessionStore, dir: &Path) -> Engine {
            mount_heavy_tasks_call_once(server).await;
            let mut engine = engine(server.uri(), store, dir)
                .await
                .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
            engine.compaction = CompactionConfig::with_threshold(50_000);
            engine.transcript.set_messages(tool_heavy_transcript());
            engine
        }

        fn mid_turn_summary() -> Message {
            synthesize_post_compact_message(
                "mid-turn summary",
                &plan(),
                &[],
                CompactTrigger::MidTurn,
            )
        }

        /// Each event variant's `snake_case` name, so a test can compare a sequence by name.
        fn event_type(event: &Event) -> &'static str {
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
                Event::ArchiveUpdated { .. } => "archive_updated",
                Event::SavedFound(_) => "saved_found",
                Event::SavedDeleted { .. } => "saved_deleted",
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

        fn event_types(events: &[Event]) -> Vec<&'static str> {
            events.iter().map(event_type).collect()
        }

        #[tokio::test]
        async fn keeps_the_prefix_and_sends_a_date_change_as_a_reminder() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                let today = engine.transcript.prompt().seen.date.clone();
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.submit_prompt("first", None, &sink, &mut ops).await;
                assert_eq!(end, TurnEnd::Completed);
                "2000-01-01".clone_into(&mut engine.transcript.prompt_mut().seen.date);
                let end = engine.submit_prompt("second", None, &sink, &mut ops).await;
                assert_eq!(end, TurnEnd::Completed);

                let turns = request_bodies(&server).await;
                assert_eq!(
                    turns.len(),
                    2,
                    "a session without titles must not request one"
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

                let id = engine.transcript.session().session_id().to_owned();
                let expected = engine.transcript.prompt().session_context();
                engine.finalize().await;
                let data = store.load_session_data(&id).unwrap();
                assert_eq!(data.context, Some(expected));
                assert!(matches!(
                    &data.messages[2].content[..],
                    [_, ContentBlock::Text { text, .. }] if text == reminder
                ));
            })
            .await;
        }

        #[tokio::test]
        async fn shows_an_instruction_warning_each_time_it_appears() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let mut ops = inert_ops();
                let warning = write_unreadable_instructions(dir.path());

                engine.submit_prompt("broken", None, &sink, &mut ops).await;
                engine
                    .submit_prompt("still broken", None, &sink, &mut ops)
                    .await;
                std::fs::write(dir.path().join("CLAUDE.md"), "Fixed rules.").unwrap();
                engine.submit_prompt("fixed", None, &sink, &mut ops).await;
                write_unreadable_instructions(dir.path());
                engine
                    .submit_prompt("broken again", None, &sink, &mut ops)
                    .await;

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
        async fn reports_usage_and_accumulates_cost_across_turns() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                for text in ["first", "second"] {
                    engine.submit_prompt(text, None, &sink, &mut ops).await;
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
        async fn bills_nothing_for_a_title_request_that_fails() {
            with_pinned_home(async {
                for (case, title_finishes_first) in [("title first", true), ("turn first", false)] {
                    let server = MockServer::start().await;
                    let failure = ResponseTemplate::new(500).insert_header("retry-after", "0");
                    mount_title(&server, failure).await;
                    mount_reply(&server, "reply").await;
                    let dir = tempfile::tempdir().unwrap();
                    let store = test_store(dir.path());
                    let mut engine = engine(server.uri(), &store, dir.path()).await;
                    let session_id = engine.session_id().to_owned();
                    let cache_ttl = engine.client.prompt_cache_ttl();
                    let sink = CapturingSink::default();
                    let title = title_generator::spawn(
                        engine.client.clone(),
                        engine.transcript.session().clone(),
                        sink.clone(),
                        "hello".to_owned(),
                    );
                    let (billed, usage) = oneshot::channel();
                    engine.usage.title_usage = Some(usage);
                    let failure = async {
                        assert!(
                            title.await.is_err(),
                            "{case}: failed title reports no usage"
                        );
                        drop(billed);
                    };
                    let mut ops = inert_ops();
                    let turn = engine.submit_prompt("hello", None, &sink, &mut ops);

                    let end = if title_finishes_first {
                        failure.await;
                        turn.await
                    } else {
                        let end = turn.await;
                        failure.await;
                        end
                    };
                    assert_eq!(end, TurnEnd::Completed, "{case}");
                    engine.emit_usage_update(&sink);
                    engine.finalize().await;

                    let shown = sink
                        .events()
                        .into_iter()
                        .rev()
                        .find_map(|event| match event {
                            Event::UsageUpdated(usage) => usage.estimated_cost_usd,
                            _ => None,
                        });
                    let expected = request_cost(MODEL, REPLY_USAGE);
                    assert!(
                        shown.is_some_and(|cost| (cost - expected).abs() < 1e-12),
                        "{case}: only the turn counts: {shown:?}"
                    );
                    let data = store.load_session_data(&session_id).unwrap();
                    assert_eq!(
                        data.usage,
                        [UsageRecord {
                            model: MODEL.to_owned(),
                            cache_ttl,
                            tokens: REPLY_USAGE,
                        }],
                        "{case}"
                    );
                }
            })
            .await;
        }

        #[tokio::test]
        async fn adopts_the_task_list_a_tasks_call_sets() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let input = serde_json::json!({ "tasks": plan() }).to_string();
                mount_tool_call_once(&server, "call-1", "tasks", &input).await;
                mount_reply(&server, "done").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.submit_prompt("plan it", None, &sink, &mut ops).await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(engine.transcript.tasks, plan());
                let events = sink.events();
                let updated = events
                    .iter()
                    .position(|event| matches!(event, Event::TasksUpdated(_)))
                    .expect("the tasks call reports the new list");
                assert!(
                    matches!(
                        &events[updated - 1],
                        Event::ToolCallEnd(result) if result.id == "call-1"
                    ),
                    "{events:?}"
                );
                assert_eq!(events[updated], Event::TasksUpdated(plan()));
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_the_list_through_a_later_rejected_tasks_call() {
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
                let mut ops = inert_ops();

                let end = engine
                    .submit_prompt("plan it", None, &CapturingSink::default(), &mut ops)
                    .await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(server.received_requests().await.unwrap().len(), 3);
                assert_eq!(engine.transcript.tasks, plan());
            })
            .await;
        }

        #[tokio::test]
        async fn resumed_first_prompt_refusal_requests_a_new_title_unless_manually_named() {
            with_pinned_home(async {
                for manual in [false, true] {
                    let refused_server = MockServer::start().await;
                    mount_refusal_once(&refused_server, 1_000).await;
                    let dir = tempfile::tempdir().unwrap();
                    let store = test_store(dir.path());
                    let mut original = engine(refused_server.uri(), &store, dir.path()).await;
                    let session_id = original.session_id().to_owned();
                    assert_eq!(
                        original
                            .submit_prompt(
                                "refused",
                                None,
                                &CapturingSink::default(),
                                &mut inert_ops()
                            )
                            .await,
                        TurnEnd::Failed
                    );
                    if manual {
                        let renamed = original
                            .transcript
                            .session()
                            .set_manual_title("Manual title".to_owned())
                            .await;
                        assert_eq!(renamed.failure, None);
                    }
                    assert_eq!(original.finalize().await, None);
                    let server = MockServer::start().await;
                    mount_title(&server, greeting_title()).await;
                    mount_reply(&server, "accepted reply").await;
                    let mut resumed = engine(server.uri(), &store, dir.path()).await;
                    resumed
                        .resume(
                            &CapturingSink::default(),
                            ResumeTarget::Path(store.find_session_path(&session_id).unwrap()),
                        )
                        .await;
                    resumed.generates_titles = true;
                    let (sink, mut events) = mpsc::unbounded_channel();

                    let end = resumed
                        .submit_prompt("accepted", None, &sink, &mut inert_ops())
                        .await;
                    assert_eq!(end, TurnEnd::Completed);
                    if manual {
                        assert!(resumed.usage.title_usage.is_none());
                    } else {
                        let title = tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                if let Some(Event::SessionTitleUpdated { title, .. }) =
                                    events.recv().await
                                {
                                    break title;
                                }
                            }
                        })
                        .await
                        .expect("the title request completes");
                        assert_eq!(title, "Greeting");
                    }
                    assert_eq!(resumed.finalize().await, None);

                    let requests = request_bodies(&server).await;
                    let title_requests: Vec<_> = requests
                        .iter()
                        .filter(|request| request["model"] == TITLE_MODEL)
                        .collect();
                    assert_eq!(title_requests.len(), usize::from(!manual));
                    assert_eq!(
                        store
                            .load_session_data(&session_id)
                            .unwrap()
                            .title
                            .unwrap()
                            .title,
                        if manual { "Manual title" } else { "Greeting" }
                    );
                }
            })
            .await;
        }

        #[tokio::test]
        async fn adopts_the_list_of_a_turn_that_fails_after_the_call() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let input = serde_json::json!({ "tasks": plan() }).to_string();
                mount_tool_call_once(&server, "call-1", "tasks", &input).await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(ToolRegistry::new(vec![Box::new(TasksTool)]));
                engine.max_tool_rounds = Some(1);
                let mut ops = inert_ops();

                let end = engine
                    .submit_prompt("plan it", None, &CapturingSink::default(), &mut ops)
                    .await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.tasks, plan());
            })
            .await;
        }

        #[tokio::test]
        async fn rolls_back_a_refused_turn_to_the_state_before_it() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = canonical_tempdir();
                let notes = dir.path().join("notes.txt");
                let other = dir.path().join("other.txt");
                std::fs::write(&notes, "notes").unwrap();
                std::fs::write(&other, "other").unwrap();
                let tasks = serde_json::json!({ "tasks": plan() }).to_string();
                let read = serde_json::json!({ "file_path": notes }).to_string();
                mount_tool_call_once(&server, "call-1", "tasks", &tasks).await;
                mount_tool_call_once(&server, "call-2", "read", &read).await;
                mount_reply_once(&server, "done").await;
                let refused_tasks = serde_json::json!({ "tasks": [] }).to_string();
                let refused_read = serde_json::json!({ "file_path": other }).to_string();
                mount_tool_call_once(&server, "call-3", "tasks", &refused_tasks).await;
                mount_tool_call_once(&server, "call-4", "read", &refused_read).await;
                mount_refusal_once(&server, 5_000).await;
                mount_reply(&server, "reply").await;
                let store = test_store(dir.path());
                let mut engine =
                    engine(server.uri(), &store, dir.path())
                        .await
                        .with_tools(ToolRegistry::new(vec![
                            Box::new(TasksTool),
                            Box::new(ReadTool),
                        ]));
                let today = engine.transcript.prompt().seen.date.clone();
                let sink = CapturingSink::default();
                let mut ops = inert_ops();
                let first = engine.submit_prompt("first", None, &sink, &mut ops).await;
                assert_eq!(first, TurnEnd::Completed);
                "2000-01-01".clone_into(&mut engine.transcript.prompt_mut().seen.date);
                let before_refusal = sink.events().len();

                let refused = engine.submit_prompt("refused", None, &sink, &mut ops).await;
                let refused_events = sink.events().split_off(before_refusal);
                let tracked = tracked_paths(&engine);
                let tasks = engine.transcript.tasks.clone();
                let rephrased = engine
                    .submit_prompt("rephrased", None, &sink, &mut ops)
                    .await;

                assert_eq!(refused, TurnEnd::Failed);
                let [
                    ..,
                    restored,
                    Event::UsageUpdated(usage),
                    rolled_back,
                    failed,
                ] = &refused_events[..]
                else {
                    panic!("{refused_events:?}");
                };
                assert_eq!(*restored, Event::TasksUpdated(plan()));
                assert_eq!(usage.context_tokens, Some(REPLY_USAGE.context_tokens()));
                assert_eq!(*rolled_back, Event::TurnRolledBack);
                assert_eq!(
                    *failed,
                    Event::TurnFailed(
                        "The model declined this request. \
                         Changes from the request's tool calls remain. \
                         The request was removed from the conversation, \
                         so rephrase it to try again."
                            .to_owned()
                    ),
                );
                assert_eq!(tracked, [notes]);
                assert_eq!(tasks, plan());
                assert_eq!(rephrased, TurnEnd::Completed);
                let turns = request_bodies(&server).await;
                let messages = turns.last().unwrap()["messages"].as_array().unwrap();
                let [.., done, prompt] = &messages[..] else {
                    panic!("{messages:?}");
                };
                assert_eq!(done["content"][0]["text"], "done");
                assert_eq!(prompt["content"][0]["text"], "rephrased");
                let reminder = prompt["content"][1]["text"].as_str().unwrap();
                assert!(
                    reminder.contains(&format!("Today's date is {today}.")),
                    "the rolled-back reminder is sent again: {reminder}"
                );

                let id = engine.transcript.session().session_id().to_owned();
                let expected = engine.transcript.prompt().session_context();
                engine.finalize().await;
                let data = store.load_session_data(&id).unwrap();
                let [.., done, prompt, reply] = &data.messages[..] else {
                    panic!("{:?}", data.messages);
                };
                assert_eq!(*done, Message::assistant("done"));
                assert!(matches!(
                    &prompt.content[..],
                    [ContentBlock::Text { text, .. }, _] if text == "rephrased"
                ));
                assert_eq!(*reply, Message::assistant("reply"));
                assert_eq!(data.context, Some(expected));
            })
            .await;
        }

        #[tokio::test]
        async fn rolls_back_a_refused_turn_in_memory_when_the_session_is_gone() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_refusal_once(&server, 1_000).await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.transcript.set_session(dead_test_session());
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let refused = engine.submit_prompt("refused", None, &sink, &mut ops).await;
                let rephrased = engine
                    .submit_prompt("rephrased", None, &sink, &mut ops)
                    .await;

                assert_eq!(refused, TurnEnd::Failed);
                assert_eq!(rephrased, TurnEnd::Completed);
                let turns = request_bodies(&server).await;
                assert_eq!(texts(&turns[1]), [SESSION_CONTEXT, "rephrased"]);
            })
            .await;
        }

        #[tokio::test]
        async fn auto_compacts_before_recording_the_new_prompt() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "auto summary").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.compaction = CompactionConfig::with_threshold(50_000);
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.context_estimate = 50_000;
                engine.usage.displayed_usage = Some(TokenUsage::new(50_000, 1));
                engine.usage.total_estimated_cost_usd = 1.23;
                engine.usage.auto_compaction_failures = AutoCompactFailures::new(2);
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.submit_prompt("five", None, &sink, &mut ops).await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(
                    engine.usage.auto_compaction_failures,
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
                    &engine.transcript.messages()[0].content[0],
                    ContentBlock::Text { text, .. } if text.contains("auto summary")
                ));
                assert_eq!(engine.transcript.messages().len(), 3);
                assert_eq!(
                    engine.usage.context_estimate,
                    1_000 + compaction::transcript_tokens(&engine.transcript.messages()[2..]),
                    "the turn's input plus its reply",
                );
                assert!(
                    (engine.usage.total_estimated_cost_usd - (1.23 + 0.006 + 0.006)).abs() < 1e-12,
                    "the session cost keeps earlier turns and adds the summary and the turn: {}",
                    engine.usage.total_estimated_cost_usd,
                );
                let events = sink.events();
                let compacted = events
                    .iter()
                    .position(|event| {
                        matches!(
                            event,
                            Event::SessionCompacted {
                                trigger: CompactTrigger::Auto,
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
        async fn compacts_a_rejected_oversized_prompt_before_the_next_prompt() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply_once(&server, "first reply").await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": "invalid_request_error",
                            "message": "prompt is too long: 60000 tokens > 50000 maximum",
                        },
                    })))
                    .up_to_n_times(1)
                    .mount(&server)
                    .await;
                mount_reply_once(&server, "Preserve the pasted service log investigation.").await;
                mount_reply(&server, "continued").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.compaction = CompactionConfig::with_threshold(50_000);
                let sink = CapturingSink::default();
                let mut ops = inert_ops();
                let oversized = "service log entry\n".repeat(15_000);

                let first = engine.submit_prompt("hello", None, &sink, &mut ops).await;
                assert_eq!(first, TurnEnd::Completed);
                assert_eq!(
                    engine.usage.context_estimate,
                    1_000 + compaction::transcript_tokens(&engine.transcript.messages()[1..])
                );
                let rejected = engine
                    .submit_prompt(&oversized, None, &sink, &mut ops)
                    .await;
                assert_eq!(rejected, TurnEnd::Failed);
                let continued = engine
                    .submit_prompt("continue the investigation", None, &sink, &mut ops)
                    .await;
                assert_eq!(continued, TurnEnd::Completed);

                let events = sink.events();
                assert!(
                    events.iter().any(|event| matches!(
                        event,
                        Event::SessionCompacted {
                            trigger: CompactTrigger::Auto,
                            ..
                        }
                    )),
                    "the rejected prompt must be compacted before continuing: {events:?}"
                );
                let requests = request_bodies(&server).await;
                let summarized = texts(&requests[2]);
                assert_eq!(summarized[summarized.len() - 2], oversized);
                let resumed = texts(&requests[3]);
                assert!(resumed[1].contains("Preserve the pasted service log investigation."));
                assert_eq!(resumed.last(), Some(&"continue the investigation"));
                assert!(!resumed.contains(&oversized.as_str()));
                let saved = store.load_session_data(engine.session_id()).unwrap();
                assert_eq!(saved.messages, engine.transcript.messages());
                assert_eq!(
                    saved.messages.last(),
                    Some(&Message::assistant("continued"))
                );
                assert_eq!(
                    events
                        .iter()
                        .filter_map(|event| match event {
                            Event::TurnFailed(message) => Some(message.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    ["Anthropic API error (HTTP 400 Bad Request): \
                      prompt is too long: 60000 tokens > 50000 maximum"]
                );
            })
            .await;
        }

        #[tokio::test]
        async fn fails_without_sending_the_prompt_when_auto_compaction_fails() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "   ").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = engine(server.uri(), &store, dir.path()).await;
                engine.compaction = CompactionConfig::with_threshold(50_000);
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.context_estimate = 50_000;
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.submit_prompt("five", None, &sink, &mut ops).await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.messages(), tool_heavy_transcript());
                assert_eq!(
                    engine.usage.auto_compaction_failures,
                    AutoCompactFailures::new(1)
                );
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
                            Event::PromptUnsent,
                            Event::TurnFailed(error),
                        ] if token == "   "
                            && error == "Auto-compaction failed: \
                                         the model returned an empty summary. \
                                         The prompt was not sent, so send it again to retry."
                    ),
                    "{events:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_a_mode_switch_sent_during_a_failed_auto_compaction() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "   ").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.compaction = CompactionConfig::with_threshold(50_000);
                engine.transcript.set_messages(tool_heavy_transcript());
                engine.usage.context_estimate = 50_000;
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);
                ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();

                let end = engine.submit_prompt("five", None, &sink, &mut ops).await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.policy.mode(), Mode::Plan);
            })
            .await;
        }

        #[tokio::test]
        async fn fails_while_signed_out_without_sending_a_request() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.client.set_auth(None);
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                let end = engine.submit_prompt("hello", None, &sink, &mut ops).await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.transcript.messages(), [] as [Message; 0]);
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [Event::PromptUnsent, Event::TurnFailed(msg)]
                            if msg == "Not signed in. Run `/login` to sign in."
                    ),
                    "{events:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn compacts_mid_turn_and_continues_the_turn_from_the_summary() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                mount_reply_once(&server, "mid-turn summary").await;
                mount_reply(&server, "done").await;
                let sink = CapturingSink::default();

                let end = engine
                    .submit_prompt("plan it", None, &sink, &mut inert_ops())
                    .await;

                assert_eq!(end, TurnEnd::Completed);
                let transcript = [mid_turn_summary(), Message::assistant("done")];
                assert_eq!(engine.transcript.messages(), transcript);
                let data = store.load_session_data(engine.session_id()).unwrap();
                assert_eq!(data.messages, transcript);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 3);
                let summarized = requests[1]["messages"].as_array().unwrap();
                assert_eq!(
                    summarized[summarized.len() - 2]["content"][0]["tool_use_id"],
                    "call-1",
                    "the summary covers the finished round"
                );
                let Some(ContentBlock::Text {
                    text: continuation, ..
                }) = mid_turn_summary().content.pop()
                else {
                    panic!("the summary message ends on its continuation");
                };
                assert_eq!(
                    requests[2]["messages"][1]["content"]
                        .as_array()
                        .unwrap()
                        .last()
                        .unwrap()["text"],
                    continuation.as_str(),
                    "the turn resumes from the summary"
                );
                assert_eq!(
                    event_types(&sink.events()),
                    [
                        "tool_call_start",
                        "tool_call_end",
                        "tasks_updated",
                        "usage_updated",
                        "auto_compaction_started",
                        "summary_token",
                        "session_compacted",
                        "usage_updated",
                        "stream_token",
                        "usage_updated",
                        "turn_complete",
                    ]
                );
            })
            .await;
        }

        #[tokio::test]
        async fn fails_the_turn_when_the_mid_turn_compaction_fails() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                mount_reply(&server, "   ").await;
                let sink = CapturingSink::default();

                let end = engine
                    .submit_prompt("plan it", None, &sink, &mut inert_ops())
                    .await;

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(request_bodies(&server).await.len(), 2);
                assert_eq!(
                    engine.usage.auto_compaction_failures,
                    AutoCompactFailures::new(1)
                );
                assert_eq!(
                    engine.transcript.messages().len(),
                    tool_heavy_transcript().len() + 3
                );
                let last = &engine.transcript.messages().last().unwrap().content;
                assert!(
                    matches!(
                        &last[..],
                        [ContentBlock::ToolResult { tool_use_id, .. }] if tool_use_id == "call-1"
                    ),
                    "the transcript keeps the finished round: {last:?}"
                );
                let events = sink.events();
                assert!(
                    !events.contains(&Event::PromptUnsent),
                    "the prompt already went out: {events:?}"
                );
                assert_eq!(
                    events.last(),
                    Some(&Event::TurnFailed(
                        "Auto-compaction failed: the model returned an empty summary. \
                         The turn stopped before its next request. \
                         Run `/compact`, then send a message to continue."
                            .to_owned()
                    ))
                );
            })
            .await;
        }

        #[tokio::test]
        async fn rolls_back_a_refusal_after_a_mid_turn_compaction_to_the_summary() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                let read = dir.path().join("read.txt");
                std::fs::write(&read, "read").unwrap();
                seed_full_read(&engine.transcript.file_tracker, &read);
                let summary_delay = Duration::from_millis(200);
                mount_delayed_reply_once(&server, "mid-turn summary", summary_delay).await;
                mount_refusal_once(&server, 1_000).await;
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);
                let queue = async {
                    until_requested(&server, 2).await;
                    let prompt = Op::Queue("also check the docs".to_owned());
                    ops_tx.send(prompt).await.unwrap();
                };

                let (end, ()) = tokio::join!(
                    engine.submit_prompt("plan it", None, &sink, &mut ops),
                    queue
                );

                assert_eq!(end, TurnEnd::Failed);
                let requests = request_bodies(&server).await;
                assert_eq!(
                    texts(&requests[2]).last(),
                    Some(&"also check the docs"),
                    "the prompt queued during the summary follows it"
                );
                assert_eq!(engine.transcript.messages(), [mid_turn_summary()]);
                assert_eq!(
                    engine.transcript.file_tracker.snapshot_all(),
                    [],
                    "the rollback keeps the tracker the summary cleared"
                );
                assert!(
                    !engine.transcript.prompt_unrecorded(),
                    "the summary recorded the prompt"
                );
                let data = store.load_session_data(engine.session_id()).unwrap();
                assert_eq!(data.messages, [mid_turn_summary()]);
                assert_eq!(
                    data.context,
                    Some(engine.transcript.prompt().session_context())
                );
                let events = sink.events();
                assert!(
                    events.contains(&Event::PromptDrained("also check the docs".to_owned())),
                    "the frontend learns which prompt the rollback removes: {events:?}"
                );
                assert_eq!(
                    events[events.len() - 2..],
                    [
                        Event::TurnRolledBack,
                        Event::TurnFailed(
                            "The model declined this request. \
                             The work since this turn's compaction was removed from the \
                             conversation, but the summary still holds the request, \
                             so run `/clear` if the model keeps declining."
                                .to_owned()
                        ),
                    ]
                );
            })
            .await;
        }

        #[tokio::test]
        async fn runs_the_turn_on_without_compaction_while_auto_compaction_is_paused() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                engine.usage.auto_compaction_failures = AutoCompactFailures::new(3);
                mount_reply(&server, "done").await;
                let sink = CapturingSink::default();

                let end = engine
                    .submit_prompt("plan it", None, &sink, &mut inert_ops())
                    .await;

                assert_eq!(end, TurnEnd::Completed);
                assert_eq!(request_bodies(&server).await.len(), 2);
                assert_eq!(
                    engine.transcript.messages().last(),
                    Some(&Message::assistant("done"))
                );
                let events = sink.events();
                assert!(
                    !events.contains(&Event::AutoCompactionStarted),
                    "{events:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn interrupt_or_shutdown_during_a_mid_turn_compaction_ends_on_the_marker() {
            with_pinned_home(async {
                for (op, expected) in [
                    (Op::Interrupt, TurnEnd::Cancelled),
                    (Op::Shutdown, TurnEnd::Shutdown),
                ] {
                    let server = MockServer::start().await;
                    let dir = tempfile::tempdir().unwrap();
                    let store = test_store(dir.path());
                    let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                    mount_stalled_reply_once(&server).await;
                    let sink = CapturingSink::default();
                    let (ops_tx, mut ops) = mpsc::channel(1);
                    let stop = async {
                        until_requested(&server, 2).await;
                        ops_tx.send(op).await.unwrap();
                    };

                    let (end, ()) =
                        tokio::join!(engine.submit_prompt("plan it", None, &sink, &mut ops), stop);

                    assert_eq!(end, expected);
                    assert_eq!(
                        engine.transcript.messages().len(),
                        tool_heavy_transcript().len() + 4
                    );
                    let data = store.load_session_data(engine.session_id()).unwrap();
                    assert_eq!(
                        data.messages.last(),
                        Some(&Message::interruption()),
                        "{expected:?}"
                    );
                }
            })
            .await;
        }

        #[tokio::test]
        async fn compacts_again_later_in_the_turn_and_counts_every_round_toward_the_cap() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                engine.max_tool_rounds = Some(3);
                let long_summary = "Read the logs. ".repeat(400);
                mount_reply_once(&server, &long_summary).await;
                mount_heavy_tasks_call_once(&server).await;
                mount_reply_once(&server, "second summary").await;
                mount_reply(&server, "done").await;
                let sink = CapturingSink::default();

                let end = engine
                    .submit_prompt("plan it", None, &sink, &mut inert_ops())
                    .await;

                assert_eq!(end, TurnEnd::Completed, "{:?}", sink.events().last());
                assert_eq!(request_bodies(&server).await.len(), 5);
                let summary = synthesize_post_compact_message(
                    "second summary",
                    &plan(),
                    &[],
                    CompactTrigger::MidTurn,
                );
                assert_eq!(
                    engine.transcript.messages(),
                    [summary, Message::assistant("done")]
                );
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_a_mode_switch_sent_during_a_failed_mid_turn_compaction() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let mut engine = mid_turn_engine(&server, &store, dir.path()).await;
                mount_delayed_reply_once(&server, "   ", Duration::from_millis(200)).await;
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(1);
                let switch = async {
                    until_requested(&server, 2).await;
                    ops_tx.send(Op::SetMode(Mode::Plan)).await.unwrap();
                };

                let (end, ()) = tokio::join!(
                    engine.submit_prompt("plan it", None, &sink, &mut ops),
                    switch
                );

                assert_eq!(end, TurnEnd::Failed);
                assert_eq!(engine.policy.mode(), Mode::Plan);
            })
            .await;
        }
    }

    mod roll_back {
        use super::*;

        #[tokio::test]
        async fn settles_the_write_before_restoring_the_checkpoint() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = canonical_tempdir();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let path = dir.path().join("notes.txt");
                std::fs::write(&path, b"original").unwrap();
                seed_full_read(&engine.transcript.file_tracker, &path);
                let start = engine.transcript.start_of_turn();
                let snapshots = engine.transcript.file_tracker.snapshot_all();
                let tracker = Arc::clone(&engine.transcript.file_tracker);
                let release = blocked_write(&tracker, &path, b"committed", "toolu_write").await;
                let sink = CapturingSink::default();
                let report = TurnReport::default();
                let mut rollback = std::pin::pin!(engine.roll_back(&sink, start, None, &report));

                assert!(futures::poll!(rollback.as_mut()).is_pending());
                assert_writes_draining(&tracker);
                assert_eq!(std::fs::read(&path).unwrap(), b"original");
                assert_eq!(tracker.snapshot_all(), snapshots);
                assert_eq!(sink.events(), []);
                release.send(()).unwrap();
                rollback.await;

                assert_eq!(std::fs::read(&path).unwrap(), b"committed");
                assert_eq!(tracker.snapshot_all(), snapshots);
                assert_eq!(
                    tracker.verify_current_content(&path, b"committed", GatePurpose::Write),
                    Err(GateError::ContentDrifted {
                        path,
                        purpose: GatePurpose::Write
                    })
                );
                assert!(
                    sink.events()
                        .iter()
                        .any(|event| matches!(event, Event::TurnRolledBack))
                );
            })
            .await;
        }
    }

    mod set_model {
        use super::*;

        fn sonnet_pick() -> ConfigPick {
            ConfigPick::Model {
                id: ResolvedModelId::new("claude-sonnet-5-5"),
                effort: None,
            }
        }

        #[tokio::test]
        async fn notices_the_change_against_the_settings_before_it() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.client.set_effort(Effort::High);
                let opus = ModelSpec::resolve(MODEL);
                engine.compaction =
                    CompactionConfig::with_default_policy(&opus, engine.client.max_tokens());
                let sink = CapturingSink::default();

                for id in ["custom-model", MODEL] {
                    let id = ResolvedModelId::new(id);
                    engine.set_model(&sink, ConfigPick::Model { id, effort: None });
                }

                let notices: Vec<String> = sink
                    .events()
                    .into_iter()
                    .filter_map(|event| match event {
                        Event::ConfigChanged { notice, .. } => Some(notice),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    notices,
                    [
                        "Switched to custom-model (custom-model). \
                         Effort cleared (model has no effort levels). \
                         Auto-compaction off (no threshold). \
                         `custom-model` is not a supported model, \
                         so requests go without thinking, effort, and context management, \
                         and cost tracking stays off. \
                         Auto-compaction requires an explicit token threshold."
                            .to_owned(),
                        "Switched to Claude Opus 5.5 (claude-opus-5-5) · \
                         effort medium (model default). \
                         Auto-compaction at 986k tokens."
                            .to_owned(),
                    ],
                );
            })
            .await;
        }

        #[tokio::test]
        async fn swap_keeps_the_system_prompt_and_names_the_model_next_turn() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                engine.submit_prompt("first", None, &sink, &mut ops).await;
                engine.set_model(&sink, sonnet_pick());
                engine.submit_prompt("second", None, &sink, &mut ops).await;

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
        async fn swap_keeps_sending_back_the_reasoning_of_the_previous_model() {
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
                let sink = CapturingSink::default();
                let mut ops = inert_ops();

                engine.submit_prompt("first", None, &sink, &mut ops).await;
                engine.set_model(&sink, sonnet_pick());
                engine.submit_prompt("second", None, &sink, &mut ops).await;

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
        async fn resolves_the_compaction_threshold_for_the_new_model() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let mut config = test_config(server.uri(), api_key(), "custom-model");
                let sink = CapturingSink::default();
                let unwindowed = CompactionConfig::with_default_policy(&config.model, 16_000);
                assert_eq!(
                    unwindowed.auto.threshold_tokens, None,
                    "precondition: no window"
                );
                let mut resolved = Vec::new();

                for (max_tokens, configured, threshold) in [
                    (None, unwindowed, WINDOW_1M_THRESHOLD),
                    (
                        None,
                        CompactionConfig::with_threshold(1_500_000),
                        WINDOW_1M_THRESHOLD,
                    ),
                    (Some(8_000), unwindowed, 979_000),
                ] {
                    config.max_tokens = max_tokens;
                    engine.client = client_for(&config);
                    engine.compaction = configured;

                    engine.set_model(&sink, sonnet_pick());

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
        async fn resumes_paused_auto_compaction() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                engine.usage.auto_compaction_failures = AutoCompactFailures::new(3);
                engine.usage.context_estimate = 100_000;
                engine.usage.total_estimated_cost_usd = 1.23;
                let sink = CapturingSink::default();

                engine.set_model(&sink, ConfigPick::Effort(Effort::Xhigh));

                assert_eq!(
                    engine.usage.auto_compaction_failures,
                    AutoCompactFailures::default()
                );
                assert_eq!(engine.usage.context_estimate, 100_000);
                assert!(
                    (engine.usage.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON,
                    "session cost must accumulate across swaps: {}",
                    engine.usage.total_estimated_cost_usd,
                );
                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::AutoCompactionResumed,
                            Event::UsageUpdated(_),
                            Event::ConfigChanged { .. },
                        ]
                    ),
                    "{events:?}",
                );
            })
            .await;
        }
    }

    mod set_mode {
        use super::*;

        #[tokio::test]
        async fn switches_the_gate_and_confirms() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();

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
    }

    mod adopt_mode {
        use super::*;

        #[tokio::test]
        async fn keeps_a_switch_that_arrives_mid_turn() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let (tx, mut ops) = mpsc::channel(1);
                tx.try_send(Op::SetMode(Mode::Plan)).unwrap();

                engine.submit_prompt("hello", None, &sink, &mut ops).await;

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
    }

    mod finalize {
        use super::*;

        #[tokio::test]
        async fn snapshots_the_committed_bytes_after_draining_writes() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = canonical_tempdir();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id().to_owned();
                let path = dir.path().join("notes.txt");
                record_write_call(
                    engine.transcript.session(),
                    &engine.transcript.file_tracker,
                    "toolu_write",
                    &path,
                    true,
                )
                .await;
                let tracker = Arc::clone(&engine.transcript.file_tracker);
                let previous = tracker.snapshot_all();
                let release = blocked_write(&tracker, &path, b"committed", "toolu_write").await;
                let mut finalization = std::pin::pin!(engine.finalize());

                assert!(futures::poll!(finalization.as_mut()).is_pending());
                assert_writes_draining(&tracker);
                assert_eq!(std::fs::read(&path).unwrap(), b"alpha");
                assert_eq!(tracker.snapshot_all(), previous);
                release.send(()).unwrap();
                assert_eq!(finalization.await, None);

                assert_eq!(std::fs::read(&path).unwrap(), b"committed");
                let saved = store.load_session_data(&session_id).unwrap().file_snapshots;
                assert_eq!(saved, tracker.snapshot_all());
                let (restored, dropped) = FileTracker::restored(saved).await;
                assert_eq!(dropped, [] as [PathBuf; 0]);
                assert_eq!(
                    restored.verify_current_content(&path, b"committed", GatePurpose::Write),
                    Ok(())
                );
            })
            .await;
        }
    }

    mod report_abort {
        use super::*;

        #[test]
        fn shows_cancel_and_failure_but_keeps_shutdown_silent() {
            let sink = CapturingSink::default();

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
    }

    mod report_unsent {
        use super::*;

        #[test]
        fn hands_the_prompt_back_before_the_terminal_event_but_not_on_shutdown() {
            let sink = CapturingSink::default();

            assert_eq!(
                report_unsent(&sink, TurnAbort::Cancelled, "label"),
                TurnEnd::Cancelled
            );
            assert_eq!(
                report_unsent(&sink, TurnAbort::Shutdown, "label"),
                TurnEnd::Shutdown
            );
            assert_eq!(
                report_unsent(&sink, TurnAbort::Failed(anyhow!("boom")), "label"),
                TurnEnd::Failed
            );

            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        Event::PromptUnsent,
                        Event::Cancelled,
                        Event::PromptUnsent,
                        Event::TurnFailed(msg)
                    ] if msg == "boom"
                ),
                "{events:?}",
            );
        }
    }
}
