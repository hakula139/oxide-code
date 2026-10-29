//! CLI entry point: config loading, session resolution, mode dispatch.

mod driver;
mod session_list;

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgGroup, CommandFactory as _, Parser, Subcommand};
use indoc::indoc;
use strum::VariantArray as _;
use time::error::IndeterminateOffset;
use tracing::warn;

use ox_core::client::anthropic::identity;
use ox_core::config::Config;
use ox_core::engine::{Engine, EnginePanic, SessionParts};
use ox_core::git;
use ox_core::permission::Mode;
use ox_core::session::display::archive_notice;
use ox_core::session::handle::ResumedSession;
use ox_core::session::resolver::{SessionTarget, export_session, resolve_session};
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::ArchiveState;
use ox_core::session::store::listing::{DEFAULT_SESSION_LIST_LIMIT, ListScope, ProjectScope};
use ox_core::util;
use ox_core::util::path::tildify;
use ox_core::util::tls::PlatformTrust;
use ox_tui::Startup;
use ox_tui::slash::is_built_in;
use ox_tui::terminal;
use ox_tui::theme::{ResolvedTheme, resolve_theme};

use self::driver::{Exit, OutputFormat};

/// The arguments that start or resume a conversation, none of which `--list`, `--archived`, or
/// `--limit` can accompany.
const SESSION_ARGS: &[&str] = &["continue", "print", "prompt", "resume"];

/// How long an exit waits for the runtime's blocking work, which covers arboard's 100 ms X11
/// clipboard handover.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Parser)]
#[command(
    name = "ox",
    version,
    args_conflicts_with_subcommands = true,
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
          ox config check                       Validate the config files and exit

        Setup:
          Sign in   Set ANTHROPIC_API_KEY, or run /login inside ox
          Config    ~/.config/ox/config.toml, checked by `ox config check`

        Guides: https://github.com/hakula139/oxide-code/tree/main/docs/guide"
    }
)]
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
    #[command(subcommand)]
    command: Option<Command>,

    /// Start with this prompt, or with `--print`, answer it.
    #[arg(value_parser = non_blank)]
    prompt: Option<String>,

    /// Answer the prompt and any piped stdin without the TUI, print the reply, and exit.
    #[arg(short, long)]
    print: bool,

    /// With `--print`, print the reply as text, one JSON result, or a JSON line per event.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        default_value_t,
        requires = "print"
    )]
    output: OutputFormat,

    /// Use a bare REPL that reads one prompt per line in place of the TUI.
    #[arg(long)]
    no_tui: bool,

    /// Start in this permission mode, overriding `permission.mode` and `OX_PERMISSION_MODE`.
    #[arg(long, value_name = "MODE", value_parser = ModeParser::new())]
    mode: Option<Mode>,

    /// Continue the most recent session in this project.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    r#continue: bool,

    /// Resume the session with this ID prefix or in this `ox export` file, or pick one from a list
    /// without a value.
    #[expect(
        clippy::option_option,
        reason = "encodes three CLI states: \
                  absent (None), flag only (Some(None)), flag with value (Some(Some))"
    )]
    #[arg(short, long, value_name = "SESSION", num_args = 0..=1, value_parser = non_blank)]
    resume: Option<Option<String>>,

    /// Widen `--list`, `--continue`, and `--resume` from this project to every project.
    #[arg(short, long, requires = "scope")]
    all: bool,

    /// List recent sessions and exit.
    #[arg(
        short,
        long,
        conflicts_with_all = SESSION_ARGS
    )]
    list: bool,

    /// With `--list`, show only archived sessions.
    #[arg(
        long,
        requires = "list",
        conflicts_with_all = SESSION_ARGS
    )]
    archived: bool,

    /// Cap `--list` to the N most-recent sessions, or `0` for no cap.
    #[arg(
        long,
        value_name = "N",
        requires = "list",
        conflicts_with_all = SESSION_ARGS,
        value_parser = limit,
        allow_negative_numbers = true,
        default_value_t = DEFAULT_SESSION_LIST_LIMIT
    )]
    limit: usize,
}

impl Cli {
    /// Parses the command line, exiting with a usage error as clap does for a combination clap
    /// cannot express.
    fn parse_valid() -> Self {
        Self::parse_valid_from(env::args_os()).unwrap_or_else(|e| e.exit())
    }

    /// Parses `args` and validates the combinations clap cannot express, returning any usage error
    /// to the caller.
    fn parse_valid_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let cli = Self::try_parse_from(&args).map_err(unexpected_word)?;
        cli.validate(prompt_after_separator(&args, cli.prompt.as_deref()))?;
        Ok(cli)
    }

    /// Rejects a lone prompt that names a slash command, and the session picker beside a flag that
    /// skips the TUI or a first prompt. A prompt after `--` is never a slash-command word.
    fn validate(&self, prompt_escaped: bool) -> Result<(), clap::Error> {
        self.reject_slash_word(prompt_escaped)?;
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

    /// Rejects a lone prompt such as `ox login`, which users of `git`, `cargo`, and `gh` type
    /// expecting a subcommand. `--` still sends the word as a prompt.
    fn reject_slash_word(&self, prompt_escaped: bool) -> Result<(), clap::Error> {
        let Some(word) = self.prompt.as_deref() else {
            return Ok(());
        };
        if prompt_escaped || !is_built_in(word) {
            return Ok(());
        }
        Err(Self::command().error(
            ErrorKind::ValueValidation,
            format!(
                "`{word}` is a slash command inside `ox`. Run `ox` and type `/{word}`, or pass \
                 `--` before the word to send it as a prompt."
            ),
        ))
    }

    /// Returns whether bare `--resume` asks for the session picker.
    fn opens_picker(&self) -> bool {
        matches!(self.resume, Some(None))
    }

    /// What the run does. A session opens the TUI only with a terminal on stdout, and fails when
    /// the TUI would open with stdin piped, since it reads keys from stdin, or when the session
    /// picker needs the TUI but cannot have it.
    fn run(&self, stdin_is_terminal: bool, stdout_is_terminal: bool) -> Result<Run> {
        match &self.command {
            Some(Command::Config {
                action: ConfigAction::Check,
            }) => return Ok(Run::CheckConfig),
            Some(Command::Export { session }) => return Ok(Run::Export(session.clone())),
            None => {}
        }
        if self.list {
            return Ok(Run::List);
        }
        if self.print {
            return Ok(Run::Session(Frontend::Print));
        }
        if self.no_tui || !stdout_is_terminal {
            if self.opens_picker() {
                bail!(
                    "`--resume` without a session opens the picker in the TUI, which needs a \
                     terminal on stdout. Pass the session's ID prefix to resume it directly."
                );
            }
            return Ok(Run::Session(Frontend::Repl {
                unattended: !stdin_is_terminal,
            }));
        }
        if !stdin_is_terminal {
            bail!(
                "stdin is not a terminal, so the TUI cannot read keys. Pass `-p` to answer the \
                 piped input, or `--no-tui` to read prompts from it."
            );
        }
        Ok(Run::Session(Frontend::Tui {
            picker: self.opens_picker().then(|| self.projects()),
        }))
    }

    /// The prompt a `--print` run answers. Exits with a usage error when there is neither a prompt
    /// argument nor text on stdin.
    async fn print_prompt(&self) -> Result<String> {
        let Some(prompt) = driver::headless::read_prompt(self.prompt.as_deref()).await? else {
            Self::command()
                .error(
                    ErrorKind::MissingRequiredArgument,
                    "`--print` needs a prompt argument or text on stdin",
                )
                .exit()
        };
        Ok(prompt)
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
            archive: self.archived.into(),
        }
    }

    /// The saved session to continue, or `None` for a new session, which the picker also starts.
    /// Fails on a blank `--resume` argument.
    fn session_target(&self) -> Result<Option<SessionTarget>> {
        match &self.resume {
            Some(Some(session)) => SessionTarget::named(session).map(Some),
            _ => Ok(self.r#continue.then_some(SessionTarget::Latest)),
        }
    }
}

/// Reports a stray word after the prompt as an unexpected argument. With
/// `args_conflicts_with_subcommands`, clap blames any such word on a subcommand of that name, even
/// when none exists.
fn unexpected_word(error: clap::Error) -> clap::Error {
    if error.kind() != ErrorKind::ArgumentConflict {
        return error;
    }
    let Some(ContextValue::String(word)) = error.get(ContextKind::InvalidSubcommand) else {
        return error;
    };
    if Cli::command().find_subcommand(word).is_some() {
        return error;
    }
    Cli::command().error(
        ErrorKind::UnknownArgument,
        format!("unexpected argument '{word}' found. Quote a prompt that has spaces."),
    )
}

/// Returns whether the prompt came after a `--`, which makes a slash-command word a prompt.
fn prompt_after_separator(args: &[OsString], prompt: Option<&str>) -> bool {
    let Some(prompt) = prompt else {
        return false;
    };
    let Some(separator) = args.iter().position(|arg| arg == OsStr::new("--")) else {
        return false;
    };
    args[separator + 1..]
        .iter()
        .any(|arg| arg.as_os_str() == OsStr::new(prompt))
}

/// Parses `--limit`, a count of sessions where `0` removes the cap.
fn limit(value: &str) -> Result<usize, &'static str> {
    value
        .parse()
        .map_err(|_| "expected a whole number, or 0 for no cap")
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
        Self(PossibleValuesParser::new(Mode::VARIANTS.iter().map(
            |mode| PossibleValue::new(mode.as_str()).help(mode.description()),
        )))
    }
}

impl TypedValueParser for ModeParser {
    type Value = Mode;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
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

#[derive(Subcommand)]
enum Command {
    /// Inspect the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Print a saved session as JSON lines, which `ox -r <file>` imports on any machine.
    Export {
        /// The session's ID prefix, matched in this project first and then in every project.
        #[arg(value_name = "SESSION", value_parser = non_blank)]
        session: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Validate the config files and environment variables without starting a session.
    Check,
}

/// What a run of `ox` does, resolved once from its arguments and standard streams.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    CheckConfig,
    /// `ox export`, with the ID prefix of the session to print.
    Export(String),
    /// `--list`, which prints and exits before reading stdin.
    List,
    Session(Frontend),
}

/// The frontend a session runs in.
#[derive(Debug, PartialEq, Eq)]
enum Frontend {
    /// `--print`, which answers one prompt and never waits for approval.
    Print,
    /// The bare REPL, which nobody attends when its prompts come from piped stdin.
    Repl { unattended: bool },
    /// The TUI, with the projects the session picker searches when bare `--resume` opens it.
    Tui { picker: Option<ProjectScope> },
}

impl Frontend {
    /// Returns whether nobody can answer an approval request during the session.
    fn unattended(&self) -> bool {
        matches!(self, Self::Print | Self::Repl { unattended: true })
    }
}

/// A frontend with what it reads before the engine starts.
enum Launch {
    Print {
        prompt: String,
    },
    Repl,
    Tui {
        theme: Box<ResolvedTheme>,
        picker: Option<ProjectScope>,
    },
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
    let result = runtime.block_on(async_main(offset_error));
    // A signal exit never returns, so blocking work such as the clipboard handover must finish
    // first. Signals go unheard until ox exits, and a DNS lookup on the blocking pool could hold
    // a plain drop for the resolver's timeout, so every exit bounds the wait.
    runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
    let exit = result.map_err(|e| match e.downcast::<EnginePanic>() {
        Ok(panic) => panic.resume(),
        Err(e) => e,
    })?;
    Ok(exit.finish())
}

async fn async_main(offset_error: Option<IndeterminateOffset>) -> Result<Exit> {
    let cli = Cli::parse_valid();
    match cli.run(io::stdin().is_terminal(), io::stdout().is_terminal())? {
        Run::CheckConfig => check_config(),
        Run::Export(session) => export(&session),
        Run::List => list_sessions(&cli, offset_error),
        Run::Session(frontend) => run_session(cli, frontend, offset_error).await,
    }
}

async fn run_session(
    cli: Cli,
    frontend: Frontend,
    offset_error: Option<IndeterminateOffset>,
) -> Result<Exit> {
    let tui = matches!(frontend, Frontend::Tui { .. });
    let _log_guard = start_logging(tui, offset_error)?;
    let cwd = current_dir()?;
    let project_root = util::path::project_root(&cwd);

    let mut config = Config::load(&cwd)?;
    if let Some(mode) = cli.mode {
        config.apply_mode_flag(mode);
    }
    if frontend.unattended() {
        config.set_unattended();
    }
    let show_thinking = config.show_thinking;
    let launch = match frontend {
        Frontend::Print => Launch::Print {
            prompt: cli.print_prompt().await?,
        },
        Frontend::Repl { .. } => Launch::Repl,
        Frontend::Tui { picker } => Launch::Tui {
            theme: Box::new(resolve_theme(&config.theme)?),
            picker,
        },
    };
    // Before resolving the session, since resuming an archived session unarchives it.
    config.resolve_auth(tui).await?;

    let store = SessionStore::open(&cwd, &project_root)?;
    let git_branch = git::current_branch(&cwd).await;
    let ResumedSession {
        handle,
        transcript,
        title,
        prior,
    } = resolve_session(
        &store,
        config.model.id(),
        git_branch.clone(),
        cli.session_target()?,
        cli.projects(),
    )
    .await?;
    if prior == ArchiveState::Archived && !tui {
        _ = writeln!(
            io::stderr(),
            "{}",
            archive_notice(&handle.session_id(), ArchiveState::Active)
        );
    }

    let generates_titles = tui;
    let engine = Engine::new(
        &config,
        PlatformTrust(config.extra_ca_certs.clone()),
        identity::load_or_create_device_id(),
        SessionParts {
            store,
            handle,
            transcript,
        },
        generates_titles,
    )
    .await?;

    let (theme, picker) = match launch {
        Launch::Print { prompt } => {
            return Ok(driver::headless::run(engine, show_thinking, &prompt, cli.output).await?);
        }
        Launch::Repl => return driver::repl::run(engine, show_thinking, cli.prompt).await,
        Launch::Tui { theme, picker } => (theme, picker),
    };

    let startup = Startup {
        git_branch,
        picker,
        prompt: cli.prompt,
        title,
        prior,
    };
    let mut signals = driver::Signals::new();
    // Boxed, since on Windows this future passes clippy's 16 KiB `large_futures` threshold.
    let signal = Box::pin(ox_tui::run(
        engine,
        *theme,
        config.snapshot(),
        startup,
        signals.recv(),
    ))
    .await?;
    Ok(signal.map_or(Exit::Success, Exit::Signal))
}

/// Runs `ox config check`: loads the config as [`Config::check`] does and resolves the theme. Then
/// it prints the startup and theme notices, each file read, and a verdict.
fn check_config() -> Result<Exit> {
    _ = util::log::init_tracing(false)?;
    let check = Config::check(&current_dir()?)?;
    let theme = resolve_theme(&check.theme)?;

    let mut stderr = io::stderr().lock();
    for notice in check.notices.iter().chain(&theme.notices) {
        writeln!(stderr, "Warning: {}", util::text::escape_controls(notice))?;
    }
    let mut stdout = io::stdout().lock();
    for file in &check.files {
        writeln!(stdout, "Loaded {}", tildify(file))?;
    }
    writeln!(stdout, "Config is valid")?;
    Ok(Exit::Success)
}

fn export(prefix: &str) -> Result<Exit> {
    _ = util::log::init_tracing(false)?;
    let cwd = current_dir()?;
    let store = SessionStore::open(&cwd, &util::path::project_root(&cwd))?;
    export_session(&store, prefix, &mut io::BufWriter::new(io::stdout().lock()))?;
    Ok(Exit::Success)
}

fn list_sessions(cli: &Cli, offset_error: Option<IndeterminateOffset>) -> Result<Exit> {
    let _log_guard = start_logging(false, offset_error)?;
    let cwd = current_dir()?;
    let store = SessionStore::open(&cwd, &util::path::project_root(&cwd))?;
    session_list::write(
        &mut io::stdout().lock(),
        &store,
        cli.list_scope(),
        cli.limit,
        terminal::stdout_width(),
    )?;
    Ok(Exit::Success)
}

/// Starts logging, to the log file under the TUI and to stderr otherwise, and reports a failed
/// local offset lookup. Returns the guard that must live until exit to flush the log file.
fn start_logging(tui: bool, offset_error: Option<IndeterminateOffset>) -> Result<impl Sized> {
    let guard = util::log::init_tracing(tui)?;
    if let Some(e) = offset_error {
        warn!(error = %e, "failed to read the local timezone offset, so times show in UTC");
    }
    Ok(guard)
}

fn current_dir() -> Result<PathBuf> {
    env::current_dir().context("failed to read the current working directory")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use indoc::formatdoc;
    use ox_core::protocol::SessionRef;

    use super::*;

    mod cli {
        use super::*;

        /// The kind of error `args` fail to parse with, or `None` when they parse.
        fn parse_error(args: &[&str]) -> Option<ErrorKind> {
            Cli::try_parse_from(args)
                .map_err(unexpected_word)
                .err()
                .map(|error| error.kind())
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
                "- yolo:   Run tool calls unless a deny rule or your ask rules hold them",
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
        fn list_conflict_names_only_the_flag_the_user_passed() {
            let error = Cli::try_parse_from(["ox", "-l", "-c"])
                .err()
                .expect("`--list` rejects a session flag");

            let message = error.to_string();
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
            assert!(
                message.contains("the argument '--list' cannot be used with '--continue'"),
                "{message}"
            );
            assert!(!message.contains("'--print'"), "{message}");
            assert!(!message.contains("'--resume'"), "{message}");
        }

        #[test]
        fn limit_takes_a_whole_number_including_zero() {
            let capped = Cli::try_parse_from(["ox", "--list", "--limit", "5"]).unwrap();
            let unbounded = Cli::try_parse_from(["ox", "--list", "--limit", "0"]).unwrap();

            assert_eq!((capped.limit, unbounded.limit), (5, 0));
            for bad in ["abc", "-1"] {
                let error = Cli::try_parse_from(["ox", "--list", "--limit", bad])
                    .err()
                    .expect("a non-number fails");

                assert_eq!(error.kind(), ErrorKind::ValueValidation, "{bad}");
                assert!(
                    error.to_string().contains(&format!(
                        "invalid value '{bad}' for '--limit <N>': expected a whole number, \
                         or 0 for no cap"
                    )),
                    "{error}",
                );
            }
        }

        #[test]
        fn a_lone_prompt_that_names_a_slash_command_is_rejected() {
            for (args, word) in [
                (&["ox", "login"][..], "login"),
                (&["ox", "status"], "status"),
                (&["ox", "new"], "new"),
                (&["ox", "-p", "diff"], "diff"),
            ] {
                let Err(error) = Cli::parse_valid_from(args.iter().copied()) else {
                    panic!("a slash-command word must not become a prompt");
                };

                assert_eq!(error.kind(), ErrorKind::ValueValidation, "{args:?}");
                assert!(
                    error.to_string().contains(&format!(
                        "`{word}` is a slash command inside `ox`. Run `ox` and type `/{word}`, \
                         or pass `--` before the word to send it as a prompt."
                    )),
                    "{error}",
                );
            }
        }

        #[test]
        fn a_prompt_that_embeds_a_slash_command_word_still_runs() {
            for prompt in ["login now", "status report", "LOGIN"] {
                let cli = Cli::try_parse_from(["ox", prompt]).unwrap();

                assert_eq!(
                    cli.validate(false).err().map(|error| error.kind()),
                    None,
                    "{prompt}",
                );
            }
        }

        #[test]
        fn a_double_dash_escapes_a_slash_command_word() {
            for (args, word) in [
                (&["ox", "--", "login"][..], "login"),
                (&["ox", "--", "config"], "config"),
                (&["ox", "--", "help"], "help"),
                (&["ox", "-p", "--", "diff"], "diff"),
            ] {
                let cli = Cli::parse_valid_from(args.iter().copied())
                    .expect("a word after `--` is a prompt");

                assert_eq!(cli.prompt.as_deref(), Some(word), "{args:?}");
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

        #[test]
        fn config_check_parses_as_a_subcommand_only_without_session_arguments() {
            let cli = Cli::try_parse_from(["ox", "config", "check"]).unwrap();

            assert!(matches!(
                cli.command,
                Some(Command::Config {
                    action: ConfigAction::Check,
                })
            ));
            assert_eq!(cli.prompt, None);
            assert_eq!(
                parse_error(&["ox", "config"]),
                Some(ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand),
            );
            assert_eq!(
                parse_error(&["ox", "-p", "hi", "config", "check"]),
                Some(ErrorKind::ArgumentConflict),
            );
            assert_eq!(parse_error(&["ox", "help"]), Some(ErrorKind::DisplayHelp));
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

                    let validated = cli.validate(false);

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

                    let error = cli
                        .validate(false)
                        .expect_err("the picker needs the TUI alone");

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

        mod run {
            use super::*;

            /// A case: its arguments, whether stdin and stdout are terminals, and the run or the
            /// message it rejects with.
            type RunCase = (
                &'static [&'static str],
                bool,
                bool,
                Result<Run, &'static str>,
            );

            #[test]
            fn follows_the_flags_and_opens_the_tui_only_with_the_terminals_it_needs() {
                let tui = |picker| Ok(Run::Session(Frontend::Tui { picker }));
                let repl = |unattended| Ok(Run::Session(Frontend::Repl { unattended }));
                let cases: [RunCase; 16] = [
                    (
                        &["ox", "config", "check"],
                        false,
                        false,
                        Ok(Run::CheckConfig),
                    ),
                    (
                        &["ox", "export", "abc"],
                        false,
                        false,
                        Ok(Run::Export("abc".to_owned())),
                    ),
                    (&["ox", "--list"], false, true, Ok(Run::List)),
                    (
                        &["ox", "-p", "hi"],
                        true,
                        true,
                        Ok(Run::Session(Frontend::Print)),
                    ),
                    (
                        &["ox", "-p"],
                        false,
                        false,
                        Ok(Run::Session(Frontend::Print)),
                    ),
                    (&["ox"], true, true, tui(None)),
                    (&["ox", "-c", "hi"], true, true, tui(None)),
                    (&["ox"], false, true, Err("stdin is not a terminal")),
                    (&["ox"], true, false, repl(false)),
                    (&["ox", "--no-tui"], true, true, repl(false)),
                    (&["ox", "--no-tui"], false, true, repl(true)),
                    (
                        &["ox", "--resume"],
                        true,
                        true,
                        tui(Some(ProjectScope::Current)),
                    ),
                    (
                        &["ox", "--resume", "--all"],
                        true,
                        true,
                        tui(Some(ProjectScope::Every)),
                    ),
                    (
                        &["ox", "--resume"],
                        true,
                        false,
                        Err("needs a terminal on stdout"),
                    ),
                    (
                        &["ox", "--resume"],
                        false,
                        true,
                        Err("stdin is not a terminal"),
                    ),
                    (
                        &["ox", "--resume", "--no-tui"],
                        true,
                        true,
                        Err("needs a terminal on stdout"),
                    ),
                ];

                for (args, stdin_is_terminal, stdout_is_terminal, expected) in cases {
                    let cli = Cli::try_parse_from(args).unwrap();

                    let result = cli.run(stdin_is_terminal, stdout_is_terminal);

                    match (result, expected) {
                        (Ok(run), Ok(expected)) => assert_eq!(run, expected, "{args:?}"),
                        (Err(error), Err(message)) => {
                            assert!(error.to_string().contains(message), "{args:?}: {error}");
                        }
                        (result, expected) => {
                            panic!("{args:?}: expected {expected:?}, got {result:?}");
                        }
                    }
                }
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

        mod session_target {
            use super::*;

            #[test]
            fn is_the_latest_for_continue_and_the_named_session_for_resume() {
                let target =
                    |args: &[&str]| Cli::try_parse_from(args).unwrap().session_target().unwrap();

                assert_eq!(target(&["ox", "-c"]), Some(SessionTarget::Latest));
                assert_eq!(
                    target(&["ox", "-r", "a1b2"]),
                    Some(SessionTarget::Ref(SessionRef::Prefix("a1b2".to_owned()))),
                );
                assert_eq!(
                    target(&["ox", "-r"]),
                    None,
                    "the picker starts a new session",
                );
                assert_eq!(target(&["ox"]), None);
            }
        }
    }

    mod unexpected_word {
        use super::*;

        #[test]
        fn a_stray_word_after_the_prompt_is_an_unexpected_argument() {
            let error = Cli::try_parse_from(["ox", "-p", "fix", "the", "bug"])
                .map_err(unexpected_word)
                .err()
                .expect("an unquoted prompt fails");

            assert_eq!(error.kind(), ErrorKind::UnknownArgument);
            assert!(
                error.to_string().starts_with(
                    "error: unexpected argument 'the' found. Quote a prompt that has spaces.\n"
                ),
                "{error}",
            );
        }
    }

    mod frontend {
        use super::*;

        mod unattended {
            use super::*;

            #[test]
            fn holds_for_print_and_a_repl_on_piped_stdin() {
                for (frontend, expected) in [
                    (Frontend::Print, true),
                    (Frontend::Repl { unattended: true }, true),
                    (Frontend::Repl { unattended: false }, false),
                    (Frontend::Tui { picker: None }, false),
                ] {
                    assert_eq!(frontend.unattended(), expected, "{frontend:?}");
                }
            }
        }
    }
}
