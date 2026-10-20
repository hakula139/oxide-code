//! CLI entry point: config loading, session resolution, mode dispatch.

mod client;
mod command;
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

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
use clap::{Arg, ArgGroup, Command, Parser};
use time::error::IndeterminateOffset;
use tracing::warn;

use client::anthropic::identity;
use config::{Config, ConfigSnapshot};
use driver::Exit;
use driver::headless::OutputFormat;
use driver::tui::Startup;
use engine::{Engine, EnginePanic, SessionParts};
use file_tracker::FileTracker;
use permission::Mode;
use session::display::archive_notice;
use session::handle::ResumedSession;
use session::list_view::render_list;
use session::resolver::resolve_session;
use session::store::SessionStore;
use session::store::listing::{DEFAULT_SESSION_LIST_LIMIT, ListScope};
use slash::ResumePicker;
use tui::theme::Theme;
use util::tls::PlatformTrust;

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

    /// Resume a session: the most recent with no value, or the one matching an ID prefix.
    #[expect(
        clippy::option_option,
        reason = "encodes three CLI states: \
                  absent (None), flag only (Some(None)), flag with value (Some(Some))"
    )]
    #[arg(
        short = 'c',
        long = "continue",
        value_name = "SESSION_ID",
        conflicts_with = "prompt"
    )]
    r#continue: Option<Option<String>>,

    /// Cap `--list` to the N most-recent sessions, or `0` for no cap.
    #[arg(long, value_name = "N", requires = "list", default_value_t = DEFAULT_SESSION_LIST_LIMIT)]
    limit: usize,

    /// List recent sessions and exit.
    #[arg(short, long, conflicts_with_all = ["prompt", "continue"])]
    list: bool,

    /// Start in this permission mode, overriding `permission.mode` and `OX_PERMISSION_MODE`.
    #[arg(long, value_name = "MODE", value_parser = ModeParser::new())]
    mode: Option<Mode>,

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

    /// Open the TUI on the session picker, and exit without a session if the picker is quit.
    #[arg(short, long, conflicts_with_all = ["continue", "list", "no_tui", "prompt"])]
    resume: bool,
}

/// Parses `--mode`, listing each mode in `--help` with the description the `/mode` picker shows.
#[derive(Clone)]
struct ModeParser(PossibleValuesParser);

impl ModeParser {
    fn new() -> Self {
        Self(PossibleValuesParser::new(Mode::ALL.map(|mode| {
            PossibleValue::new(mode.as_str()).help(mode.description())
        })))
    }
}

impl TypedValueParser for ModeParser {
    type Value = Mode;

    fn parse_ref(
        &self,
        cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Mode, clap::Error> {
        // Replaces invalid UTF-8 as the `ValueEnum` parser behind `--output` does, so the error for
        // such a value lists the modes.
        let value = value.to_string_lossy();
        let name = self.0.parse_ref(cmd, arg, OsStr::new(&*value))?;
        Ok(name.parse().expect("each value names a mode"))
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        self.0.possible_values()
    }
}

fn main() -> Result<ExitCode> {
    let offset_error = util::time::init_local_offset().err();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let exit = match runtime.block_on(async_main(offset_error)) {
        Ok(exit) => exit,
        Err(e) => e.downcast::<EnginePanic>()?.resume(),
    };
    if matches!(exit, Exit::Signal(_)) {
        // tokio's stdin reader can stay blocked in `read`, and a regular drop waits for it.
        runtime.shutdown_background();
    }
    Ok(exit.code())
}

async fn async_main(offset_error: Option<IndeterminateOffset>) -> Result<Exit> {
    let cli = Cli::parse();

    let tui_mode =
        !cli.no_tui && cli.prompt.is_none() && !cli.list && std::io::stdout().is_terminal();
    if cli.resume && !tui_mode {
        bail!("--resume opens the session picker in the TUI, which needs a terminal on stdout");
    }
    let _log_guard = util::log::init_tracing(tui_mode)?;
    if let Some(e) = offset_error {
        warn!("cannot read local timezone offset, falling back to UTC: {e}");
    }
    let cwd = std::env::current_dir().context("failed to read the current working directory")?;

    if cli.list {
        list_sessions(
            &mut std::io::stdout().lock(),
            &cwd,
            ListScope {
                all: cli.all,
                archived: cli.archived,
            },
            cli.limit,
        )?;
        return Ok(Exit::Success);
    }

    let mut config = Config::load(&cwd, tui_mode).await?;
    if let Some(mode) = cli.mode {
        config.apply_mode_flag(mode);
    }
    let show_thinking = config.show_thinking;
    let theme = tui_mode
        .then(|| tui::theme::resolve_theme(&config.theme))
        .transpose()?;
    let snapshot = config.snapshot();
    let theme_notices = theme.iter().flat_map(|resolved| &resolved.notices);
    for notice in snapshot.notices().iter().chain(theme_notices) {
        warn!("{notice}");
    }

    let store = SessionStore::open(&cwd)?;
    let file_tracker = Arc::new(FileTracker::default());
    let git_branch = util::git::current_branch(&cwd).await;
    let ResumedSession {
        handle,
        transcript,
        title,
        unarchived,
        file_snapshots,
    } = resolve_session(
        &store,
        config.model.id(),
        git_branch.clone(),
        cli.r#continue.as_ref(),
        cli.all,
    )
    .await?;
    if unarchived && !tui_mode {
        eprintln!("{}", archive_notice(handle.session_id(), false));
    }
    let drifted = file_tracker.restore_verified(file_snapshots);
    let picker = cli
        .resume
        .then(|| ResumePicker::at_startup(store.clone(), handle.session_id().to_owned(), cli.all));

    let engine = Engine::new(
        &config,
        PlatformTrust(config.extra_ca_certs.clone()),
        identity::load_or_create_device_id(),
        SessionParts {
            store,
            handle,
            transcript,
            file_tracker,
        },
        tui_mode,
    )
    .await?;

    if let Some(prompt) = cli.prompt {
        return Ok(driver::headless::run(engine, show_thinking, &prompt, cli.output).await?);
    }

    let Some(theme) = theme else {
        return driver::repl::run(engine, show_thinking, &drifted).await;
    };

    let startup = Startup {
        git_branch,
        picker,
        theme_notices: theme.notices,
        title,
        unarchived,
    };
    Box::pin(run_tui(
        engine,
        &theme.theme,
        snapshot,
        show_thinking,
        startup,
        &drifted,
    ))
    .await
}

/// Runs the TUI until the user quits or a shutdown signal arrives, which then sets the exit status.
async fn run_tui(
    engine: Engine,
    theme: &Theme,
    snapshot: ConfigSnapshot,
    show_thinking: bool,
    startup: Startup,
    drifted: &[PathBuf],
) -> Result<Exit> {
    let signal = driver::tui::run(
        engine,
        theme,
        snapshot,
        show_thinking,
        startup,
        drifted,
        driver::shutdown_signal(),
    )
    .await?;
    Ok(signal.map_or(Exit::Success, Exit::Signal))
}

// ── Session Helpers ──

fn list_sessions(
    out: &mut dyn std::io::Write,
    cwd: &Path,
    scope: ListScope,
    limit: usize,
) -> Result<()> {
    let store = SessionStore::open(cwd)?;
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::CommandFactory;
    use clap::error::ErrorKind;
    use indoc::formatdoc;
    use time::macros::datetime;

    use super::*;
    use crate::session::store::testing::{archive_test_session, seed_test_session};

    mod cli {
        use super::*;

        #[test]
        fn mode_takes_a_permission_mode() {
            let cli = Cli::try_parse_from(["ox", "--mode", "plan"]).unwrap();
            assert_eq!(cli.mode, Some(Mode::Plan));
            assert_eq!(Cli::try_parse_from(["ox"]).unwrap().mode, None);
            let invalid = Cli::try_parse_from(["ox", "--mode", "bypass"])
                .err()
                .expect("bypass is not a mode");
            assert_eq!(invalid.kind(), ErrorKind::InvalidValue);
            assert!(
                invalid
                    .to_string()
                    .contains("[possible values: manual, plan, yolo]"),
                "{invalid}",
            );
        }

        #[test]
        fn mode_long_help_describes_each_mode() {
            let help = Cli::command().render_long_help().to_string();

            for line in [
                "- manual: Ask about calls no rule settles",
                "- plan:   Read-only: deny edits and commands",
                "- yolo:   Run calls no rule settles",
            ] {
                assert!(help.contains(line), "{help}");
            }
        }

        #[test]
        fn mode_lists_the_modes_for_a_value_that_is_not_utf8() {
            #[cfg(unix)]
            let value = <OsString as std::os::unix::ffi::OsStringExt>::from_vec(vec![0xff]);
            #[cfg(windows)]
            let value = <OsString as std::os::windows::ffi::OsStringExt>::from_wide(&[0xd800]);

            let invalid = Cli::try_parse_from([OsString::from("ox"), "--mode".into(), value])
                .err()
                .expect("the value names no mode");

            assert_eq!(invalid.kind(), ErrorKind::InvalidValue);
            assert_eq!(
                invalid.to_string(),
                formatdoc! {"
                    error: invalid value '{}' for '--mode <MODE>'
                      [possible values: manual, plan, yolo]

                    For more information, try '--help'.
                    ",
                    char::REPLACEMENT_CHARACTER,
                },
            );
        }

        #[test]
        fn output_picks_a_headless_format_and_defaults_to_text() {
            let cli =
                Cli::try_parse_from(["ox", "-p", "hello", "--output", "stream-json"]).unwrap();
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
        fn resume_takes_all_to_widen_the_picker() {
            let cli = Cli::try_parse_from(["ox", "-r", "--all"]).unwrap();
            assert!(cli.resume);
            assert!(cli.all);
        }

        #[test]
        fn resume_conflicts_with_every_mode_that_skips_the_picker() {
            let kinds = [
                &["ox", "--resume", "--continue"][..],
                &["ox", "--resume", "--list"],
                &["ox", "--resume", "--no-tui"],
                &["ox", "--resume", "-p", "hello"],
            ]
            .map(|args| Cli::try_parse_from(args).err().map(|error| error.kind()));
            assert_eq!(kinds, [Some(ErrorKind::ArgumentConflict); 4]);
        }
    }

    mod list_sessions {
        use super::*;

        #[test]
        fn scope_limit_and_archive_state() {
            let dir = tempfile::tempdir().unwrap();
            let cwd = Path::new("/work/local");
            temp_env::with_var("XDG_DATA_HOME", Some(dir.path()), || {
                let store = SessionStore::open(cwd).unwrap();
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
                list_sessions(&mut output, cwd, ListScope::default(), 0).unwrap();
                let local = String::from_utf8(output).unwrap();
                assert!(local.contains("Local session"));
                assert!(!local.contains("Other session"));
                assert!(!local.contains("Project"));

                let mut output = Vec::new();
                list_sessions(
                    &mut output,
                    cwd,
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
                    cwd,
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

                archive_test_session(&other, "other-session");
                let mut output = Vec::new();
                list_sessions(
                    &mut output,
                    cwd,
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
        fn storage_error() {
            let file = tempfile::NamedTempFile::new().unwrap();
            temp_env::with_var("XDG_DATA_HOME", Some(file.path()), || {
                assert!(
                    list_sessions(&mut Vec::new(), Path::new("/work"), ListScope::default(), 0)
                        .is_err()
                );
            });
        }
    }
}
