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

use agent::event::{AgentEvent, AgentSink, StdioSink, UserAction, inert_user_action_channel};
use agent::{TurnAbort, agent_turn};
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

    let cwd = std::env::current_dir()
        .as_deref()
        .map(tildify)
        .unwrap_or_default();

    let session_info = LiveSessionInfo {
        cwd,
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
    reason = "session lifecycle (store, handle, file tracker) lives here for /clear; bundling into a struct would just rename the dependencies"
)]
async fn agent_loop_task(
    mut client: Client,
    tools: Arc<ToolRegistry>,
    sink: tui::event::ChannelSink,
    mut user_rx: mpsc::Receiver<UserAction>,
    mut session: SessionHandle,
    resumed_messages: Vec<Message>,
    store: SessionStore,
    file_tracker: Arc<FileTracker>,
) -> Result<()> {
    let mut messages: Vec<Message> = resumed_messages;

    while let Some(action) = user_rx.recv().await {
        match action {
            UserAction::SubmitPrompt(text) => {
                let user_msg = Message::user(&text);
                let outcome = session.record_message(user_msg.clone()).await;
                sink.session_write_error(outcome.failure.as_deref());
                messages.push(user_msg);

                if let Some(seed) = outcome.ai_title_seed {
                    session::title_generator::spawn(
                        client.clone(),
                        session.clone(),
                        sink.clone(),
                        seed,
                    );
                }

                let prompt = prompt::build_prompt(client.model()).await;
                let outcome = agent_turn(
                    &client,
                    &tools,
                    &mut messages,
                    &prompt,
                    &sink,
                    &session,
                    &mut user_rx,
                )
                .await;
                match outcome {
                    Ok(()) => {
                        _ = sink.send(AgentEvent::TurnComplete);
                    }
                    Err(TurnAbort::Cancelled) => {
                        _ = sink.send(AgentEvent::Cancelled);
                    }
                    Err(TurnAbort::Quit) => break,
                    Err(TurnAbort::Failed(e)) => {
                        _ = sink.send(AgentEvent::Error(format!("{e:#}")));
                    }
                }
            }
            // Cancel / ConfirmExit are no-ops here; PreviewTheme / SwapTheme are TUI-only and
            // applied client-side in `App::apply_action_locally`.
            UserAction::Cancel
            | UserAction::ConfirmExit
            | UserAction::PreviewTheme { .. }
            | UserAction::SwapTheme { .. } => {}
            UserAction::Clear => {
                let outcome =
                    roll_session(&mut session, &store, &file_tracker, client.model()).await;
                sink.session_write_error(outcome.finalize_failure.as_deref());
                client.set_session_id(outcome.new_id.clone());
                messages.clear();
                if let Err(e) = sink.send(AgentEvent::SessionRolled { id: outcome.new_id }) {
                    // /clear succeeded server-side but the TUI never sees the new id — surfaces as
                    // a stuck "old session" header. Error-level so the log makes it findable.
                    tracing::error!("session-rolled event dropped: {e}");
                }
            }
            UserAction::Resume { session_id } => {
                apply_resume(
                    &mut session,
                    &mut client,
                    &mut messages,
                    &store,
                    &file_tracker,
                    &sink,
                    &session_id,
                )
                .await;
            }
            UserAction::Compact { instructions } => {
                let outcome = apply_compact(
                    &client,
                    &session,
                    &file_tracker,
                    &mut messages,
                    &sink,
                    &mut user_rx,
                    instructions,
                )
                .await;
                match outcome {
                    Ok(()) => {}
                    Err(TurnAbort::Cancelled) => {
                        _ = sink.send(AgentEvent::Cancelled);
                    }
                    Err(TurnAbort::Quit) => break,
                    Err(TurnAbort::Failed(e)) => {
                        _ = sink.send(AgentEvent::Error(format!("{e:#}")));
                    }
                }
            }
            UserAction::Rename { title } => {
                apply_rename(&session, &sink, title).await;
            }
            UserAction::SwapConfig { model, effort } => {
                apply_swap_config(&mut client, &sink, model, effort);
            }
            UserAction::Quit => break,
        }
    }

    Ok(())
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
) {
    let outcome = match session::handle::roll_into(session, store, file_tracker, target_id).await {
        Ok(o) => o,
        Err(e) => {
            _ = sink.send(AgentEvent::Error(format!(
                "Resume failed (still on session {}): {e:#}",
                session.session_id(),
            )));
            return;
        }
    };
    let new_id = session.session_id().to_owned();
    client.set_session_id(new_id.clone());
    messages.clone_from(&outcome.messages);
    if let Err(e) = sink.send(AgentEvent::SessionResumed {
        id: new_id,
        title: outcome.title,
        messages: outcome.messages,
        compact: outcome.compact,
        tool_metadata: outcome.tool_result_metadata,
    }) {
        // Channel closed mid-resume leaves the TUI on the OLD chat. Pinpoint the desync.
        tracing::error!("session-resumed event dropped: {e}");
    }
    // Emit OLD-session finalize failure AFTER SessionResumed so the chat-clear doesn't wipe it.
    // Distinct phrasing (not session_write_error) so the user doesn't read it as a current-writer
    // fault.
    if let Some(failure) = outcome.finalize_failure.as_deref() {
        _ = sink.send(AgentEvent::Error(format!(
            "Previous session failed to finalize cleanly: {failure}",
        )));
    }
    if !outcome.drifted_paths.is_empty() {
        _ = sink.send(AgentEvent::Error(format_drift_warning(
            &outcome.drifted_paths,
        )));
    }
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
) -> std::result::Result<(), TurnAbort> {
    let mut pending_prompts = Vec::new();
    let summary = agent::await_unless_aborted(
        agent::compaction::compact_session(client, messages, instructions.as_deref()),
        user_rx,
        &mut pending_prompts,
    )
    .await?
    .map_err(|e| TurnAbort::Failed(anyhow!("Compaction failed: {e:#}")))?;
    let synthetic = agent::compaction::synthesize_post_compact_message(&summary);
    let outcome = session
        .compact(summary.clone(), instructions.clone(), synthetic.clone())
        .await;
    sink.session_write_error(outcome.failure.as_deref());
    if outcome.failure.is_some() {
        return Ok(());
    }
    // Reset the file tracker so post-compact Edits require a fresh Read — pre-compact Reads
    // are no longer in the visible transcript and the safety contract has to follow.
    file_tracker.clear();
    *messages = vec![synthetic];
    if let Err(e) = sink.send(AgentEvent::SessionCompacted {
        summary,
        pre_count: outcome.pre_count,
        instructions,
    }) {
        tracing::error!("session-compacted event dropped: {e}");
    }
    Ok(())
}

async fn apply_rename(session: &SessionHandle, sink: &dyn AgentSink, title: String) {
    let outcome = session.set_manual_title(title.clone()).await;
    sink.session_write_error(outcome.failure.as_deref());
    if outcome.failure.is_some() {
        return;
    }
    if let Err(e) = sink.send(AgentEvent::SessionTitleUpdated {
        session_id: session.session_id().to_owned(),
        title,
    }) {
        tracing::error!("session-title-updated event dropped: {e}");
    }
}

/// Order matters: model swap re-clamps effort before the explicit pick is applied.
fn apply_swap_config(
    client: &mut Client,
    sink: &dyn AgentSink,
    model: Option<ResolvedModelId>,
    effort: Option<Effort>,
) {
    if let Some(id) = model {
        client.set_model(id.into_inner());
    }
    let resolved = match effort {
        Some(pick) => client.set_effort(pick),
        None => client.effort(),
    };
    if let Err(e) = sink.send(AgentEvent::ConfigChanged {
        model_id: client.model().to_owned(),
        effort: resolved,
        requested_effort: effort,
    }) {
        // Dropping this leaves the status bar showing the previous model / effort even though the
        // client has already swapped — error-level so it stands out in the log when the TUI looks
        // wrong after a /model or /effort swap.
        tracing::error!("config-changed event dropped: {e}");
    }
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

            let user_msg = Message::user(&input);
            let outcome = session.record_message(user_msg.clone()).await;
            sink.session_write_error(outcome.failure.as_deref());
            messages.push(user_msg);
            let prompt = prompt::build_prompt(model).await;
            let turn = agent_turn(
                client,
                &tools,
                &mut messages,
                &prompt,
                &sink,
                &session,
                &mut user_rx,
            );
            let turn_result = tokio::select! {
                r = turn => r,
                () = shutdown_signal() => {
                    eprintln!();
                    shutdown_fired = true;
                    break;
                }
            };
            match turn_result {
                Ok(()) | Err(TurnAbort::Cancelled | TurnAbort::Quit) => {}
                Err(TurnAbort::Failed(e)) => return Err(e),
            }
            _ = sink.send(AgentEvent::TurnComplete);
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
    );
    let result: Result<()> = tokio::select! {
        r = turn => match r {
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
