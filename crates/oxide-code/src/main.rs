//! CLI entry point: config loading, session resolution, mode dispatch.

mod agent;
mod client;
mod config;
mod file_tracker;
mod message;
mod model;
mod prompt;
mod session;
mod slash;
mod tool;
mod tui;
mod util;

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use clap::{ArgGroup, Parser};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use agent::event::{
    AgentEvent, AgentSink, StdioSink, UsageSnapshot, UserAction, inert_user_action_channel,
};
use agent::{AutoCompact, TokenUsage, TurnAbort, TurnOutcome, agent_turn};
use client::anthropic::Client;
use config::{Config, Effort};
use file_tracker::FileTracker;
use message::Message;
use model::ResolvedModelId;
use session::handle::{ResumedSession, SessionHandle, roll as roll_session};
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::{DEFAULT_SESSION_LIST_LIMIT, SessionStore};
use slash::LiveSessionInfo;
use tool::{
    ToolRegistry, bash::BashTool, edit::EditTool, glob::GlobTool, grep::GrepTool, read::ReadTool,
    write::WriteTool,
};
use util::path::tildify;

#[derive(Parser)]
#[command(name = "ox", version, about = "A terminal-based AI coding assistant")]
#[command(group(
    ArgGroup::new("scope")
        .args(["list", "continue"])
        .multiple(true),
))]
struct Cli {
    /// Widen `--list` / `--continue` from the current cwd to every project.
    #[arg(short, long, requires = "scope")]
    all: bool,

    /// Resume a session: bare flag picks the most recent, an ID prefix picks a specific one.
    #[expect(
        clippy::option_option,
        reason = "encodes three CLI states: absent (None), flag only (Some(None)), flag with value (Some(Some))"
    )]
    #[arg(
        short = 'c',
        long = "continue",
        value_name = "SESSION_ID",
        conflicts_with = "prompt"
    )]
    r#continue: Option<Option<String>>,

    /// Cap `--list` to the N most-recent sessions. `0` disables the cap.
    #[arg(long, value_name = "N", requires = "list", default_value_t = DEFAULT_SESSION_LIST_LIMIT)]
    limit: usize,

    /// List recent sessions and exit.
    #[arg(short, long, conflicts_with_all = ["prompt", "continue"])]
    list: bool,

    /// Disable the TUI and use a bare REPL instead.
    #[arg(long)]
    no_tui: bool,

    /// Run in headless mode: send a single prompt and print the response.
    #[arg(short, long, value_name = "PROMPT")]
    prompt: Option<String>,
}

fn main() -> Result<()> {
    util::time::init_local_offset();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();

    let tui_mode =
        !cli.no_tui && cli.prompt.is_none() && !cli.list && std::io::stdout().is_terminal();
    let _log_guard = util::log::init_tracing(tui_mode)?;

    if cli.list {
        return list_sessions(cli.all, cli.limit);
    }

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let model = config.model.clone();
    let theme = config.theme.clone();
    let snapshot = config.snapshot();

    let store = SessionStore::open()?;
    let file_tracker = Arc::new(FileTracker::default());
    let mut resumed = resolve_session(&store, &model, cli.r#continue.as_ref(), cli.all).await?;
    let drifted = file_tracker.restore_verified(std::mem::take(&mut resumed.file_snapshots));
    if !drifted.is_empty() {
        warn!(
            "{} tracked file(s) drifted on disk since this session ran; re-Read needed before Edit",
            drifted.len(),
        );
    }

    let client = Client::new(config, Some(resumed.handle.session_id().to_owned()))?;

    let tools = Arc::new(create_tool_registry(&file_tracker));

    if let Some(prompt_text) = cli.prompt {
        return headless(
            &client,
            tools,
            &model,
            show_thinking,
            &prompt_text,
            resumed.handle,
            file_tracker,
        )
        .await;
    }

    if cli.no_tui || !std::io::stdout().is_terminal() {
        return bare_repl(
            &client,
            tools,
            &model,
            show_thinking,
            resumed.handle,
            resumed.messages,
            file_tracker,
        )
        .await;
    }

    run_tui(
        &client,
        show_thinking,
        snapshot,
        &theme,
        tools,
        resumed,
        file_tracker,
        store,
    )
    .await
}

// ── Session Helpers ──

fn list_sessions(all: bool, limit: usize) -> Result<()> {
    let store = SessionStore::open()?;
    let local_offset = util::time::local_offset();
    let term_width = detect_terminal_width();
    let cap = (limit > 0).then_some(limit);
    render_list(
        &mut std::io::stdout().lock(),
        &store,
        all,
        local_offset,
        term_width,
        cap,
    )
}

fn detect_terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(cols, _)| usize::from(cols))
}

fn create_tool_registry(tracker: &Arc<FileTracker>) -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(BashTool),
        Box::new(ReadTool::new(Arc::clone(tracker))),
        Box::new(WriteTool::new(Arc::clone(tracker))),
        Box::new(EditTool::new(Arc::clone(tracker))),
        Box::new(GlobTool),
        Box::new(GrepTool),
    ])
}

/// Resolves on the first SIGINT / SIGTERM / SIGHUP.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let ctrl_c = tokio::signal::ctrl_c();
        let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
            _ = ctrl_c.await;
            return;
        };
        let Ok(mut sighup) = signal(SignalKind::hangup()) else {
            _ = ctrl_c.await;
            return;
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
            _ = sighup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        _ = tokio::signal::ctrl_c().await;
    }
}

// ── TUI Mode ──

#[expect(
    clippy::too_many_arguments,
    reason = "wires the full TUI surface (client, display config, resumed state, tool registry, file tracker, store); a builder would obscure which dependencies run_tui owns vs. borrows"
)]
async fn run_tui(
    client: &Client,
    show_thinking: bool,
    config: config::ConfigSnapshot,
    theme: &tui::theme::Theme,
    tools: Arc<ToolRegistry>,
    resumed: ResumedSession,
    file_tracker: Arc<FileTracker>,
    store: SessionStore,
) -> Result<()> {
    let ResumedSession {
        handle: session,
        messages: resumed_messages,
        compact: resumed_compact,
        title: resumed_title,
        tool_result_metadata: resumed_tool_metadata,
        file_snapshots: _,
    } = resumed;
    tui::terminal::install_panic_hook();

    let (agent_sink, agent_rx) = tui::event::channel();
    let (user_tx, user_rx) = mpsc::channel::<UserAction>(32);

    let cwd_path = std::env::current_dir().ok();
    let cwd = cwd_path.as_deref().map(tildify).unwrap_or_default();
    let git_branch = cwd_path.as_deref().and_then(util::git::current_branch);

    let session_info = LiveSessionInfo {
        cwd,
        git_cwd: cwd_path,
        git_branch,
        version: env!("CARGO_PKG_VERSION"),
        session_id: session.session_id().to_owned(),
        config,
    };

    let mut terminal = tui::terminal::init()?;
    let mut app = tui::app::App::new(
        theme,
        session_info,
        show_thinking,
        agent_rx,
        user_tx,
        Arc::clone(&tools),
        tui::app::AppHistory {
            messages: &resumed_messages,
            compact: resumed_compact.as_ref(),
            tool_metadata: &resumed_tool_metadata,
            title: resumed_title,
        },
    );

    let agent_handle = {
        let client = client.clone();
        let session = session.clone();
        let store = store.clone();
        let file_tracker = Arc::clone(&file_tracker);
        tokio::spawn(async move {
            agent_loop_task(
                client,
                tools,
                agent_sink,
                user_rx,
                session,
                resumed_messages,
                store,
                file_tracker,
            )
            .await
        })
    };

    let result = tokio::select! {
        result = app.run(&mut terminal) => result,
        () = shutdown_signal() => {
            debug!("TUI received shutdown signal, tearing down");
            Ok(())
        }
    };

    tui::terminal::restore();

    agent_handle.abort();
    match agent_handle.await {
        Ok(Err(e)) => warn!("agent loop error: {e}"),
        Err(e) if !e.is_cancelled() => warn!("agent task panicked: {e}"),
        _ => {}
    }

    if let Some(msg) = session.finalize(file_tracker.snapshot_all()).await {
        warn!("session finish failed: {msg}");
    }

    result
}

/// Each `TurnAbort` arm emits exactly one terminal event (`Error` xor `TurnComplete`).
#[expect(
    clippy::too_many_arguments,
    reason = "the task entry point receives the spawned loop dependencies before AgentLoopTask owns them"
)]
async fn agent_loop_task(
    client: Client,
    tools: Arc<ToolRegistry>,
    sink: tui::event::ChannelSink,
    user_rx: mpsc::Receiver<UserAction>,
    session: SessionHandle,
    resumed_messages: Vec<Message>,
    store: SessionStore,
    file_tracker: Arc<FileTracker>,
) -> Result<()> {
    AgentLoopTask {
        client,
        tools,
        sink,
        user_rx,
        session,
        messages: resumed_messages,
        store,
        file_tracker,
        auto_compaction_failures: 0,
        last_usage: None,
        displayed_usage: None,
        total_estimated_cost_usd: 0.0,
    }
    .run()
    .await
}

struct AgentLoopTask {
    client: Client,
    tools: Arc<ToolRegistry>,
    sink: tui::event::ChannelSink,
    user_rx: mpsc::Receiver<UserAction>,
    session: SessionHandle,
    messages: Vec<Message>,
    store: SessionStore,
    file_tracker: Arc<FileTracker>,
    auto_compaction_failures: u8,
    last_usage: Option<TokenUsage>,
    displayed_usage: Option<TokenUsage>,
    total_estimated_cost_usd: f64,
}

enum LoopControl {
    Continue,
    Stop,
}

impl AgentLoopTask {
    async fn run(&mut self) -> Result<()> {
        while let Some(action) = self.user_rx.recv().await {
            if matches!(self.handle_action(action).await, LoopControl::Stop) {
                break;
            }
        }

        Ok(())
    }

    async fn handle_action(&mut self, action: UserAction) -> LoopControl {
        match action {
            UserAction::SubmitPrompt(text) => self.handle_submit_prompt(text).await,
            // Cancel / ConfirmExit are no-ops here; PreviewTheme / SwapTheme are TUI-only and
            // applied client-side in `App::apply_action_locally`.
            UserAction::Cancel
            | UserAction::ConfirmExit
            | UserAction::PreviewTheme { .. }
            | UserAction::SwapTheme { .. } => LoopControl::Continue,
            UserAction::Clear => {
                let outcome = roll_session(
                    &mut self.session,
                    &self.store,
                    &self.file_tracker,
                    self.client.model(),
                )
                .await;
                self.sink
                    .session_write_error(outcome.finalize_failure.as_deref());
                self.client.set_session_id(outcome.new_id.clone());
                self.messages.clear();
                self.reset_auto_compaction();
                self.reset_usage_display();
                // /clear succeeded server-side, so dropping `SessionRolled` would strand the TUI
                // on a stuck "old session" header.
                self.sink.emit(
                    AgentEvent::SessionRolled { id: outcome.new_id },
                    "session-rolled",
                );
                LoopControl::Continue
            }
            UserAction::Resume { session_id } => {
                let resumed = apply_resume(
                    &mut self.session,
                    &mut self.client,
                    &mut self.messages,
                    &self.store,
                    &self.file_tracker,
                    &self.sink,
                    &session_id,
                )
                .await;
                if resumed {
                    self.reset_auto_compaction();
                    self.reset_usage_display();
                }
                LoopControl::Continue
            }
            UserAction::Compact { instructions } => {
                let outcome = apply_compact(
                    &self.client,
                    &self.session,
                    &self.file_tracker,
                    &mut self.messages,
                    &self.sink,
                    &mut self.user_rx,
                    instructions,
                )
                .await;
                match outcome {
                    Ok(true) => {
                        self.reset_auto_compaction();
                        self.reset_usage_display();
                        LoopControl::Continue
                    }
                    Ok(false) => LoopControl::Continue,
                    Err(TurnAbort::Cancelled) => {
                        self.sink.emit(AgentEvent::Cancelled, "cancelled");
                        LoopControl::Continue
                    }
                    Err(TurnAbort::Quit) => LoopControl::Stop,
                    Err(TurnAbort::Failed(e)) => {
                        self.sink
                            .emit(AgentEvent::Error(format!("{e:#}")), "turn-failed");
                        LoopControl::Continue
                    }
                }
            }
            UserAction::Rename { title } => {
                apply_rename(&self.session, &self.sink, title).await;
                LoopControl::Continue
            }
            UserAction::SwapConfig { model, effort } => {
                if apply_swap_config(&mut self.client, &self.sink, model, effort) {
                    self.auto_compaction_failures = 0;
                    self.emit_usage_update();
                }
                LoopControl::Continue
            }
            UserAction::Quit => LoopControl::Stop,
        }
    }

    async fn handle_submit_prompt(&mut self, text: String) -> LoopControl {
        let mut pre_prompt_pending = Vec::new();
        let pre_prompt_compact = auto_compact_before_prompt(
            &self.client,
            &self.session,
            &self.file_tracker,
            &mut self.messages,
            &self.sink,
            &mut self.user_rx,
            &mut pre_prompt_pending,
            &mut self.auto_compaction_failures,
            self.last_usage,
        )
        .await;
        match pre_prompt_compact {
            Ok(true) => {
                self.last_usage = None;
                self.reset_usage_display();
            }
            Ok(false) => {}
            Err(TurnAbort::Cancelled) => {
                self.sink.emit(AgentEvent::Cancelled, "cancelled");
                return LoopControl::Continue;
            }
            Err(TurnAbort::Quit) => return LoopControl::Stop,
            Err(TurnAbort::Failed(e)) => {
                self.sink
                    .emit(AgentEvent::Error(format!("{e:#}")), "auto-compact-failed");
                return LoopControl::Continue;
            }
        }

        let user_msg = Message::user(&text);
        let outcome = self.session.record_message(user_msg.clone()).await;
        self.sink.session_write_error(outcome.failure.as_deref());
        self.messages.push(user_msg);
        agent::record_drained_prompts(
            pre_prompt_pending.drain(..),
            &mut self.messages,
            &self.session,
            &self.sink,
        )
        .await;

        if let Some(seed) = outcome.ai_title_seed {
            session::title_generator::spawn(
                self.client.clone(),
                self.session.clone(),
                self.sink.clone(),
                seed,
            );
        }

        let prompt = prompt::build_prompt(self.client.model()).await;
        let outcome = agent_turn(
            &self.client,
            &self.tools,
            &mut self.messages,
            &prompt,
            &self.sink,
            &self.session,
            &mut self.user_rx,
            self.client.max_tool_rounds(),
        )
        .await;
        let TurnOutcome { report, result } = outcome;
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
            self.emit_usage_update();
        }
        match result {
            Ok(()) => {
                self.sink.emit(AgentEvent::TurnComplete, "turn-complete");
                LoopControl::Continue
            }
            Err(TurnAbort::Cancelled) => {
                self.sink.emit(AgentEvent::Cancelled, "cancelled");
                LoopControl::Continue
            }
            Err(TurnAbort::Quit) => LoopControl::Stop,
            Err(TurnAbort::Failed(e)) => {
                self.sink
                    .emit(AgentEvent::Error(format!("{e:#}")), "turn-failed");
                LoopControl::Continue
            }
        }
    }

    fn reset_auto_compaction(&mut self) {
        self.auto_compaction_failures = 0;
        self.last_usage = None;
    }

    fn reset_usage_display(&mut self) {
        self.displayed_usage = None;
        self.total_estimated_cost_usd = 0.0;
    }

    fn emit_usage_update(&self) {
        let Some(usage) = self.displayed_usage else {
            return;
        };
        self.sink.emit(
            AgentEvent::UsageUpdated(UsageSnapshot {
                context_tokens: usage.context_tokens(),
                context_window: crate::model::context_window_for(self.client.model()),
                estimated_cost_usd: (self.total_estimated_cost_usd > 0.0)
                    .then_some(self.total_estimated_cost_usd),
            }),
            "usage-updated",
        );
    }
}

fn estimate_usage_cost_usd(client: &Client, usage: TokenUsage) -> Option<f64> {
    crate::model::token_cost_rates_for(client.model()).map(|rates| {
        rates.estimate_usd(
            usage.input_tokens(),
            usage.cache_creation_input_tokens(),
            usage.cache_read_input_tokens(),
            usage.output_tokens(),
            client.prompt_cache_ttl(),
        )
    })
}

/// Drives the mid-session resume: swap the handle, repaint the chat, surface previous-session
/// finalize failures and tracker drift as distinct events so the user sees their source.
async fn apply_resume(
    session: &mut SessionHandle,
    client: &mut Client,
    messages: &mut Vec<Message>,
    store: &SessionStore,
    file_tracker: &FileTracker,
    sink: &dyn AgentSink,
    target_id: &str,
) -> bool {
    let outcome = match session::handle::roll_into(session, store, file_tracker, target_id).await {
        Ok(o) => o,
        Err(e) => {
            sink.emit(
                AgentEvent::Error(format!(
                    "Resume failed (still on session {}): {e:#}",
                    session.session_id(),
                )),
                "resume-failed",
            );
            return false;
        }
    };
    let new_id = session.session_id().to_owned();
    client.set_session_id(new_id.clone());
    messages.clone_from(&outcome.messages);
    sink.emit(
        AgentEvent::SessionResumed {
            id: new_id,
            title: outcome.title,
            messages: outcome.messages,
            compact: outcome.compact,
            tool_metadata: outcome.tool_result_metadata,
        },
        "session-resumed",
    );
    // Emit OLD-session finalize failure AFTER SessionResumed so the chat-clear doesn't wipe it.
    // Distinct phrasing (not session_write_error) so the user doesn't read it as a current-writer
    // fault.
    if let Some(failure) = outcome.finalize_failure.as_deref() {
        sink.emit(
            AgentEvent::Error(format!(
                "Previous session failed to finalize cleanly: {failure}",
            )),
            "previous-session-finalize-failed",
        );
    }
    if !outcome.drifted_paths.is_empty() {
        sink.emit(
            AgentEvent::Error(format_drift_warning(&outcome.drifted_paths)),
            "resume-drift-warning",
        );
    }
    true
}

fn format_drift_warning(drifted: &[std::path::PathBuf]) -> String {
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
        "{} tracked file(s) drifted on disk since the resumed session — re-Read before Edit: {}{suffix}",
        drifted.len(),
        preview.join(", "),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "pre-prompt auto-compaction needs the live session state and the shared failure counter"
)]
async fn auto_compact_before_prompt(
    client: &Client,
    session: &SessionHandle,
    file_tracker: &FileTracker,
    messages: &mut Vec<Message>,
    sink: &dyn AgentSink,
    user_rx: &mut mpsc::Receiver<UserAction>,
    pending: &mut Vec<String>,
    failures: &mut u8,
    usage: Option<TokenUsage>,
) -> std::result::Result<bool, TurnAbort> {
    agent::auto_compact_if_needed(
        client,
        session,
        messages,
        sink,
        user_rx,
        pending,
        Some(&mut AutoCompact {
            config: client.compaction().auto,
            failures,
            file_tracker,
        }),
        usage,
    )
    .await
}

/// Drives `/compact`: stream the summarization, replace the in-memory transcript with the
/// synthetic continuation, persist the boundary + synthetic message, surface the post-compact
/// system event so the TUI can repaint. Errors leave the session untouched.
async fn apply_compact(
    client: &Client,
    session: &SessionHandle,
    file_tracker: &FileTracker,
    messages: &mut Vec<Message>,
    sink: &dyn AgentSink,
    user_rx: &mut mpsc::Receiver<UserAction>,
    instructions: Option<String>,
) -> std::result::Result<bool, TurnAbort> {
    let mut pending_prompts = Vec::new();
    let summary = agent::await_unless_aborted(
        agent::compaction::compact_session(client, messages, instructions.as_deref()),
        user_rx,
        &mut pending_prompts,
    )
    .await?
    .map_err(|e| TurnAbort::Failed(anyhow!("Compaction failed: {e:#}")))?;
    Ok(agent::compact_boundary::replace_session_with_summary(
        session,
        file_tracker,
        messages,
        sink,
        summary,
        instructions,
        false,
    )
    .await)
}

async fn apply_rename(session: &SessionHandle, sink: &dyn AgentSink, title: String) {
    let outcome = session.set_manual_title(title.clone()).await;
    sink.session_write_error(outcome.failure.as_deref());
    if outcome.failure.is_some() {
        return;
    }
    sink.emit(
        AgentEvent::SessionTitleUpdated {
            session_id: session.session_id().to_owned(),
            title,
        },
        "session-title-updated",
    );
}

/// Order matters: model swap re-clamps effort before the explicit pick is applied.
fn apply_swap_config(
    client: &mut Client,
    sink: &dyn AgentSink,
    model: Option<ResolvedModelId>,
    effort: Option<Effort>,
) -> bool {
    if let Some(id) = model
        && let Err(e) = client.set_model(id.into_inner())
    {
        sink.emit(
            AgentEvent::Error(format!("Config change failed: {e:#}")),
            "config-change-failed",
        );
        return false;
    }
    let resolved = match effort {
        Some(pick) => client.set_effort(pick),
        None => client.effort(),
    };
    // Dropping this leaves the status bar showing the previous model / effort even though the
    // client has already swapped.
    sink.emit(
        AgentEvent::ConfigChanged {
            model_id: client.model().to_owned(),
            effort: resolved,
            compaction: client.compaction(),
            requested_effort: effort,
        },
        "config-changed",
    );
    true
}

// ── Bare REPL Mode ──

async fn bare_repl(
    client: &Client,
    tools: Arc<ToolRegistry>,
    model: &str,
    show_thinking: bool,
    session: SessionHandle,
    resumed_messages: Vec<Message>,
    file_tracker: Arc<FileTracker>,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking, Arc::clone(&tools));
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut messages: Vec<Message> = resumed_messages;
    let mut shutdown_fired = false;
    let (_user_tx, mut user_rx) = inert_user_action_channel();
    let mut auto_compaction_failures = 0_u8;
    let mut last_usage = None;

    let result: Result<()> = async {
        loop {
            eprint!("> ");
            std::io::stderr().flush()?;

            let line = tokio::select! {
                line = lines.next_line() => line?,
                () = shutdown_signal() => {
                    eprintln!();
                    shutdown_fired = true;
                    None
                }
            };
            let Some(line) = line else {
                break; // EOF or signal
            };

            let input = line.trim().to_owned();
            if input.is_empty() {
                continue;
            }

            let mut pre_prompt_pending = Vec::new();
            match auto_compact_before_prompt(
                client,
                &session,
                &file_tracker,
                &mut messages,
                &sink,
                &mut user_rx,
                &mut pre_prompt_pending,
                &mut auto_compaction_failures,
                last_usage,
            )
            .await
            {
                Ok(true) => last_usage = None,
                Ok(false) => {}
                Err(TurnAbort::Cancelled | TurnAbort::Quit) => continue,
                Err(TurnAbort::Failed(e)) => return Err(e),
            }

            let user_msg = Message::user(&input);
            let outcome = session.record_message(user_msg.clone()).await;
            sink.session_write_error(outcome.failure.as_deref());
            messages.push(user_msg);
            agent::record_drained_prompts(
                pre_prompt_pending.drain(..),
                &mut messages,
                &session,
                &sink,
            )
            .await;
            let prompt = prompt::build_prompt(model).await;
            let turn = agent_turn(
                client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut user_rx,
                client.max_tool_rounds(),
            );
            let TurnOutcome { report, result } = tokio::select! {
                outcome = turn => outcome,
                () = shutdown_signal() => {
                    eprintln!();
                    shutdown_fired = true;
                    break;
                }
            };
            last_usage = report.usage;
            match result {
                Ok(()) | Err(TurnAbort::Cancelled | TurnAbort::Quit) => {}
                Err(TurnAbort::Failed(e)) => return Err(e),
            }
            sink.emit(AgentEvent::TurnComplete, "turn-complete");
        }
        Ok(())
    }
    .await;

    let failure = session.finalize(file_tracker.snapshot_all()).await;
    sink.session_write_error(failure.as_deref());

    // tokio::io::stdin's blocking thread hangs runtime Drop on signal; force-exit instead.
    if shutdown_fired {
        std::process::exit(0);
    }
    result
}

// ── Headless Mode ──

async fn headless(
    client: &Client,
    tools: Arc<ToolRegistry>,
    model: &str,
    show_thinking: bool,
    prompt_text: &str,
    session: SessionHandle,
    file_tracker: Arc<FileTracker>,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking, Arc::clone(&tools));
    let user_msg = Message::user(prompt_text);
    let outcome = session.record_message(user_msg.clone()).await;
    sink.session_write_error(outcome.failure.as_deref());
    let mut messages = vec![user_msg];
    let prompt = prompt::build_prompt(model).await;
    let mut shutdown_fired = false;
    let (_user_tx, mut user_rx) = inert_user_action_channel();
    let turn = agent_turn(
        client,
        &tools,
        &mut messages,
        &prompt,
        &sink,
        &session,
        &mut user_rx,
        client.max_tool_rounds(),
    );
    let result: Result<()> = tokio::select! {
        outcome = turn => match outcome.result {
            Ok(()) | Err(TurnAbort::Cancelled | TurnAbort::Quit) => Ok(()),
            Err(TurnAbort::Failed(e)) => Err(e),
        },
        () = shutdown_signal() => {
            eprintln!();
            shutdown_fired = true;
            Ok(())
        }
    };
    let failure = session.finalize(file_tracker.snapshot_all()).await;
    sink.session_write_error(failure.as_deref());

    if shutdown_fired {
        std::process::exit(0);
    }
    result?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::client::anthropic::testing::{api_key, test_config};
    use crate::config::{AutoCompactionConfig, CompactionConfig};
    use crate::message::ContentBlock;
    use crate::session::store::test_store;

    fn streamed_summary_body(text: &str) -> String {
        let start = serde_json::json!({
            "type": "message_start",
            "message": {"id": "m", "model": "claude-haiku-4-5"},
        });
        let block = serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": text},
        });
        format!(
            "event: message_start\ndata: {start}\n\n\
             event: content_block_start\ndata: {block}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        )
    }

    #[tokio::test]
    async fn auto_compact_before_prompt_compacts_previous_turn_before_recording_new_prompt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(streamed_summary_body("auto summary"))
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let mut config = test_config(server.uri(), api_key(), "claude-opus-4-7[1m]");
        config.compaction = CompactionConfig::resolved_for_test(AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(50_000),
        });
        let client = Client::new(config, Some("sid".to_owned())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let session = session::handle::start(&store, "claude-opus-4-7[1m]");
        let file_tracker = Arc::new(FileTracker::default());
        let sink = CapturingSink::new();
        let (_tx, mut user_rx) = agent::event::inert_user_action_channel();
        let mut pending = Vec::new();
        let mut failures = 0;
        let mut messages = vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
            Message::assistant("four"),
        ];

        let compacted = auto_compact_before_prompt(
            &client,
            &session,
            &file_tracker,
            &mut messages,
            &sink,
            &mut user_rx,
            &mut pending,
            &mut failures,
            Some(TokenUsage::new(50_000, 1)),
        )
        .await
        .unwrap();

        assert!(compacted);
        assert!(pending.is_empty());
        assert_eq!(failures, 0);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("auto summary"))
        );
        assert!(sink.events().iter().any(|event| {
            matches!(
                event,
                AgentEvent::SessionCompacted {
                    automatic: true,
                    ..
                }
            )
        }));
    }

    #[tokio::test]
    async fn handle_action_swap_config_resets_auto_compaction_breaker() {
        let server = MockServer::start().await;
        let config = test_config(server.uri(), api_key(), "claude-opus-4-7[1m]");
        let client = Client::new(config, Some("sid".to_owned())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let session = session::handle::start(&store, "claude-opus-4-7[1m]");
        let file_tracker = Arc::new(FileTracker::default());
        let (sink, mut event_rx) = tui::event::channel();
        let (_user_tx, user_rx) = agent::event::inert_user_action_channel();
        let mut task = AgentLoopTask {
            client,
            tools: Arc::new(ToolRegistry::new(Vec::new())),
            sink,
            user_rx,
            session,
            messages: Vec::new(),
            store,
            file_tracker,
            auto_compaction_failures: 3,
            last_usage: Some(TokenUsage::new(100_000, 1)),
            displayed_usage: None,
            total_estimated_cost_usd: 0.0,
        };

        let control = task
            .handle_action(UserAction::SwapConfig {
                model: None,
                effort: Some(Effort::Xhigh),
            })
            .await;

        assert!(matches!(control, LoopControl::Continue));
        assert_eq!(task.auto_compaction_failures, 0);
        assert_eq!(task.last_usage, Some(TokenUsage::new(100_000, 1)));
        assert!(matches!(
            event_rx.recv().await,
            Some(AgentEvent::ConfigChanged { .. })
        ));
    }

    #[tokio::test]
    async fn handle_action_failed_resume_preserves_current_session_usage_state() {
        let server = MockServer::start().await;
        let config = test_config(server.uri(), api_key(), "claude-opus-4-7[1m]");
        let client = Client::new(config, Some("sid".to_owned())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let session = session::handle::start(&store, "claude-opus-4-7[1m]");
        let original_id = session.session_id().to_owned();
        let file_tracker = Arc::new(FileTracker::default());
        let (sink, mut event_rx) = tui::event::channel();
        let (_user_tx, user_rx) = agent::event::inert_user_action_channel();
        let last_usage = TokenUsage::new(100_000, 10);
        let displayed_usage = TokenUsage::new(90_000, 9);
        let mut task = AgentLoopTask {
            client,
            tools: Arc::new(ToolRegistry::new(Vec::new())),
            sink,
            user_rx,
            session,
            messages: vec![Message::user("still here")],
            store,
            file_tracker,
            auto_compaction_failures: 2,
            last_usage: Some(last_usage),
            displayed_usage: Some(displayed_usage),
            total_estimated_cost_usd: 1.23,
        };

        let control = task
            .handle_action(UserAction::Resume {
                session_id: "missing-target-id".to_owned(),
            })
            .await;

        assert!(matches!(control, LoopControl::Continue));
        assert_eq!(task.session.session_id(), original_id);
        assert_eq!(task.auto_compaction_failures, 2);
        assert_eq!(task.last_usage, Some(last_usage));
        assert_eq!(task.displayed_usage, Some(displayed_usage));
        assert!((task.total_estimated_cost_usd - 1.23).abs() < f64::EPSILON);
        assert!(matches!(
            event_rx.recv().await,
            Some(AgentEvent::Error(msg)) if msg.contains("still on session")
        ));
    }
}
