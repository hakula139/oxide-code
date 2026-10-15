//! CLI entry point: config loading, session resolution, mode dispatch.

mod agent;
mod client;
mod config;
mod driver;
mod engine;
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
use tracing::warn;

use client::anthropic::Client;
use config::Config;
use driver::Exit;
use driver::tui::History;
use engine::Engine;
use file_tracker::FileTracker;
use session::display::archive_notice;
use session::handle::ResumedSession;
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::SessionStore;
use session::store::listing::DEFAULT_SESSION_LIST_LIMIT;
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent CLI switch that clap parses into its own field"
)]
struct Cli {
    /// Widen `--list` / `--continue` from the current cwd to every project.
    #[arg(short, long, requires = "scope")]
    all: bool,

    /// With `--list`, show only archived sessions.
    #[arg(long, requires = "list")]
    archived: bool,

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
        list_sessions(
            &mut std::io::stdout().lock(),
            cli.all,
            cli.archived,
            cli.limit,
        )?;
        return Ok(Exit::Success);
    }

    let config = Config::load().await?;
    if let Some(notice) = config.model.unsupported_notice() {
        warn!("{notice}");
    }
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
        unarchived,
        tool_result_metadata,
        file_snapshots,
    } = resolve_session(&store, config.model.id(), cli.r#continue.as_ref(), cli.all).await?;
    if unarchived && !tui_mode {
        eprintln!("{}", archive_notice(handle.session_id(), false));
    }
    let drifted = file_tracker.restore_verified(file_snapshots);

    let client = Client::new(config, Some(handle.session_id().to_owned()))?;
    let tools = Arc::new(create_tool_registry(&file_tracker));
    let engine = Engine::new(
        client,
        tools,
        store,
        handle,
        messages,
        context,
        file_tracker,
        cwd,
        tui_mode,
    )
    .await;

    if let Some(prompt) = cli.prompt {
        return Ok(driver::headless::run(engine, show_thinking, &prompt).await);
    }

    if !tui_mode {
        return driver::repl::run(engine, show_thinking, &drifted).await;
    }

    let history = History {
        compact,
        tool_metadata: tool_result_metadata,
        title,
        unarchived,
    };
    driver::tui::run(engine, &theme, snapshot, show_thinking, history, &drifted).await
}

// ── Session Helpers ──

fn list_sessions(
    out: &mut dyn std::io::Write,
    all: bool,
    archived: bool,
    limit: usize,
) -> Result<()> {
    let store = SessionStore::open()?;
    let local_offset = util::time::local_offset();
    let term_width = detect_terminal_width();
    let cap = (limit > 0).then_some(limit);
    render_list(out, &store, all, archived, local_offset, term_width, cap)
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

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::seed_test_session;

    // ── list_sessions ──

    #[test]
    fn list_sessions_scope_limit_and_archive_state() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("XDG_DATA_HOME", Some(dir.path()), || {
            let store = SessionStore::open().unwrap();
            seed_test_session(
                &store,
                "local-session",
                Some("Local session"),
                Some(2),
                datetime!(2026-01-01 00:00 UTC),
            );
            let other = SessionStore::open_at(dir.path().join("ox/sessions"), "other").unwrap();
            seed_test_session(
                &other,
                "other-session",
                Some("Other session"),
                Some(3),
                datetime!(2026-01-02 00:00 UTC),
            );

            let mut output = Vec::new();
            list_sessions(&mut output, false, false, 0).unwrap();
            let local = String::from_utf8(output).unwrap();
            assert!(local.contains("Local session"));
            assert!(!local.contains("Other session"));
            assert!(!local.contains("Project"));

            let mut output = Vec::new();
            list_sessions(&mut output, true, false, 0).unwrap();
            let all = String::from_utf8(output).unwrap();
            assert!(all.contains("Local session"));
            assert!(all.contains("Other session"));
            assert!(all.contains("Project"));
            assert!(!all.contains("... more"));

            let mut output = Vec::new();
            list_sessions(&mut output, true, false, 1).unwrap();
            let capped = String::from_utf8(output).unwrap();
            assert!(capped.contains("Other session"));
            assert!(!capped.contains("Local session"));
            assert!(capped.contains("... more"));

            set_archived_at(&other.find_session_path("other-session").unwrap(), true).unwrap();
            let mut output = Vec::new();
            list_sessions(&mut output, true, true, 0).unwrap();
            let archived = String::from_utf8(output).unwrap();
            assert!(archived.contains("Other session"));
            assert!(!archived.contains("Local session"));
        });
    }

    #[test]
    fn list_sessions_storage_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        temp_env::with_var("XDG_DATA_HOME", Some(file.path()), || {
            assert!(list_sessions(&mut Vec::new(), false, false, 0).is_err());
        });
    }

    // ── create_tool_registry ──

    #[tokio::test]
    async fn create_tool_registry_shares_file_tracking() {
        let tracker = Arc::new(FileTracker::default());
        let tools = create_tool_registry(&tracker);
        let names: Vec<_> = tools.definitions().iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["bash", "read", "write", "edit", "glob", "grep"]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("example.txt");
        std::fs::write(&path, "original").unwrap();
        let edit = serde_json::json!({
            "file_path": path,
            "old_string": "original",
            "new_string": "edited",
        });
        let rejected = tools.run("edit", edit.clone()).await;
        assert!(rejected.is_error);
        assert!(rejected.content.contains("has not been read"));

        let read = tools
            .run("read", serde_json::json!({ "file_path": path }))
            .await;
        assert!(!read.is_error, "{}", read.content);
        let edited = tools.run("edit", edit).await;
        assert!(!edited.is_error, "{}", edited.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");

        let written = tools
            .run(
                "write",
                serde_json::json!({
                    "file_path": path,
                    "content": "rewritten",
                }),
            )
            .await;
        assert!(!written.is_error, "{}", written.content);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "rewritten");
    }
}
