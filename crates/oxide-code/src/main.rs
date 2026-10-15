//! CLI entry point: config loading, session resolution, mode dispatch.

mod agent;
mod client;
mod config;
mod driver;
mod file_tracker;
mod message;
mod model;
mod permission;
mod prompt;
mod session;
mod slash;
mod tool;
mod tui;
mod util;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{ArgGroup, Parser};

use client::anthropic::Client;
use config::Config;
use driver::tui::History;
use driver::{Conversation, Exit};
use file_tracker::FileTracker;
use session::handle::ResumedSession;
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::{DEFAULT_SESSION_LIST_LIMIT, SessionStore};
use tool::{
    ToolRegistry, bash::BashTool, edit::EditTool, glob::GlobTool, grep::GrepTool, read::ReadTool,
    write::WriteTool,
};

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

fn main() -> Result<ExitCode> {
    util::time::init_local_offset();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let exit = runtime.block_on(async_main())?;
    if matches!(exit, Exit::Signal(_)) {
        // tokio's stdin reader can stay blocked in `read`, and a regular drop waits for it.
        runtime.shutdown_background();
    }
    Ok(exit.code())
}

async fn async_main() -> Result<Exit> {
    let cli = Cli::parse();

    let tui_mode =
        !cli.no_tui && cli.prompt.is_none() && !cli.list && std::io::stdout().is_terminal();
    let _log_guard = util::log::init_tracing(tui_mode)?;

    if cli.list {
        list_sessions(cli.all, cli.limit)?;
        return Ok(Exit::Success);
    }

    let config = Config::load().await?;
    let show_thinking = config.show_thinking;
    let theme = config.theme.clone();
    let snapshot = config.snapshot();
    let cwd = std::env::current_dir().context("failed to read the current working directory")?;

    let store = SessionStore::open()?;
    let file_tracker = Arc::new(FileTracker::default());
    let ResumedSession {
        handle,
        messages,
        context,
        compact,
        title,
        tool_result_metadata,
        file_snapshots,
    } = resolve_session(&store, &config.model, cli.r#continue.as_ref(), cli.all).await?;
    let drifted = file_tracker.restore_verified(file_snapshots);

    let client = Client::new(config, Some(handle.session_id().to_owned()))?;
    let tools = Arc::new(create_tool_registry(&file_tracker));
    let conversation = Conversation::new(
        client,
        tools,
        handle,
        messages,
        context,
        file_tracker,
        cwd,
        tui_mode,
    )
    .await;

    if let Some(prompt) = cli.prompt {
        return Ok(driver::headless::run(conversation, show_thinking, &prompt).await);
    }

    if !tui_mode {
        return driver::repl::run(conversation, show_thinking, &drifted).await;
    }

    let history = History {
        compact,
        tool_metadata: tool_result_metadata,
        title,
    };
    driver::tui::run(
        conversation,
        store,
        &theme,
        snapshot,
        show_thinking,
        history,
        &drifted,
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
