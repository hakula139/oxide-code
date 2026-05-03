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

use anyhow::Result;
use clap::{ArgGroup, Parser};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use agent::event::{AgentEvent, AgentSink, StdioSink, UserAction, inert_user_action_channel};
use agent::{TurnAbort, agent_turn};
use client::anthropic::Client;
use config::Config;
use file_tracker::FileTracker;
use message::Message;
use session::handle::{ResumedSession, SessionHandle, roll as roll_session};
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::SessionStore;
use slash::SessionInfo;
use tool::{
    ToolRegistry, bash::BashTool, edit::EditTool, glob::GlobTool, grep::GrepTool, read::ReadTool,
    write::WriteTool,
};
use util::path::tildify;

/// Computed before the tokio runtime starts (unsound under multi-threaded).
static LOCAL_OFFSET: std::sync::OnceLock<time::UtcOffset> = std::sync::OnceLock::new();

#[derive(Parser)]
#[command(name = "ox", version, about = "A terminal-based AI coding assistant")]
#[command(group(
    ArgGroup::new("scope")
        .args(["list", "continue"])
        .multiple(true),
))]
struct Cli {
    /// Disable the TUI and use a bare REPL instead.
    #[arg(long)]
    no_tui: bool,

    /// Run in headless mode: send a single prompt and print the response.
    #[arg(short, long, value_name = "PROMPT")]
    prompt: Option<String>,

    /// Resume a session. Without a value, resumes the most recent session.
    /// With a session ID prefix, resumes that specific session.
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

    /// List recent sessions and exit.
    #[arg(short, long, conflicts_with_all = ["prompt", "continue"])]
    list: bool,

    /// Operate across every project. Widens the scope of `--list` /
    /// `--continue` from the current working directory to every
    /// project. Must be combined with `--list` or `--continue`; on its
    /// own (or with `--prompt`) it would have no effect.
    #[arg(short, long, requires = "scope")]
    all: bool,
}

fn main() -> Result<()> {
    LOCAL_OFFSET
        .set(time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC))
        .ok();
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
        return list_sessions(cli.all);
    }

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let model = config.model.clone();
    let theme = config.theme.clone();
    let snapshot = config.snapshot();

    let store = SessionStore::open()?;
    let file_tracker = Arc::new(FileTracker::default());
    let mut resumed = resolve_session(&store, &model, cli.r#continue.as_ref(), cli.all).await?;
    file_tracker.restore_verified(std::mem::take(&mut resumed.file_snapshots));

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

fn list_sessions(all: bool) -> Result<()> {
    let store = SessionStore::open()?;
    let local_offset = *LOCAL_OFFSET.get().unwrap_or(&time::UtcOffset::UTC);
    let term_width = detect_terminal_width();
    render_list(
        &mut std::io::stdout().lock(),
        &store,
        all,
        local_offset,
        term_width,
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

/// Waits for SIGINT, SIGTERM, or SIGHUP (Unix). Returns on first arrival.
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

    let session_info = SessionInfo {
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
        resumed_title,
        agent_rx,
        user_tx,
        &resumed_messages,
        &resumed_tool_metadata,
        Arc::clone(&tools),
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

/// Each `TurnAbort` arm emits exactly one terminal event (`Error` and
/// `TurnComplete` are mutually exclusive).
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
            UserAction::Cancel | UserAction::ConfirmExit => {}
            UserAction::Clear => {
                let outcome =
                    roll_session(&mut session, &store, &file_tracker, client.model()).await;
                sink.session_write_error(outcome.finalize_failure.as_deref());
                client.set_session_id(outcome.new_id.clone());
                messages.clear();
                if let Err(e) = sink.send(AgentEvent::SessionRolled { id: outcome.new_id }) {
                    warn!("session-rolled event dropped: {e}");
                }
            }
            UserAction::SwitchModel(id) => {
                let effort = client.set_model(id.clone());
                if let Err(e) = sink.send(AgentEvent::ModelSwitched {
                    model_id: id,
                    effort,
                }) {
                    warn!("model-switched event dropped: {e}");
                }
            }
            UserAction::SwitchEffort(pick) => {
                let effort = client.set_effort(pick);
                if let Err(e) = sink.send(AgentEvent::EffortSwitched { pick, effort }) {
                    warn!("effort-switched event dropped: {e}");
                }
            }
            UserAction::Quit => break,
        }
    }

    Ok(())
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

    // tokio::io::stdin's blocking thread hangs runtime Drop on signal exit.
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
