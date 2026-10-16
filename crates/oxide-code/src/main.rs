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
mod protocol;
mod session;
mod slash;
mod tool;
mod tui;
mod util;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser};
use tracing::warn;

use client::anthropic::Client;
use config::Config;
use driver::Exit;
use driver::headless::OutputFormat;
use driver::tui::Startup;
use engine::Engine;
use file_tracker::FileTracker;
use permission::ReadDeny;
use session::display::archive_notice;
use session::handle::ResumedSession;
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::SessionStore;
use session::store::listing::{DEFAULT_SESSION_LIST_LIMIT, ListScope};
use slash::ResumePicker;
use tool::{
    ToolRegistry, bash::BashTool, edit::EditTool, glob::GlobTool, grep::GrepTool, read::ReadTool,
    write::WriteTool,
};

#[derive(Parser)]
#[command(name = "ox", version, about = "A terminal-based AI coding assistant")]
#[command(group(
    ArgGroup::new("scope")
        .args(["list", "continue", "resume"])
        .multiple(true),
))]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent CLI switch that clap parses into its own field"
)]
struct Cli {
    /// Widen `--list`, `--continue`, and `--resume` from the current cwd to every project.
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

    /// With `--prompt`, print the reply as text, one JSON result, or a JSON line per event.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        default_value_t,
        requires = "prompt"
    )]
    output: OutputFormat,

    /// Run in headless mode: send a single prompt and print the response.
    #[arg(short, long, value_name = "PROMPT")]
    prompt: Option<String>,

    /// Open the TUI on the session picker. Quitting the picker exits without starting a session.
    #[arg(short, long, conflicts_with_all = ["continue", "list", "no_tui", "prompt"])]
    resume: bool,
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
    if cli.resume && !tui_mode {
        bail!("--resume opens the session picker in the TUI, which needs a terminal on stdout");
    }
    let _log_guard = util::log::init_tracing(tui_mode)?;

    if cli.list {
        list_sessions(
            &mut std::io::stdout().lock(),
            ListScope {
                all: cli.all,
                archived: cli.archived,
            },
            cli.limit,
        )?;
        return Ok(Exit::Success);
    }

    let cwd = std::env::current_dir().context("failed to read the current working directory")?;
    let config = Config::load(&cwd, tui_mode).await?;
    let show_thinking = config.show_thinking;
    let theme = tui_mode
        .then(|| tui::theme::resolve_theme(&config.theme))
        .transpose()?;
    let snapshot = config.snapshot();
    for notice in snapshot.notices() {
        warn!("{notice}");
    }

    let store = SessionStore::open()?;
    let file_tracker = Arc::new(FileTracker::default());
    let ResumedSession {
        handle,
        transcript,
        title,
        unarchived,
        file_snapshots,
    } = resolve_session(&store, config.model.id(), cli.r#continue.as_ref(), cli.all).await?;
    if unarchived && !tui_mode {
        eprintln!("{}", archive_notice(handle.session_id(), false));
    }
    let drifted = file_tracker.restore_verified(file_snapshots);
    let picker = cli
        .resume
        .then(|| ResumePicker::at_startup(store.clone(), handle.session_id().to_owned(), cli.all));

    let read_deny = config.permission.read_deny();
    let client = Client::new(config, Some(handle.session_id().to_owned()))?;
    let tools = Arc::new(create_tool_registry(&file_tracker, &read_deny));
    let engine = Engine::new(
        client,
        tools,
        store,
        handle,
        transcript,
        file_tracker,
        cwd,
        tui_mode,
    )
    .await;

    if let Some(prompt) = cli.prompt {
        return Ok(driver::headless::run(engine, show_thinking, &prompt, cli.output).await);
    }

    let Some(theme) = theme else {
        return driver::repl::run(engine, show_thinking, &drifted).await;
    };

    let startup = Startup {
        picker,
        title,
        unarchived,
    };
    driver::tui::run(engine, &theme, snapshot, show_thinking, startup, &drifted).await
}

// ── Session Helpers ──

fn list_sessions(out: &mut dyn std::io::Write, scope: ListScope, limit: usize) -> Result<()> {
    let store = SessionStore::open()?;
    let local_offset = util::time::local_offset();
    let term_width = detect_terminal_width();
    let cap = (limit > 0).then_some(limit);
    render_list(out, &store, scope, local_offset, term_width, cap)
}

fn detect_terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    crossterm::terminal::size()
        .ok()
        .map(|(cols, _)| usize::from(cols))
}

fn create_tool_registry(tracker: &Arc<FileTracker>, read_deny: &ReadDeny) -> ToolRegistry {
    ToolRegistry::new(vec![
        Box::new(BashTool),
        Box::new(ReadTool::new(Arc::clone(tracker))),
        Box::new(WriteTool::new(Arc::clone(tracker))),
        Box::new(EditTool::new(Arc::clone(tracker))),
        Box::new(GlobTool::new(read_deny.clone())),
        Box::new(GrepTool::new(read_deny.clone())),
    ])
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use time::macros::datetime;

    use super::*;
    use crate::permission::{Mode, Policy};
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::seed_test_session;
    use crate::tool::testing::canonical_tempdir;
    use crate::util::path::slash_separated;

    // ── Cli ──

    #[test]
    fn cli_output_picks_a_headless_format_and_defaults_to_text() {
        let cli = Cli::try_parse_from(["ox", "-p", "hello", "--output", "stream-json"]).unwrap();
        assert_eq!(cli.output, OutputFormat::StreamJson);
        let cli = Cli::try_parse_from(["ox", "-p", "hello"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Text);
        let missing = Cli::try_parse_from(["ox", "--output", "json"]).err();
        assert_eq!(
            missing.map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument),
        );
    }

    #[test]
    fn cli_resume_takes_all_to_widen_the_picker() {
        let cli = Cli::try_parse_from(["ox", "-r", "--all"]).unwrap();
        assert!(cli.resume);
        assert!(cli.all);
    }

    #[test]
    fn cli_resume_conflicts_with_every_mode_that_skips_the_picker() {
        let kinds = [
            &["ox", "--resume", "--continue"][..],
            &["ox", "--resume", "--list"],
            &["ox", "--resume", "--no-tui"],
            &["ox", "--resume", "-p", "hello"],
        ]
        .map(|args| Cli::try_parse_from(args).err().map(|error| error.kind()));
        assert_eq!(kinds, [Some(ErrorKind::ArgumentConflict); 4]);
    }

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
            list_sessions(&mut output, ListScope::default(), 0).unwrap();
            let local = String::from_utf8(output).unwrap();
            assert!(local.contains("Local session"));
            assert!(!local.contains("Other session"));
            assert!(!local.contains("Project"));

            let mut output = Vec::new();
            list_sessions(
                &mut output,
                ListScope {
                    all: true,
                    ..ListScope::default()
                },
                0,
            )
            .unwrap();
            let all = String::from_utf8(output).unwrap();
            assert!(all.contains("Local session"));
            assert!(all.contains("Other session"));
            assert!(all.contains("Project"));
            assert!(!all.contains("... more"));

            let mut output = Vec::new();
            list_sessions(
                &mut output,
                ListScope {
                    all: true,
                    ..ListScope::default()
                },
                1,
            )
            .unwrap();
            let capped = String::from_utf8(output).unwrap();
            assert!(capped.contains("Other session"));
            assert!(!capped.contains("Local session"));
            assert!(capped.contains("... more"));

            set_archived_at(&other.find_session_path("other-session").unwrap(), true).unwrap();
            let mut output = Vec::new();
            list_sessions(
                &mut output,
                ListScope {
                    all: true,
                    archived: true,
                },
                0,
            )
            .unwrap();
            let archived = String::from_utf8(output).unwrap();
            assert!(archived.contains("Other session"));
            assert!(!archived.contains("Local session"));
        });
    }

    #[test]
    fn list_sessions_storage_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        temp_env::with_var("XDG_DATA_HOME", Some(file.path()), || {
            assert!(list_sessions(&mut Vec::new(), ListScope::default(), 0).is_err());
        });
    }

    // ── create_tool_registry ──

    #[tokio::test]
    async fn create_tool_registry_shares_file_tracking() {
        let tracker = Arc::new(FileTracker::default());
        let tools = create_tool_registry(&tracker, &ReadDeny::default());
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

    #[tokio::test]
    async fn create_tool_registry_hands_read_deny_to_the_search_tools() {
        let dir = canonical_tempdir();
        std::fs::write(dir.path().join("notes.txt"), "token").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "token").unwrap();
        // An absolute rule, since the tools search from the process cwd, which the dir sits outside.
        let deny = [format!("read({}/secret.txt)", slash_separated(dir.path()))];
        let read_deny = Policy::resolve(Mode::Auto, &[], &deny, dir.path())
            .unwrap()
            .read_deny();
        let tools = create_tool_registry(&Arc::new(FileTracker::default()), &read_deny);

        for (tool, pattern) in [("glob", "*.txt"), ("grep", "token")] {
            let input = serde_json::json!({"pattern": pattern, "path": dir.path()});
            let output = tools.run(tool, input).await;
            assert!(!output.is_error, "{tool}: {}", output.content);
            assert!(
                output.content.contains("notes.txt"),
                "{tool}: {}",
                output.content
            );
            assert!(
                !output.content.contains("secret"),
                "{tool}: {}",
                output.content
            );
            assert!(
                output
                    .content
                    .ends_with("(1 file withheld by read deny rules)"),
                "{tool}: {}",
                output.content
            );
        }
    }
}
