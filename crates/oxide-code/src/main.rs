mod agent;
mod client;
mod config;
mod message;
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
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, warn};

use agent::agent_turn;
use agent::event::{AgentEvent, AgentSink, StdioSink, UserAction};
use client::anthropic::Client;
use config::Config;
use message::Message;
use prompt::environment::marketing_name;
use session::list_view::render_list;
use session::manager::SessionManager;
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

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Handle --list before loading config (no API access needed).
    if cli.list {
        return list_sessions(cli.all);
    }

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let model = config.model.clone();

    // Resolve which session to resume (if any) before creating the client,
    // so we can pass the session ID to the API headers.
    let store = SessionStore::open()?;
    let (session, messages) =
        resolve_session(&store, &model, cli.r#continue.as_ref(), cli.all).await?;
    let client = Client::new(config, Some(session.session_id().to_owned()))?;

    let tools = create_tool_registry();

    if let Some(prompt_text) = cli.prompt {
        return headless(
            &client,
            &tools,
            &model,
            show_thinking,
            &prompt_text,
            session,
        )
        .await;
    }

    if cli.no_tui || !std::io::stdout().is_terminal() {
        return bare_repl(&client, &tools, &model, show_thinking, session, messages).await;
    }

    run_tui(&client, &model, show_thinking, tools, session, messages).await
}

// ── Session Helpers ──

/// Print a table of recent sessions and exit. With `all = true`, spans
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

/// Detect the terminal width for title truncation in `--list`.
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

fn create_tool_registry() -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(BashTool),
        Box::new(ReadTool),
        Box::new(WriteTool),
        Box::new(EditTool),
        Box::new(GlobTool),
        Box::new(GrepTool),
    ])
}

/// Log session I/O errors without aborting the agent loop.
///
/// The first failure within a session is also surfaced to the user via
/// `sink` (when available) so they know the conversation may not be
/// saved. Subsequent failures warn-log only to avoid spamming the UI
/// — the persistence problem has already been announced.
fn log_session_err(
    result: anyhow::Result<()>,
    session: &mut SessionManager,
    sink: Option<&dyn AgentSink>,
) {
    let Err(e) = result else {
        return;
    };
    warn!("session write failed: {e}");
    if session.record_write_failure()
        && let Some(sink) = sink
    {
        _ = sink.send(AgentEvent::Error(format!(
            "Session write failed: {e}. Conversation history may be incomplete; further write errors will be silent."
        )));
    }
}

/// Record one message to the session, surfacing any write failure via
/// `sink`. Holds the session lock only for the duration of the write
/// so other tasks (and concurrent writes from the same task) see
/// fresh access instead of blocking behind a long-running agent turn.
pub(crate) async fn record_session_message(
    session: &Mutex<SessionManager>,
    msg: &Message,
    sink: Option<&dyn AgentSink>,
) {
    let mut s = session.lock().await;
    let r = s.record_message(msg).await;
    log_session_err(r, &mut s, sink);
}

/// Wait for any shutdown signal — SIGINT (portable), SIGTERM, or
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
    tools: ToolRegistry,
    session: SessionManager,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    tui::terminal::install_panic_hook();

    let (agent_sink, agent_rx) = tui::event::channel();
    let (user_tx, user_rx) = mpsc::unbounded_channel::<UserAction>();

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
        display_model,
        show_thinking,
        cwd,
        agent_rx,
        user_tx,
        &resumed_messages,
    );

    let session = Arc::new(Mutex::new(session));

    let agent_handle = {
        let client = client.clone();
        let session = Arc::clone(&session);
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

    // Write the session summary after abort to guarantee it runs. The
    // TUI is already torn down, so surfaced-error channels are gone —
    // fall back to warn-log only (sink = None).
    {
        let mut session = session.lock().await;
        let r = session.finish();
        log_session_err(r, &mut session, None);
    }

    result
}

async fn agent_loop_task(
    client: Client,
    tools: ToolRegistry,
    sink: tui::event::ChannelSink,
    mut user_rx: mpsc::UnboundedReceiver<UserAction>,
    session: Arc<Mutex<SessionManager>>,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    let mut messages: Vec<Message> = resumed_messages;

    while let Some(action) = user_rx.recv().await {
        match action {
            UserAction::SubmitPrompt(text) => {
                let user_msg = Message::user(&text);
                record_session_message(&session, &user_msg, Some(&sink)).await;
                messages.push(user_msg);
                let prompt = prompt::build_prompt(client.model()).await;
                let turn_result =
                    agent_turn(&client, &tools, &mut messages, &prompt, &sink, &session).await;
                if let Err(e) = turn_result {
                    _ = sink.send(AgentEvent::Error(e.to_string()));
                }
                _ = sink.send(AgentEvent::TurnComplete);
            }
            UserAction::Quit => break,
        }
    }

    // Summary is written by the caller (run_tui) to guarantee it runs
    // regardless of how this task exits.
    Ok(())
}

// ── Bare REPL Mode ──

async fn bare_repl(
    client: &Client,
    tools: &ToolRegistry,
    model: &str,
    show_thinking: bool,
    session: SessionManager,
    resumed_messages: Vec<Message>,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut messages: Vec<Message> = resumed_messages;
    // Wrap in Mutex so `agent_turn` (and the user-msg recorder) can
    // lock briefly per write. No other task touches `session` here,
    // so the lock is uncontended; the Mutex just matches agent_turn's
    // shared-state signature.
    let session = Mutex::new(session);
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
            record_session_message(&session, &user_msg, Some(&sink)).await;
            messages.push(user_msg);
            let prompt = prompt::build_prompt(model).await;
            // Allow the in-flight turn to be interrupted too; the
            // session state that's already been written persists and
            // resume-side sanitization heals any dangling tool_use.
            let turn = agent_turn(client, tools, &mut messages, &prompt, &sink, &session);
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

    let mut session = session.into_inner();
    let r = session.finish();
    log_session_err(r, &mut session, Some(&sink));

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
    tools: &ToolRegistry,
    model: &str,
    show_thinking: bool,
    prompt_text: &str,
    session: SessionManager,
) -> Result<()> {
    let sink = StdioSink::new(show_thinking);
    // Wrap in Mutex so `agent_turn` can lock briefly per write. Only
    // one task touches the session in headless mode, so this is just
    // type-plumbing to match the shared-state signature.
    let session = Mutex::new(session);
    let user_msg = Message::user(prompt_text);
    record_session_message(&session, &user_msg, Some(&sink)).await;
    let mut messages = vec![user_msg];
    let prompt = prompt::build_prompt(model).await;
    // Race the single turn against shutdown signals so the recorded
    // user message still gets a Summary entry on Ctrl+C / SIGTERM /
    // SIGHUP; resume-side sanitization heals any dangling state.
    let mut shutdown_fired = false;
    let turn = agent_turn(client, tools, &mut messages, &prompt, &sink, &session);
    let result = tokio::select! {
        r = turn => r,
        () = shutdown_signal() => {
            eprintln!();
            shutdown_fired = true;
            Ok(())
        }
    };
    let mut session = session.into_inner();
    let r = session.finish();
    log_session_err(r, &mut session, Some(&sink));

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
