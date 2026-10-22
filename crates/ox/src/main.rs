//! CLI entry point: config loading, session resolution, mode dispatch.

mod driver;
mod session_list;

use std::env;
use std::ffi::OsStr;
use std::io::{self, IsTerminal as _, Write as _};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
use clap::error::ErrorKind;
use clap::{Arg, ArgGroup, Command, CommandFactory as _, Parser};
use indoc::indoc;
use time::error::IndeterminateOffset;
use tracing::warn;

use ox_core::client::anthropic::identity;
use ox_core::config::Config;
use ox_core::engine::{Engine, EnginePanic, SessionParts};
use ox_core::permission::Mode;
use ox_core::session::display::archive_notice;
use ox_core::session::entry::ArchiveState;
use ox_core::session::handle::ResumedSession;
use ox_core::session::resolver::{SessionTarget, resolve_session};
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::{DEFAULT_SESSION_LIST_LIMIT, ListScope, ProjectScope};
use ox_core::util;
use ox_core::util::tls::PlatformTrust;
use ox_tui::Startup;
use ox_tui::slash::ResumePicker;
use ox_tui::terminal;
use ox_tui::theme::resolve_theme;

use driver::Exit;
use driver::headless::OutputFormat;

#[derive(Parser)]
#[command(
    name = "ox",
    version,
    about = "A terminal-based AI coding assistant",
    long_about = indoc! {"
        A terminal-based AI coding assistant.

        Run `ox` to start an interactive session in the working directory, or `ox -p \"<prompt>\"` to answer one prompt and print the reply."
    },
    after_help = indoc! {"
        Examples:
          ox                                    Start an interactive session
          ox \"Fix the failing test\"             Start one with a first prompt
          ox -c                                 Continue the latest session in this project
          ox -r                                 Pick a session to resume
          git diff | ox -p \"Review this diff\"   Answer once, with the diff attached from stdin

        Guides: https://github.com/hakula139/oxide-code/tree/main/docs/guide"
    }
)]
#[command(group(
    ArgGroup::new("scope")
        .args(["list", "continue", "resume"])
        .multiple(true),
))]
#[command(group(
    ArgGroup::new("session")
        .args(["continue", "print", "prompt", "resume"])
        .multiple(true),
))]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent CLI switch that clap parses into its own field"
)]
struct Cli {
    /// Start with this prompt, or with `--print`, answer it.
    #[arg(value_parser = non_blank)]
    prompt: Option<String>,

    /// Widen `--list`, `--continue`, and `--resume` from this project to every project.
    #[arg(short, long, requires = "scope")]
    all: bool,

    /// With `--list`, show only archived sessions.
    #[arg(long, requires = "list", conflicts_with = "session")]
    archived: bool,

    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    r#continue: bool,

    /// Cap `--list` to the N most-recent sessions, or `0` for no cap.
    #[arg(
        long,
        value_name = "N",
        requires = "list",
        conflicts_with = "session",
        default_value_t = DEFAULT_SESSION_LIST_LIMIT
    )]
    limit: usize,

    /// List recent sessions and exit.
    #[arg(short, long, conflicts_with = "session")]
    list: bool,

    /// Start in this permission mode, overriding `permission.mode` and `OX_PERMISSION_MODE`.
    #[arg(long, value_name = "MODE", value_parser = ModeParser::new())]
    mode: Option<Mode>,

    /// Use a bare REPL that reads one prompt per line in place of the TUI.
    #[arg(long)]
    no_tui: bool,

    /// With `--print`, print the reply as text, one JSON result, or a JSON line per event.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        default_value_t,
        requires = "print"
    )]
    output: OutputFormat,

    /// Answer the prompt and any piped stdin without the TUI, print the reply, and exit.
    #[arg(short, long)]
    print: bool,

    /// Resume the session with this ID prefix or session file, or pick one from a list without a
    /// value.
    #[expect(
        clippy::option_option,
        reason = "encodes three CLI states: \
                  absent (None), flag only (Some(None)), flag with value (Some(Some))"
    )]
    #[arg(short, long, value_name = "SESSION", num_args = 0..=1, value_parser = non_blank)]
    resume: Option<Option<String>>,
}

impl Cli {
    /// Parses the command line, exiting with a usage error as clap does for a combination clap
    /// cannot express.
    fn parse_valid() -> Self {
        let cli = Self::parse();
        if let Err(e) = cli.validate() {
            e.exit();
        }
        cli
    }

    /// Rejects the session picker beside a flag that skips the TUI or a first prompt.
    fn validate(&self) -> Result<(), clap::Error> {
        if !self.opens_picker() {
            return Ok(());
        }
        let other = if self.print {
            "--print"
        } else if self.no_tui {
            "--no-tui"
        } else if self.prompt.is_some() {
            "[PROMPT]"
        } else {
            return Ok(());
        };
        Err(Self::command().error(
            ErrorKind::ArgumentConflict,
            format!(
                "`--resume` without a session opens the picker, which cannot be used with \
                 `{other}`. Pass the session's ID prefix to resume it directly."
            ),
        ))
    }

    /// Returns whether bare `--resume` asks for the session picker.
    fn opens_picker(&self) -> bool {
        matches!(self.resume, Some(None))
    }

    /// Returns whether the run opens the TUI, which needs a terminal on stdout. Fails when the
    /// session picker needs the TUI but stdout is not a terminal, or when the TUI would open with
    /// stdin piped, since it reads keys from stdin.
    fn tui_mode(&self) -> Result<bool> {
        let tui_mode = !self.no_tui && !self.print && !self.list && io::stdout().is_terminal();
        if tui_mode && !io::stdin().is_terminal() {
            bail!(
                "stdin is not a terminal, so the TUI cannot read keys. Pass `-p` to answer the \
                 piped input, or `--no-tui` to read prompts from it."
            );
        }
        if self.opens_picker() && !tui_mode {
            bail!(
                "`--resume` without a session opens the picker in the TUI, which needs a terminal \
                 on stdout. Pass the session's ID prefix to resume it directly."
            );
        }
        Ok(tui_mode)
    }

    /// The prompt a `--print` run answers, or `None` without `--print`. Exits with a usage error
    /// when `--print` has neither a prompt argument nor text on stdin.
    async fn headless_prompt(&self) -> Result<Option<String>> {
        if !self.print {
            return Ok(None);
        }
        let Some(prompt) = driver::headless::read_prompt(self.prompt.as_deref()).await? else {
            Self::command()
                .error(
                    ErrorKind::MissingRequiredArgument,
                    "`--print` needs a prompt argument or text on stdin",
                )
                .exit()
        };
        Ok(Some(prompt))
    }

    /// The projects `--list`, `--continue`, `--resume`, and the picker search.
    fn projects(&self) -> ProjectScope {
        if self.all {
            ProjectScope::Every
        } else {
            ProjectScope::Current
        }
    }

    /// The listing `--list` prints, from `--all` and `--archived`.
    fn list_scope(&self) -> ListScope {
        ListScope {
            projects: self.projects(),
            archive: ArchiveState::of(self.archived),
        }
    }

    /// The saved session to continue, or `None` for a new session, which the picker also starts.
    fn session_target(&self) -> Option<SessionTarget<'_>> {
        match &self.resume {
            Some(Some(session)) => Some(SessionTarget::Named(session)),
            _ => self.r#continue.then_some(SessionTarget::Latest),
        }
    }
}

/// Parses a prompt or session, which must hold more than whitespace.
fn non_blank(value: &str) -> Result<String, &'static str> {
    if value.trim().is_empty() {
        return Err("the value is empty");
    }
    Ok(value.to_owned())
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

fn main() -> ExitCode {
    try_main().unwrap_or_else(|e| {
        _ = writeln!(
            io::stderr(),
            "Error: {}",
            util::text::escape_controls(&format!("{e:#}"))
        );
        ExitCode::FAILURE
    })
}

fn try_main() -> Result<ExitCode> {
    let offset_error = util::time::init_local_offset().err();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    let exit = runtime.block_on(async_main(offset_error)).map_err(|e| {
        match e.downcast::<EnginePanic>() {
            Ok(panic) => panic.resume(),
            Err(e) => e,
        }
    })?;
    if matches!(exit, Exit::Signal(_)) {
        // tokio's stdin reader can stay blocked in `read`, and a regular drop waits for it.
        runtime.shutdown_background();
    }
    Ok(exit.code())
}

async fn async_main(offset_error: Option<IndeterminateOffset>) -> Result<Exit> {
    let cli = Cli::parse_valid();

    let tui_mode = cli.tui_mode()?;
    let _log_guard = util::log::init_tracing(tui_mode)?;
    if let Some(e) = offset_error {
        warn!("failed to read the local timezone offset, so times show in UTC: {e}");
    }
    let cwd = env::current_dir().context("failed to read the current working directory")?;
    let project_root = util::git::project_root(&cwd);

    if cli.list {
        let store = SessionStore::open(&cwd, &project_root)?;
        session_list::write(
            &mut io::stdout().lock(),
            &store,
            cli.list_scope(),
            cli.limit,
            terminal::stdout_width(),
        )?;
        return Ok(Exit::Success);
    }

    let headless_prompt = cli.headless_prompt().await?;

    let mut config = Config::load(&cwd, tui_mode).await?;
    if let Some(mode) = cli.mode {
        config.apply_mode_flag(mode);
    }
    let show_thinking = config.show_thinking;
    let theme = tui_mode.then(|| resolve_theme(&config.theme)).transpose()?;

    let store = SessionStore::open(&cwd, &project_root)?;
    let git_branch = util::git::current_branch(&cwd).await;
    let ResumedSession {
        handle,
        transcript,
        title,
        prior,
    } = resolve_session(
        &store,
        config.model.id(),
        git_branch.clone(),
        cli.session_target(),
        cli.projects(),
    )?;
    if prior == ArchiveState::Archived && !tui_mode {
        _ = writeln!(
            io::stderr(),
            "{}",
            archive_notice(handle.session_id(), ArchiveState::Active)
        );
    }
    let picker = cli.opens_picker().then(|| {
        ResumePicker::at_startup(
            store.clone(),
            handle.session_id().to_owned(),
            cli.projects(),
        )
    });

    let engine = Engine::new(
        &config,
        PlatformTrust(config.extra_ca_certs.clone()),
        identity::load_or_create_device_id(),
        SessionParts {
            store,
            handle,
            transcript,
        },
        tui_mode,
    )
    .await?;

    if let Some(prompt) = headless_prompt {
        return Ok(driver::headless::run(engine, show_thinking, &prompt, cli.output).await?);
    }

    let Some(theme) = theme else {
        return driver::repl::run(engine, show_thinking, cli.prompt).await;
    };

    let startup = Startup {
        git_branch,
        picker,
        prompt: cli.prompt,
        title,
        prior,
    };
    // Boxed, since on Windows this future passes clippy's 16 KiB `large_futures` threshold.
    let signal = Box::pin(ox_tui::run(
        engine,
        theme,
        config.snapshot(),
        startup,
        driver::shutdown_signal(),
    ))
    .await?;
    Ok(signal.map_or(Exit::Success, Exit::Signal))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use indoc::formatdoc;

    use super::*;

    mod cli {
        use super::*;

        /// The kind of error `args` fail to parse with, or `None` when they parse.
        fn parse_error(args: &[&str]) -> Option<ErrorKind> {
            Cli::try_parse_from(args).err().map(|error| error.kind())
        }

        #[test]
        fn prompt_is_positional_and_print_is_a_switch() {
            let interactive = Cli::try_parse_from(["ox", "Fix the test"]).unwrap();
            let headless = Cli::try_parse_from(["ox", "-p", "Fix the test"]).unwrap();
            let stdin_only = Cli::try_parse_from(["ox", "--print"]).unwrap();

            assert_eq!(
                (interactive.prompt.as_deref(), interactive.print),
                (Some("Fix the test"), false),
            );
            assert_eq!(
                (headless.prompt.as_deref(), headless.print),
                (Some("Fix the test"), true),
            );
            assert_eq!((stdin_only.prompt, stdin_only.print), (None, true));
        }

        #[test]
        fn prompt_and_session_reject_a_blank_value() {
            for args in [&["ox", ""][..], &["ox", "-p", " \n"], &["ox", "-r", " "]] {
                let error = Cli::try_parse_from(args)
                    .err()
                    .expect("a blank value fails");

                assert_eq!(error.kind(), ErrorKind::ValueValidation, "{args:?}");
                assert!(error.to_string().contains("the value is empty"), "{error}");
            }
        }

        #[test]
        fn mode_takes_a_permission_mode() {
            let cli = Cli::try_parse_from(["ox", "--mode", "plan"]).unwrap();
            let unset = Cli::try_parse_from(["ox"]).unwrap();
            let invalid = Cli::try_parse_from(["ox", "--mode", "bypass"])
                .err()
                .expect("bypass is not a mode");

            assert_eq!(cli.mode, Some(Mode::Plan));
            assert_eq!(unset.mode, None);
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
                "- manual: Ask before tool calls that no permission rule allows or denies",
                "- plan:   Read-only: deny edits and commands",
                "- yolo:   Run tool calls without asking unless a rule denies or asks",
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
            let replacement = char::REPLACEMENT_CHARACTER;

            let invalid = Cli::try_parse_from([OsString::from("ox"), "--mode".into(), value])
                .err()
                .expect("the value names no mode");

            assert_eq!(invalid.kind(), ErrorKind::InvalidValue);
            assert_eq!(
                invalid.to_string(),
                formatdoc! {"
                    error: invalid value '{replacement}' for '--mode <MODE>'
                      [possible values: manual, plan, yolo]

                    For more information, try '--help'.
                "},
            );
        }

        #[test]
        fn output_picks_a_headless_format_and_defaults_to_text() {
            let streamed =
                Cli::try_parse_from(["ox", "-p", "hello", "--output", "stream-json"]).unwrap();
            let default = Cli::try_parse_from(["ox", "-p", "hello"]).unwrap();

            assert_eq!(streamed.output, OutputFormat::StreamJson);
            assert_eq!(default.output, OutputFormat::Text);
            assert_eq!(
                parse_error(&["ox", "--output", "json"]),
                Some(ErrorKind::MissingRequiredArgument),
            );
        }

        #[test]
        fn list_flags_conflict_with_every_session_flag() {
            let session_flags: [&[&str]; 5] = [
                &["-c"],
                &["-r"],
                &["-r", "a1b2"],
                &["hello"],
                &["-p", "hello"],
            ];
            let list_flags: [&[&str]; 3] =
                [&["--list"], &["--list", "--archived"], &["--limit", "3"]];

            for list in list_flags {
                for session in session_flags {
                    let args = [&["ox"][..], list, session].concat();

                    assert_eq!(
                        parse_error(&args),
                        Some(ErrorKind::ArgumentConflict),
                        "{args:?}",
                    );
                }
            }
        }

        #[test]
        fn archived_and_limit_need_list() {
            for args in [&["ox", "--archived"][..], &["ox", "--limit", "3"]] {
                assert_eq!(
                    parse_error(args),
                    Some(ErrorKind::MissingRequiredArgument),
                    "{args:?}"
                );
            }
        }

        #[test]
        fn continue_and_resume_pick_the_session() {
            let latest = Cli::try_parse_from(["ox", "-c"]).unwrap();
            let named = Cli::try_parse_from(["ox", "-r", "a1b2"]).unwrap();
            let picker = Cli::try_parse_from(["ox", "-r", "--all"]).unwrap();

            assert!(latest.r#continue);
            assert_eq!(latest.resume, None);
            assert_eq!(named.resume, Some(Some("a1b2".to_owned())));
            assert_eq!((picker.resume, picker.all), (Some(None), true));
            assert_eq!(
                parse_error(&["ox", "-c", "-r", "a1b2"]),
                Some(ErrorKind::ArgumentConflict),
            );
        }

        mod validate {
            use super::*;

            #[test]
            fn accepts_continue_a_named_resume_or_no_tui_beside_print() {
                for args in [
                    &["ox", "-c", "-p", "next step"][..],
                    &["ox", "-r", "a1b2", "-p", "next step"],
                    &["ox", "--no-tui", "-p", "next step"],
                ] {
                    let cli = Cli::try_parse_from(args).unwrap();

                    let validated = cli.validate();

                    assert!(cli.print, "{args:?}");
                    assert_eq!(validated.err().map(|error| error.kind()), None, "{args:?}");
                }
            }

            #[test]
            fn rejects_the_picker_beside_print_no_tui_or_a_prompt() {
                for (args, other) in [
                    (&["ox", "-r", "-p", "hello"][..], "--print"),
                    (&["ox", "--no-tui", "-r"], "--no-tui"),
                    (&["ox", "hello", "-r"], "[PROMPT]"),
                ] {
                    let cli = Cli::try_parse_from(args).unwrap();

                    let error = cli.validate().expect_err("the picker needs the TUI alone");

                    assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{args:?}");
                    assert!(
                        error.to_string().contains(&format!(
                            "`--resume` without a session opens the picker, \
                             which cannot be used with `{other}`."
                        )),
                        "{error}",
                    );
                }
            }
        }

        mod session_target {
            use super::*;

            #[test]
            fn is_the_latest_for_continue_and_the_named_session_for_resume() {
                let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();

                assert_eq!(
                    parse(&["ox", "-c"]).session_target(),
                    Some(SessionTarget::Latest),
                );
                assert_eq!(
                    parse(&["ox", "-r", "a1b2"]).session_target(),
                    Some(SessionTarget::Named("a1b2")),
                );
                assert_eq!(
                    parse(&["ox", "-r"]).session_target(),
                    None,
                    "the picker starts a new session",
                );
                assert_eq!(parse(&["ox"]).session_target(), None);
            }
        }

        mod projects {
            use super::*;

            #[test]
            fn all_widens_lookups_to_every_project() {
                let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap();

                assert_eq!(parse(&["ox", "-c"]).projects(), ProjectScope::Current);
                assert_eq!(parse(&["ox", "-c", "-a"]).projects(), ProjectScope::Every);
                assert_eq!(
                    parse(&["ox", "-r", "--all"]).projects(),
                    ProjectScope::Every
                );
            }
        }

        mod list_scope {
            use super::*;

            #[test]
            fn archived_lists_the_archive_in_the_current_project() {
                let cli = Cli::try_parse_from(["ox", "--list", "--archived"]).unwrap();

                assert_eq!(
                    cli.list_scope(),
                    ListScope {
                        projects: ProjectScope::Current,
                        archive: ArchiveState::Archived,
                    },
                );
            }
        }
    }
}
