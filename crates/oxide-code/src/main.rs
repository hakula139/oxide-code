//! Binary entry point.
//!
//! Parses CLI flags, loads [`Config`], resolves which session to
//! resume (if any), and dispatches into one of three run modes: TUI
//! (default), bare REPL (`--no-tui`), or headless one-shot (`-p`).
//! Signal handling and session summary writes on abort live here.

mod agent;
mod client;
mod config;
mod file_tracker;
mod message;
mod model;
mod prompt;
mod session;
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

use agent::agent_turn;
use agent::event::{AgentEvent, AgentSink, StdioSink, UserAction};
use client::anthropic::Client;
use config::Config;
use file_tracker::FileTracker;
use message::Message;
use prompt::environment::marketing_name;
use session::handle::{ResumedSession, SessionHandle};
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::SessionStore;
use tool::{
    ToolRegistry, bash::BashTool, edit::EditTool, glob::GlobTool, grep::GrepTool, read::ReadTool,
    write::WriteTool,
};
use util::path::tildify;

/// Cached local UTC offset, computed before the tokio runtime starts.
///
/// `time::UtcOffset::current_local_offset()` is unsound under
/// multi-threaded runtimes on Linux (it reads `/etc/localtime` via
/// `localtime_r` while other threads may call `setenv`). Computing the
/// offset in single-threaded `fn main()` avoids the issue.
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

    // Decide mode before subscriber init so the writer can match it.
    // TUI mode routes tracing into a file under `$XDG_STATE_HOME` so
    // diagnostics never bleed onto the alternate screen; every other
    // mode keeps stderr (its natural surface for command-line output).
    let tui_mode =
        !cli.no_tui && cli.prompt.is_none() && !cli.list && std::io::stdout().is_terminal();
    // Bind for the function lifetime so the appender's worker thread
    // keeps flushing right up to the final teardown warning. `None` in
    // stderr modes — no async worker to drain.
    let _log_guard = util::log::init_tracing(tui_mode)?;

    // Handle --list before loading config (no API access needed).
    if cli.list {
        return list_sessions(cli.all);
    }

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let model = config.model.clone();
    let theme = config.theme.clone();

    // Resolve which session to resume (if any) before creating the client,
    // so we can pass the session ID to the API headers.
    let store = SessionStore::open()?;
    let file_tracker = Arc::new(FileTracker::default());
    let mut resumed = resolve_session(&store, &model, cli.r#continue.as_ref(), cli.all).await?;
    // Restore before the agent loop so resumed Reads clear the gate
    // without forcing a re-Read.
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
        &model,
        show_thinking,
        &theme,
        tools,
        resumed,
        file_tracker,
    )
    .await
}

// ── Session Helpers ──

/// Prints a table of recent sessions and exits. With `all = true`, spans
/// every project; otherwise scoped to the current working directory.
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

/// Detects the terminal width for title truncation in `--list`.
/// Returns `None` when stdout is not a TTY (piped / redirected) or
/// when the window size cannot be queried — the renderer skips
/// truncation in either case so downstream tools see the full title.
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

/// Waits for any shutdown signal — SIGINT (portable), SIGTERM, or
/// SIGHUP (Unix only). Returns when the first signal arrives.
///
/// Installs the handlers lazily on first call. Callers that embed this
/// in a `tokio::select!` let the arbiter cut off the other branch and
/// run cleanup (session `finish()`, terminal restore, etc.) before the
/// process exits. Crucially, `tokio::signal::ctrl_c` overrides tokio's
/// default "terminate on SIGINT" behavior — without this handler our
/// bare REPL / headless modes would exit without writing a Summary.
///
/// In the TUI, SIGINT from Ctrl+C is already intercepted by crossterm's
/// raw-mode input; this handler catches it only when raw mode is not
/// engaged (e.g., during setup / teardown) and still catches SIGTERM /
/// SIGHUP regardless of mode.
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

async fn run_tui(
    client: &Client,
    model: &str,
    show_thinking: bool,
    theme: &tui::theme::Theme,
    tools: Arc<ToolRegistry>,
    resumed: ResumedSession,
    file_tracker: Arc<FileTracker>,
) -> Result<()> {
    let ResumedSession {
        handle: session,
        messages: resumed_messages,
        title: resumed_title,
        tool_result_metadata: resumed_tool_metadata,
        // Snapshots were already drained into the tracker by the caller.
        file_snapshots: _,
    } = resumed;
    tui::terminal::install_panic_hook();

    let (agent_sink, agent_rx) = tui::event::channel();
    // 32 is plenty: UserAction fires at human typing speed. Bounded so a
    // stalled agent loop surfaces `try_send` failure instead of growing the
    // queue without bound.
    let (user_tx, user_rx) = mpsc::channel::<UserAction>(32);

    let cwd = std::env::current_dir()
        .as_deref()
        .map(tildify)
        .unwrap_or_default();

    let display_model = match marketing_name(model) {
        Some(name) => name.to_owned(),
        None => model.to_owned(),
    };

    let mut terminal = tui::terminal::init()?;
    let mut app = tui::app::App::new(
        theme,
        display_model,
        show_thinking,
        cwd,
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
        tokio::spawn(async move {
            agent_loop_task(
                client,
                tools,
                agent_sink,
                user_rx,
                session,
                resumed_messages,
            )
            .await
        })
    };

    // Run the TUI on the main thread (it needs terminal access).
    // Race against shutdown signals (SIGTERM / SIGHUP — raw mode eats
    // SIGINT before it reaches us) so external signals trigger the
    // same teardown path as a normal quit.
    let result = tokio::select! {
        result = app.run(&mut terminal) => result,
        () = shutdown_signal() => {
            debug!("TUI received shutdown signal, tearing down");
            Ok(())
        }
    };

    tui::terminal::restore();

    // Cancel the agent loop — it may be blocked on an API stream.
    agent_handle.abort();
    match agent_handle.await {
        Ok(Err(e)) => warn!("agent loop error: {e}"),
        Err(e) if !e.is_cancelled() => warn!("agent task panicked: {e}"),
        _ => {}
    }

    // Summary write after abort, no sink available — actor warn-logs
    // the cause.
    let outcome = session.finish(file_tracker.snapshot_all()).await;
    if let Some(msg) = outcome.failure {
        warn!("session finish failed: {msg}");
    }
    session.shutdown().await;

    result
}

async fn agent_loop_task(
    client: Client,
    tools: Arc<ToolRegistry>,
    sink: tui::event::ChannelSink,
    mut user_rx: mpsc::Receiver<UserAction>,
    session: SessionHandle,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    let mut messages: Vec<Message> = resumed_messages;

    while let Some(action) = user_rx.recv().await {
        match action {
            UserAction::SubmitPrompt(text) => {
                let user_msg = Message::user(&text);
                let outcome = session.record_message(user_msg.clone()).await;
                sink.session_write_error(outcome.failure.as_deref());
                messages.push(user_msg);

                // The actor sets the seed only on a fresh session's
                // first user-text message — fire-and-forget the AI
                // title generator from there.
                if let Some(seed) = outcome.ai_title_seed {
                    session::title_generator::spawn(
                        client.clone(),
                        session.clone(),
                        sink.clone(),
                        seed,
                    );
                }

                let prompt = prompt::build_prompt(client.model()).await;
                let outcome = drive_turn(
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
                    TurnOutcome::Completed(Ok(())) => {
                        _ = sink.send(AgentEvent::TurnComplete);
                    }
                    TurnOutcome::Completed(Err(e)) => {
                        // `{e:#}` flattens the anyhow cause chain into one
                        // string ("stream error: API error (HTTP 503): ...").
                        // Plain `Display` would drop everything below the
                        // outermost context and surface only "stream error",
                        // which doesn't distinguish a transient gateway 5xx
                        // from a permanent config error.
                        _ = sink.send(AgentEvent::Error(format!("{e:#}")));
                        _ = sink.send(AgentEvent::TurnComplete);
                    }
                    TurnOutcome::Cancelled => {
                        _ = sink.send(AgentEvent::Cancelled);
                    }
                    TurnOutcome::Quit => break,
                }
            }
            // `ConfirmExit` is a TUI-only signal (arms the exit hint).
            UserAction::Cancel | UserAction::ConfirmExit => {}
            UserAction::Quit => break,
        }
    }

    // Summary is written by the caller (run_tui) to guarantee it runs
    // regardless of how this task exits.
    Ok(())
}

/// Outcome of one [`drive_turn`] call. `Cancelled` drops the future,
/// relying on reqwest's stream-close-on-drop and `kill_on_drop(true)`
/// to reap in-flight work. `Quit` ends the agent loop.
enum TurnOutcome {
    Completed(Result<()>),
    Cancelled,
    Quit,
}

/// Run one turn while watching `user_rx` for cancel / quit so an Esc /
/// Ctrl+C from the TUI can drop the future. Submits during a busy turn
/// are not expected — the TUI disables its input — but a stray one is
/// logged and ignored rather than dropped silently.
async fn drive_turn(
    client: &Client,
    tools: &Arc<ToolRegistry>,
    messages: &mut Vec<Message>,
    prompt: &prompt::PromptParts,
    sink: &tui::event::ChannelSink,
    session: &SessionHandle,
    user_rx: &mut mpsc::Receiver<UserAction>,
) -> TurnOutcome {
    let turn = agent_turn(client, tools, messages, prompt, sink, session);
    tokio::pin!(turn);

    loop {
        tokio::select! {
            result = &mut turn => return TurnOutcome::Completed(result),
            action = user_rx.recv() => match action {
                Some(UserAction::Cancel) => return TurnOutcome::Cancelled,
                // `None`: every sender dropped — TUI is gone, exit.
                Some(UserAction::Quit) | None => return TurnOutcome::Quit,
                Some(UserAction::SubmitPrompt(_)) => {
                    warn!("ignoring submit during in-flight turn");
                }
                // Idle-state signal that shouldn't reach a busy turn,
                // but the TUI may still forward it during teardown.
                Some(UserAction::ConfirmExit) => {}
            },
        }
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
    // Tracks whether we broke out of the loop due to a shutdown signal
    // (as opposed to EOF / error). See the post-`finish()` exit note
    // below for why we care.
    let mut shutdown_fired = false;

    let result: Result<()> = async {
        loop {
            eprint!("> ");
            std::io::stderr().flush()?;

            // Race stdin input against shutdown signals so Ctrl+C (SIGINT),
            // SIGTERM, or SIGHUP break the loop cleanly and fall through
            // to `finish()` below.
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
            // Allow the in-flight turn to be interrupted too; the
            // session state that's already been written persists and
            // resume-side sanitization heals any dangling tool_use.
            let turn = agent_turn(client, &tools, &mut messages, &prompt, &sink, &session);
            let turn_result = tokio::select! {
                r = turn => r,
                () = shutdown_signal() => {
                    eprintln!();
                    shutdown_fired = true;
                    break;
                }
            };
            turn_result?;
            _ = sink.send(AgentEvent::TurnComplete);
        }
        Ok(())
    }
    .await;

    let outcome = session.finish(file_tracker.snapshot_all()).await;
    sink.session_write_error(outcome.failure.as_deref());
    session.shutdown().await;

    // `tokio::io::stdin()` spawns a blocking thread that cannot be
    // cancelled (see tokio::io::stdin docs), so on a signal-induced
    // exit the runtime Drop would hang waiting for that thread until
    // the user hits Enter. Our explicit cleanup (`finish()`) has
    // already run, so skip the runtime teardown entirely via
    // `std::process::exit(0)`. Only do this on signal exit —
    // normal EOF / error paths should return through `main` so the
    // exit code reflects the result.
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
    // Race the single turn against shutdown signals so the recorded
    // user message still gets a Summary entry on Ctrl+C / SIGTERM /
    // SIGHUP; resume-side sanitization heals any dangling state.
    let mut shutdown_fired = false;
    let turn = agent_turn(client, &tools, &mut messages, &prompt, &sink, &session);
    let result = tokio::select! {
        r = turn => r,
        () = shutdown_signal() => {
            eprintln!();
            shutdown_fired = true;
            Ok(())
        }
    };
    let outcome = session.finish(file_tracker.snapshot_all()).await;
    sink.session_write_error(outcome.failure.as_deref());
    session.shutdown().await;

    // Mirror `bare_repl`: on signal exit, skip runtime Drop so any
    // outstanding HTTP / reqwest connection pool doesn't hold the
    // process open. Headless does not touch `tokio::io::stdin`, so
    // this is defensive rather than strictly necessary.
    if shutdown_fired {
        std::process::exit(0);
    }
    result?;
    println!();
    Ok(())
}
