//! Session drivers for the TUI, the bare REPL, and headless mode, all built on [`Conversation`].

pub(crate) mod headless;
pub(crate) mod repl;
pub(crate) mod tui;

#[cfg(test)]
mod testing;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::anyhow;
use tokio::sync::mpsc;

use crate::agent::event::{AgentEvent, AgentSink, UsageSnapshot, UserAction};
use crate::agent::{self, AutoCompact, TokenUsage, TurnAbort, TurnOutcome, TurnReport};
use crate::client::anthropic::Client;
use crate::config::Effort;
use crate::file_tracker::FileTracker;
use crate::message::{ContentBlock, Message};
use crate::model::{self, ResolvedModelId};
use crate::prompt::{self, PromptParts, SessionContext, UserContext};
use crate::session::handle::{self, SessionHandle};
use crate::session::store::SessionStore;
use crate::session::title_generator;
use crate::tool::ToolRegistry;

// ── Exit Status ──

/// How a driver ended, mapped to the process exit status by [`Exit::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Success,
    Failure,
    Signal(Shutdown),
}

impl Exit {
    pub(crate) fn code(self) -> ExitCode {
        match self {
            Self::Success => ExitCode::SUCCESS,
            Self::Failure => ExitCode::FAILURE,
            Self::Signal(signal) => ExitCode::from(signal.exit_status()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shutdown {
    #[cfg(unix)]
    Hangup,
    Interrupt,
    #[cfg(unix)]
    Terminate,
}

impl Shutdown {
    /// 128 plus the signal number, following the shell convention.
    const fn exit_status(self) -> u8 {
        match self {
            #[cfg(unix)]
            Self::Hangup => 129,
            Self::Interrupt => 130,
            #[cfg(unix)]
            Self::Terminate => 143,
        }
    }
}

/// Resolves on the first SIGINT / SIGTERM / SIGHUP.
async fn shutdown_signal() -> Shutdown {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let ctrl_c = tokio::signal::ctrl_c();
        let (Ok(mut sigterm), Ok(mut sighup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            _ = ctrl_c.await;
            return Shutdown::Interrupt;
        };
        tokio::select! {
            _ = ctrl_c => Shutdown::Interrupt,
            _ = sigterm.recv() => Shutdown::Terminate,
            _ = sighup.recv() => Shutdown::Hangup,
        }
    }
    #[cfg(not(unix))]
    {
        _ = tokio::signal::ctrl_c().await;
        Shutdown::Interrupt
    }
}

// ── Conversation ──

/// The live session with its transcript, prompt, and usage accounting.
pub(crate) struct Conversation {
    client: Client,
    tools: Arc<ToolRegistry>,
    session: SessionHandle,
    messages: Vec<Message>,
    prompt: PromptParts,
    /// `prompt` changed since the session last recorded it, so the next user message records it.
    prompt_unrecorded: bool,
    file_tracker: Arc<FileTracker>,
    cwd: PathBuf,
    /// A TUI is attached, so the permission gate can ask and titles generate in the background.
    interactive: bool,
    auto_compaction_failures: u8,
    last_usage: Option<TokenUsage>,
    displayed_usage: Option<TokenUsage>,
    total_estimated_cost_usd: f64,
}

impl Conversation {
    #[expect(
        clippy::too_many_arguments,
        reason = "each part comes from a separate startup step in `main`"
    )]
    pub(crate) async fn new(
        client: Client,
        tools: Arc<ToolRegistry>,
        session: SessionHandle,
        messages: Vec<Message>,
        context: Option<SessionContext>,
        file_tracker: Arc<FileTracker>,
        cwd: PathBuf,
        interactive: bool,
    ) -> Self {
        let prompt_unrecorded = context.is_none();
        let prompt = session_prompt(context, client.model(), &cwd).await;
        Self {
            client,
            tools,
            session,
            messages,
            prompt,
            prompt_unrecorded,
            file_tracker,
            cwd,
            interactive,
            auto_compaction_failures: 0,
            last_usage: None,
            displayed_usage: None,
            total_estimated_cost_usd: 0.0,
        }
    }

    /// Runs `text` through auto-compaction and one agent turn, reporting how it ended on `sink`.
    async fn submit_prompt<S>(
        &mut self,
        text: &str,
        sink: &S,
        user_rx: &mut mpsc::Receiver<UserAction>,
    ) -> TurnEnd
    where
        S: AgentSink + Clone + 'static,
    {
        let mut pending = Vec::new();
        if let Err(abort) = self.auto_compact(sink, user_rx, &mut pending).await {
            return report_abort(sink, abort, "auto-compact-failed");
        }

        let mut user_msg = Message::user(text);
        if let Some(reminder) = self.prompt.catch_up(UserContext::load(&self.cwd).await) {
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
        agent::record_drained_prompts(pending, &mut self.messages, &self.session, sink).await;

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

        let gate = agent::GateContext {
            policy: self.client.permission(),
            cwd: &self.cwd,
            interactive: self.interactive,
        };
        let TurnOutcome { report, result } = agent::agent_turn(
            &self.client,
            &self.tools,
            &mut self.messages,
            &self.prompt,
            sink,
            &self.session,
            user_rx,
            self.client.max_tool_rounds(),
            &gate,
        )
        .await;
        self.account_usage(sink, report);
        match result {
            Ok(()) => {
                sink.emit(AgentEvent::TurnComplete, "turn-complete");
                TurnEnd::Completed
            }
            Err(abort) => report_abort(sink, abort, "turn-failed"),
        }
    }

    async fn auto_compact(
        &mut self,
        sink: &dyn AgentSink,
        user_rx: &mut mpsc::Receiver<UserAction>,
        pending: &mut Vec<String>,
    ) -> Result<(), TurnAbort> {
        let compacted = agent::auto_compact_if_needed(
            &self.client,
            &self.session,
            &mut self.messages,
            sink,
            user_rx,
            pending,
            Some(&mut AutoCompact {
                config: self.client.compaction().auto,
                failures: &mut self.auto_compaction_failures,
                file_tracker: &self.file_tracker,
            }),
            self.last_usage,
        )
        .await?;
        if compacted {
            self.refresh_prompt().await;
            self.reset_usage();
        }
        Ok(())
    }

    /// Replaces the transcript with a summary steered by `instructions`. Failures leave it intact.
    async fn compact(
        &mut self,
        sink: &dyn AgentSink,
        user_rx: &mut mpsc::Receiver<UserAction>,
        instructions: Option<String>,
    ) -> TurnEnd {
        let mut pending = Vec::new();
        let summary = agent::await_unless_aborted(
            agent::compaction::compact_session(
                &self.client,
                &self.messages,
                instructions.as_deref(),
            ),
            user_rx,
            &mut pending,
        )
        .await;
        let summary = match summary {
            Ok(Ok(summary)) => summary,
            Ok(Err(e)) => {
                let abort = TurnAbort::Failed(anyhow!("Compaction failed: {e:#}"));
                return report_abort(sink, abort, "compact-failed");
            }
            Err(abort) => return report_abort(sink, abort, "compact-failed"),
        };
        let compacted = agent::compact_boundary::replace_session_with_summary(
            &self.session,
            &self.file_tracker,
            &mut self.messages,
            sink,
            summary,
            instructions,
            false,
        )
        .await;
        if !compacted {
            return TurnEnd::Failed;
        }
        self.refresh_prompt().await;
        self.reset_usage();
        TurnEnd::Completed
    }

    /// Starts the compacted transcript from the current instructions and date.
    async fn refresh_prompt(&mut self) {
        self.prompt.refresh(UserContext::load(&self.cwd).await);
        self.prompt_unrecorded = true;
    }

    /// Finalizes the live session and continues in a fresh, empty one.
    async fn clear(&mut self, sink: &dyn AgentSink, store: &SessionStore) {
        let outcome = handle::roll(
            &mut self.session,
            store,
            &self.file_tracker,
            self.client.model(),
        )
        .await;
        sink.session_write_error(outcome.finalize_failure.as_deref());
        self.adopt_session(Vec::new(), None).await;
        sink.emit(
            AgentEvent::SessionRolled { id: outcome.new_id },
            "session-rolled",
        );
    }

    /// Finalizes the live session and continues `target_id`. On failure the live session stays.
    async fn resume(&mut self, sink: &dyn AgentSink, store: &SessionStore, target_id: &str) {
        let outcome = match handle::roll_into(
            &mut self.session,
            store,
            &self.file_tracker,
            target_id,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                sink.emit(
                    AgentEvent::Error(format!(
                        "Resume failed (still on session {}): {e:#}",
                        self.session.session_id(),
                    )),
                    "resume-failed",
                );
                return;
            }
        };
        self.adopt_session(outcome.messages.clone(), outcome.context)
            .await;
        sink.emit(
            AgentEvent::SessionResumed {
                id: self.session.session_id().to_owned(),
                title: outcome.title,
                messages: outcome.messages,
                compact: outcome.compact,
                tool_metadata: outcome.tool_result_metadata,
            },
            "session-resumed",
        );
        // `SessionResumed` clears the chat, so anything about the old session must follow it.
        if let Some(failure) = outcome.finalize_failure {
            sink.emit(
                AgentEvent::Error(format!(
                    "Previous session failed to finalize cleanly: {failure}",
                )),
                "previous-session-finalize-failed",
            );
        }
        report_drift(sink, &outcome.drifted_paths);
    }

    async fn adopt_session(&mut self, messages: Vec<Message>, context: Option<SessionContext>) {
        self.client
            .set_session_id(self.session.session_id().to_owned());
        self.messages = messages;
        self.prompt_unrecorded = context.is_none();
        self.prompt = session_prompt(context, self.client.model(), &self.cwd).await;
        self.reset_usage();
    }

    async fn rename(&self, sink: &dyn AgentSink, title: String) {
        let outcome = self.session.set_manual_title(title.clone()).await;
        sink.session_write_error(outcome.failure.as_deref());
        if outcome.failure.is_some() {
            return;
        }
        sink.emit(
            AgentEvent::SessionTitleUpdated {
                session_id: self.session.session_id().to_owned(),
                title,
            },
            "session-title-updated",
        );
    }

    /// Applies a model and / or effort pick. The model goes first because it re-clamps effort.
    fn swap_config(
        &mut self,
        sink: &dyn AgentSink,
        model: Option<ResolvedModelId>,
        effort: Option<Effort>,
    ) {
        if let Some(id) = model {
            if let Err(e) = self.client.set_model(id.into_inner()) {
                sink.emit(
                    AgentEvent::Error(format!("Config change failed: {e:#}")),
                    "config-change-failed",
                );
                return;
            }
            self.prompt.set_model(self.client.model());
        }
        let resolved = match effort {
            Some(pick) => self.client.set_effort(pick),
            None => self.client.effort(),
        };
        sink.emit(
            AgentEvent::ConfigChanged {
                model_id: self.client.model().to_owned(),
                effort: resolved,
                compaction: self.client.compaction(),
                requested_effort: effort,
            },
            "config-changed",
        );
        self.auto_compaction_failures = 0;
        self.emit_usage_update(sink);
    }

    fn account_usage(&mut self, sink: &dyn AgentSink, report: TurnReport) {
        self.last_usage = report.usage;
        if let Some(usage) = report.usage {
            self.displayed_usage = Some(usage);
        }
        if let Some(cost) = report
            .billable_usage
            .and_then(|usage| estimate_usage_cost_usd(&self.client, usage))
        {
            self.total_estimated_cost_usd += cost;
        }
        if report.usage.is_some() || report.billable_usage.is_some() {
            self.emit_usage_update(sink);
        }
    }

    fn reset_usage(&mut self) {
        self.auto_compaction_failures = 0;
        self.last_usage = None;
        self.displayed_usage = None;
        self.total_estimated_cost_usd = 0.0;
    }

    fn emit_usage_update(&self, sink: &dyn AgentSink) {
        let Some(usage) = self.displayed_usage else {
            return;
        };
        sink.emit(
            AgentEvent::UsageUpdated(UsageSnapshot {
                context_tokens: usage.context_tokens(),
                context_window: model::context_window_for(self.client.model()),
                estimated_cost_usd: (self.total_estimated_cost_usd > 0.0)
                    .then_some(self.total_estimated_cost_usd),
            }),
            "usage-updated",
        );
    }

    /// Flushes and closes the live session, returning the write failure if any.
    async fn finalize(self) -> Option<String> {
        self.session
            .finalize(self.file_tracker.snapshot_all())
            .await
    }
}

/// Replays the prompt a session recorded, or builds a fresh one for a session without one.
async fn session_prompt(context: Option<SessionContext>, model: &str, cwd: &Path) -> PromptParts {
    match context {
        Some(context) => prompt::restore_prompt(context, model, cwd).await,
        None => prompt::build_prompt(model, cwd).await,
    }
}

fn estimate_usage_cost_usd(client: &Client, usage: TokenUsage) -> Option<f64> {
    model::token_cost_rates_for(client.model()).map(|rates| {
        rates.estimate_usd(
            usage.input_tokens(),
            usage.cache_creation_input_tokens(),
            usage.cache_read_input_tokens(),
            usage.output_tokens(),
            client.prompt_cache_ttl(),
        )
    })
}

// ── Reporting ──

/// How a prompt or compaction ended, after the sink has already shown it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnEnd {
    Completed,
    Cancelled,
    Failed,
    Quit,
}

/// Shows `abort` on `sink`, logging `label` if a failure event is dropped. `Quit` stays silent.
fn report_abort(sink: &dyn AgentSink, abort: TurnAbort, label: &'static str) -> TurnEnd {
    match abort {
        TurnAbort::Cancelled => {
            sink.emit(AgentEvent::Cancelled, "cancelled");
            TurnEnd::Cancelled
        }
        TurnAbort::Quit => TurnEnd::Quit,
        TurnAbort::Failed(e) => {
            sink.emit(AgentEvent::Error(format!("{e:#}")), label);
            TurnEnd::Failed
        }
    }
}

/// Warns that the tracked files in `drifted` changed on disk since the resumed session read them.
fn report_drift(sink: &dyn AgentSink, drifted: &[PathBuf]) {
    if drifted.is_empty() {
        return;
    }
    sink.emit(
        AgentEvent::Error(format_drift_warning(drifted)),
        "resume-drift-warning",
    );
}

fn format_drift_warning(drifted: &[PathBuf]) -> String {
    const PREVIEW_CAP: usize = 3;
    let preview: Vec<String> = drifted
        .iter()
        .take(PREVIEW_CAP)
        .map(|p| p.display().to_string())
        .collect();
    let suffix = if drifted.len() > preview.len() {
        format!(", and {} more", drifted.len() - preview.len())
    } else {
        String::new()
    };
    format!(
        "{} tracked file(s) changed on disk since the resumed session read them and need a fresh \
         Read before Edit: {}{suffix}",
        drifted.len(),
        preview.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use wiremock::MockServer;

    use super::testing::{
        MODEL, SESSION_CONTEXT, conversation, mount_reply, request_bodies, system_prompt,
    };
    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::client::anthropic::testing::{api_key, test_config};
    use crate::config::{AutoCompactionConfig, CompactionConfig};
    use crate::session::store::test_store;

    // ── exit_status ──

    #[test]
    fn exit_status_is_128_plus_the_signal_number() {
        #[cfg(unix)]
        assert_eq!(Shutdown::Hangup.exit_status(), 129);
        assert_eq!(Shutdown::Interrupt.exit_status(), 130);
        #[cfg(unix)]
        assert_eq!(Shutdown::Terminate.exit_status(), 143);
    }

    // ── submit_prompt ──

    #[tokio::test]
    async fn submit_prompt_keeps_the_prefix_and_sends_a_date_change_as_a_reminder() {
        let server = MockServer::start().await;
        mount_reply(&server, "reply").await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut conversation = conversation(server.uri(), &store).await;
        let today = conversation.prompt.seen.date.clone();
        let sink = CapturingSink::new();
        let (_user_tx, mut user_rx) = agent::event::inert_user_action_channel();

        let end = conversation
            .submit_prompt("first", &sink, &mut user_rx)
            .await;
        assert_eq!(end, TurnEnd::Completed);
        "2000-01-01".clone_into(&mut conversation.prompt.seen.date);
        let end = conversation
            .submit_prompt("second", &sink, &mut user_rx)
            .await;
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

        let id = conversation.session.session_id().to_owned();
        let expected = conversation.prompt.session_context();
        conversation.finalize().await;
        let data = store.load_session_data(&id).unwrap();
        assert_eq!(data.context, Some(expected));
        assert!(
            matches!(&data.messages[2].content[..], [_, ContentBlock::Text { text }] if text == reminder)
        );
    }

    #[tokio::test]
    async fn submit_prompt_reports_usage_and_accumulates_cost_across_turns() {
        let server = MockServer::start().await;
        mount_reply(&server, "reply").await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;
        let sink = CapturingSink::new();
        let (_user_tx, mut user_rx) = agent::event::inert_user_action_channel();

        for text in ["first", "second"] {
            conversation.submit_prompt(text, &sink, &mut user_rx).await;
        }

        let usage = sink
            .events()
            .into_iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::UsageUpdated(usage) => Some(usage),
                _ => None,
            });
        let usage = usage.expect("each turn reports usage");
        assert_eq!(usage.context_tokens, 1_000);
        assert_eq!(usage.context_window, Some(1_000_000));
        let cost = usage.estimated_cost_usd.unwrap();
        assert!(
            (cost - 0.015).abs() < 1e-12,
            "two turns at $0.0075 each: {cost}"
        );
    }

    #[tokio::test]
    async fn submit_prompt_auto_compacts_before_recording_the_new_prompt() {
        let server = MockServer::start().await;
        mount_reply(&server, "auto summary").await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;
        let mut config = test_config(server.uri(), api_key(), MODEL);
        config.compaction = CompactionConfig::resolved_for_test(AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(50_000),
        });
        conversation.client = Client::new(config, Some("sid".to_owned())).unwrap();
        conversation.messages = vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
            Message::assistant("four"),
        ];
        conversation.last_usage = Some(TokenUsage::new(50_000, 1));
        conversation.displayed_usage = conversation.last_usage;
        conversation.total_estimated_cost_usd = 1.23;
        let sink = CapturingSink::new();
        let (_user_tx, mut user_rx) = agent::event::inert_user_action_channel();

        let end = conversation
            .submit_prompt("five", &sink, &mut user_rx)
            .await;

        assert_eq!(end, TurnEnd::Completed);
        let requests = request_bodies(&server).await;
        assert_eq!(requests.len(), 2);
        let turn = requests[1]["messages"].as_array().unwrap();
        assert!(
            turn[1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("auto summary"),
            "the turn must start from the summary: {turn:?}",
        );
        assert_eq!(turn.last().unwrap()["content"][0]["text"], "five");
        assert!(
            matches!(&conversation.messages[0].content[0], ContentBlock::Text { text } if text.contains("auto summary"))
        );
        assert_eq!(conversation.messages.len(), 3);
        assert_eq!(conversation.last_usage, Some(TokenUsage::new(1_000, 100)));
        assert!(
            (conversation.total_estimated_cost_usd - 0.0075).abs() < 1e-12,
            "compaction must reset the session cost before the turn bills: {}",
            conversation.total_estimated_cost_usd,
        );
        let events = sink.events();
        let compacted = events.iter().position(|event| {
            matches!(
                event,
                AgentEvent::SessionCompacted {
                    automatic: true,
                    ..
                }
            )
        });
        let completed = events
            .iter()
            .position(|event| matches!(event, AgentEvent::TurnComplete));
        assert!(compacted.unwrap() < completed.unwrap());
    }

    // ── compact ──

    fn transcript() -> Vec<Message> {
        vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
            Message::assistant("four"),
        ]
    }

    #[tokio::test]
    async fn compact_replaces_the_transcript_and_resets_usage() {
        let server = MockServer::start().await;
        mount_reply(&server, "manual summary").await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;
        conversation.messages = transcript();
        conversation.displayed_usage = Some(TokenUsage::new(90_000, 9));
        conversation.total_estimated_cost_usd = 1.23;
        let sink = CapturingSink::new();
        let (_user_tx, mut user_rx) = agent::event::inert_user_action_channel();

        let end = conversation
            .compact(&sink, &mut user_rx, Some("focus".to_owned()))
            .await;

        assert_eq!(end, TurnEnd::Completed);
        assert!(matches!(
            conversation.messages.as_slice(),
            [Message { content, .. }]
                if matches!(&content[0], ContentBlock::Text { text } if text.contains("manual summary"))
        ));
        assert_eq!(conversation.displayed_usage, None);
        assert!(conversation.total_estimated_cost_usd.abs() < f64::EPSILON);
        assert_ne!(
            conversation.prompt.user_context, SESSION_CONTEXT,
            "the compacted transcript starts from a fresh user context"
        );
        assert!(conversation.prompt_unrecorded);
        assert!(matches!(
            sink.events().as_slice(),
            [AgentEvent::SessionCompacted { automatic: false, instructions: Some(focus), .. }]
                if focus == "focus"
        ));
    }

    #[tokio::test]
    async fn compact_failure_or_cancel_keeps_the_transcript() {
        let server = MockServer::start().await;
        mount_reply(&server, "summary").await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;
        let sink = CapturingSink::new();
        let (user_tx, mut user_rx) = mpsc::channel(1);

        conversation.messages = vec![Message::user("too short")];
        let end = conversation.compact(&sink, &mut user_rx, None).await;
        assert_eq!(end, TurnEnd::Failed);
        assert_eq!(conversation.messages.len(), 1);

        conversation.messages = transcript();
        user_tx.send(UserAction::Cancel).await.unwrap();
        let end = conversation.compact(&sink, &mut user_rx, None).await;
        assert_eq!(end, TurnEnd::Cancelled);
        assert_eq!(conversation.messages.len(), 4);

        conversation.session = handle::testing::dead("dead");
        let end = conversation.compact(&sink, &mut user_rx, None).await;
        assert_eq!(
            end,
            TurnEnd::Failed,
            "an unwritten boundary is a failed compaction"
        );
        assert_eq!(conversation.messages.len(), 4);

        let events = sink.events();
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::Error(short), AgentEvent::Cancelled, AgentEvent::Error(write)]
                    if short.starts_with("Compaction failed: session is too short")
                        && write.starts_with("Session write failed")
            ),
            "{events:?}",
        );
    }

    // ── clear ──

    #[tokio::test]
    async fn clear_and_resume_rebuild_session_prompt() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut conversation = conversation(server.uri(), &store).await;
        let sink = CapturingSink::new();
        let target = handle::start(&store, MODEL);
        let target_id = target.session_id().to_owned();
        target.record_message(Message::user("earlier prompt")).await;
        target
            .record_message(Message::assistant("earlier reply"))
            .await;
        target.finish(Vec::new()).await;
        drop(target);
        conversation.messages = vec![Message::user("before clear")];

        conversation.clear(&sink, &store).await;
        assert!(conversation.messages.is_empty());
        assert_ne!(conversation.prompt.user_context, SESSION_CONTEXT);
        assert!(conversation.prompt.user_context.contains("# Current date"));
        assert_eq!(
            conversation.client.session_id(),
            conversation.session.session_id()
        );

        SESSION_CONTEXT.clone_into(&mut conversation.prompt.user_context);
        conversation.resume(&sink, &store, &target_id).await;
        assert_eq!(conversation.session.session_id(), target_id);
        assert_eq!(conversation.client.session_id(), target_id);
        assert_eq!(conversation.messages.len(), 2);
        assert_ne!(conversation.prompt.user_context, SESSION_CONTEXT);
    }

    // ── resume ──

    #[tokio::test]
    async fn resume_replays_the_recorded_prompt() {
        let server = MockServer::start().await;
        mount_reply(&server, "reply").await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut conversation = conversation(server.uri(), &store).await;
        let recorded_id = conversation.session.session_id().to_owned();
        let sink = CapturingSink::new();
        let (_user_tx, mut user_rx) = agent::event::inert_user_action_channel();
        conversation
            .submit_prompt("first", &sink, &mut user_rx)
            .await;
        conversation.clear(&sink, &store).await;

        conversation.resume(&sink, &store, &recorded_id).await;
        assert_eq!(conversation.prompt.user_context, SESSION_CONTEXT);
        assert!(!conversation.prompt_unrecorded);
        conversation
            .submit_prompt("second", &sink, &mut user_rx)
            .await;

        let turns = request_bodies(&server).await;
        assert_eq!(system_prompt(&turns[1]), system_prompt(&turns[0]));
        assert_eq!(turns[1]["messages"][0], turns[0]["messages"][0]);
        assert_eq!(turns[1]["messages"][1]["content"][0]["text"], "first");
    }

    #[tokio::test]
    async fn resume_failure_preserves_current_session_usage_state() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let mut conversation = conversation(server.uri(), &store).await;
        let original_id = conversation.session.session_id().to_owned();
        let last_usage = TokenUsage::new(100_000, 10);
        let displayed_usage = TokenUsage::new(90_000, 9);
        conversation.messages = vec![Message::user("still here")];
        conversation.auto_compaction_failures = 2;
        conversation.last_usage = Some(last_usage);
        conversation.displayed_usage = Some(displayed_usage);
        conversation.total_estimated_cost_usd = 1.23;
        let sink = CapturingSink::new();

        conversation
            .resume(&sink, &store, "missing-target-id")
            .await;

        assert_eq!(conversation.session.session_id(), original_id);
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.auto_compaction_failures, 2);
        assert_eq!(conversation.last_usage, Some(last_usage));
        assert_eq!(conversation.displayed_usage, Some(displayed_usage));
        assert!((conversation.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON);
        assert!(matches!(
            sink.events().as_slice(),
            [AgentEvent::Error(msg)] if msg.contains("still on session")
        ));
    }

    // ── rename ──

    #[tokio::test]
    async fn rename_announces_only_a_persisted_title() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;
        let live_id = conversation.session.session_id().to_owned();
        let sink = CapturingSink::new();

        conversation.rename(&sink, "Fix login".to_owned()).await;
        assert!(conversation.session.manual_title_set());
        conversation.session = handle::testing::dead("dead");
        conversation.rename(&sink, "Lost".to_owned()).await;

        let events = sink.events();
        assert!(
            matches!(
                events.as_slice(),
                [
                    AgentEvent::SessionTitleUpdated { session_id, title },
                    AgentEvent::Error(write),
                ] if *session_id == live_id && title == "Fix login"
                    && write.starts_with("Session write failed")
            ),
            "{events:?}",
        );
    }

    // ── swap_config ──

    #[tokio::test]
    async fn swap_config_model_rerenders_system_and_keeps_user_context() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;

        conversation.swap_config(
            &CapturingSink::new(),
            Some(ResolvedModelId::new("claude-sonnet-4-6".to_owned())),
            None,
        );

        assert!(
            conversation
                .prompt
                .system_sections
                .iter()
                .any(|section| section.contains("The exact model ID is claude-sonnet-4-6.")),
        );
        assert_eq!(conversation.prompt.user_context, SESSION_CONTEXT);
    }

    #[tokio::test]
    async fn swap_config_resets_auto_compaction_breaker() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut conversation = conversation(server.uri(), &test_store(dir.path())).await;
        conversation.auto_compaction_failures = 3;
        conversation.last_usage = Some(TokenUsage::new(100_000, 1));
        conversation.total_estimated_cost_usd = 1.23;
        let sink = CapturingSink::new();

        conversation.swap_config(&sink, None, Some(Effort::Xhigh));

        assert_eq!(conversation.auto_compaction_failures, 0);
        assert_eq!(conversation.last_usage, Some(TokenUsage::new(100_000, 1)));
        assert!(
            (conversation.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON,
            "session cost must accumulate across swaps: {}",
            conversation.total_estimated_cost_usd,
        );
        assert!(matches!(
            sink.events().first(),
            Some(AgentEvent::ConfigChanged { .. })
        ));
    }

    // ── report_abort ──

    #[test]
    fn report_abort_shows_cancel_and_failure_but_keeps_quit_silent() {
        let sink = CapturingSink::new();

        assert_eq!(
            report_abort(&sink, TurnAbort::Cancelled, "label"),
            TurnEnd::Cancelled
        );
        assert_eq!(report_abort(&sink, TurnAbort::Quit, "label"), TurnEnd::Quit);
        assert_eq!(
            report_abort(&sink, TurnAbort::Failed(anyhow!("boom")), "label"),
            TurnEnd::Failed
        );

        let events = sink.events();
        assert!(
            matches!(events.as_slice(), [AgentEvent::Cancelled, AgentEvent::Error(msg)] if msg == "boom"),
            "{events:?}",
        );
    }

    // ── format_drift_warning ──

    #[test]
    fn format_drift_warning_previews_three_paths_and_counts_the_rest() {
        let drifted: Vec<PathBuf> = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]
            .into_iter()
            .map(PathBuf::from)
            .collect();

        let warning = format_drift_warning(&drifted);

        assert!(warning.starts_with("5 tracked file(s) changed on disk"));
        assert!(
            warning.ends_with(": a.rs, b.rs, c.rs, and 2 more"),
            "{warning}"
        );
    }
}
