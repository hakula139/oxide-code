//! Bare REPL driver for `--no-tui`: prompts and slash commands from stdin, replies on stdout.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::pin;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};

use ox_core::command::{self, Parsed};
use ox_core::engine::{Engine, EngineHandle, TurnEnd};
use ox_core::permission::rule::Rule;
use ox_core::permission::{Basis, SessionGrant};
use ox_core::protocol::notice::ENGINE_STOPPED;
use ox_core::protocol::{Decision, Event, EventSink, Op};
use ox_core::tool::view::{ApprovalBody, ApprovalPreview};
use ox_core::util::git;
use ox_core::util::text::escape_hidden;

use super::stdio::{Approver, StdioSink, Unattended, render_op, render_queued};
use super::{Exit, Shutdown, shutdown_signal};

/// Reads prompts and slash commands until EOF or a signal, then finalizes the session. EOF after
/// an operation failed or a command was rejected exits with failure. Only a terminal on stdin
/// answers approvals, so a piped line is never taken as an answer. With a terminal, SIGINT
/// interrupts the running operation or discards the line at the prompt, as shells do. Every other
/// signal ends the run. `first`, when given, runs as the first input line.
pub(crate) async fn run(
    engine: Engine,
    show_thinking: bool,
    drifted: &[PathBuf],
    first: Option<String>,
) -> Result<Exit> {
    let sink = StdioSink::new(show_thinking);
    let attended = io::stdin().is_terminal();
    let stdin = BufReader::new(tokio::io::stdin());
    let project_root = git::project_root(engine.cwd());
    let handle = engine.spawn(drifted);
    let signal = async || {
        let caught = shutdown_signal().await;
        _ = sink.break_line(&mut io::stdout(), &mut io::stderr());
        caught
    };
    drive(handle, &project_root, &sink, first, stdin, attended, signal).await
}

/// Runs the REPL over `first`, then the lines of `input`, where each `signal` call waits for the
/// next signal.
async fn drive<R>(
    mut handle: EngineHandle,
    project_root: &Path,
    sink: &dyn EventSink,
    mut first: Option<String>,
    input: R,
    attended: bool,
    mut signal: impl AsyncFnMut() -> Shutdown,
) -> Result<Exit>
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = input.lines();
    let mut stopped = None;
    let mut failed = false;

    let result: Result<()> = async {
        'repl: loop {
            render_queued(&mut handle.events, sink);
            let line = if let Some(line) = first.take() {
                line
            } else {
                _ = write!(io::stderr(), "> ");
                _ = io::stderr().flush();
                let line = tokio::select! {
                    line = lines.next_line() => line?,
                    caught = signal() => {
                        if attended && caught == Shutdown::Interrupt {
                            continue;
                        }
                        stopped = Some(caught);
                        break;
                    }
                };
                let Some(line) = line else {
                    break;
                };
                line
            };

            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            let op = match command::parse_slash(input) {
                Some(parsed) => match command_op(&parsed, project_root) {
                    Ok(op) => op,
                    Err(message) => {
                        sink.emit(Event::Error(message), "slash-command-rejected");
                        failed = true;
                        continue;
                    }
                },
                None => Op::Prompt(command::unescape_literal(input).to_owned()),
            };

            // A send fails only once the engine has stopped, which `render_op` then reports.
            _ = handle.ops.send(op.clone()).await;
            let mut prompter = Prompter {
                lines: &mut lines,
                out: io::stderr(),
                attended,
            };
            let mut rendered = pin!(render_op(
                &mut handle.events,
                sink,
                &handle.ops,
                &op,
                &mut prompter
            ));
            let end = loop {
                tokio::select! {
                    end = &mut rendered => break end,
                    caught = signal() => {
                        if !(attended && caught == Shutdown::Interrupt) {
                            stopped = Some(caught);
                            break 'repl;
                        }
                        // A send fails only once the engine has stopped, which `rendered` reports.
                        _ = handle.ops.send(Op::Interrupt).await;
                    }
                }
            };
            match end {
                Some(TurnEnd::Failed) => failed = true,
                Some(_) => {}
                None => bail!(ENGINE_STOPPED),
            }
        }
        Ok(())
    }
    .await;

    let failure = handle.shutdown().await?;
    sink.session_write_error(failure.as_deref());
    result?;
    Ok(match stopped {
        Some(signal) => Exit::Signal(signal),
        None if failed => Exit::Failure,
        None => Exit::Success,
    })
}

/// The op a slash command runs in the REPL at `project_root`, or the message saying why it cannot
/// run here.
fn command_op(parsed: &Parsed, project_root: &Path) -> Result<Op, String> {
    command::typed_op(parsed, project_root).unwrap_or_else(|| {
        let names: Vec<String> = command::NAMES
            .iter()
            .map(|name| format!("/{name}"))
            .collect();
        Err(format!(
            "/{name} is not a REPL command. \
             Available: {names}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            names = names.join(", "),
        ))
    })
}

// ── Prompter ──

const CHOICES: &str = "[y] approve  [n] deny  [f] feedback  [x] abort: ";
const GRANT_CHOICES: &str =
    "[y] approve  [a] allow for session  [n] deny  [f] feedback  [x] abort: ";

/// Answers approvals with lines read from the REPL's input, writing the call and its choices to
/// `out`. The input ending aborts the turn. Unless `attended`, it denies as [`Unattended`] does.
struct Prompter<'a, R, W> {
    lines: &'a mut Lines<R>,
    out: W,
    attended: bool,
}

impl<R: AsyncBufRead + Unpin, W: Write> Approver for Prompter<'_, R, W> {
    async fn decide(
        &mut self,
        preview: &ApprovalPreview,
        basis: &Basis,
        grant: Option<&SessionGrant>,
    ) -> Decision {
        if !self.attended {
            return Unattended.decide(preview, basis, grant).await;
        }
        _ = write_preview(&mut self.out, preview, basis);
        let choices = if grant.is_some() {
            GRANT_CHOICES
        } else {
            CHOICES
        };
        loop {
            let Some(answer) = self.ask(choices).await else {
                return Decision::Abort;
            };
            match (answer.to_ascii_lowercase().as_str(), grant) {
                ("y" | "yes", _) => return Decision::Approve,
                ("a", Some(grant)) => match self.rule(grant).await {
                    Some(Ok(rule)) => {
                        return Decision::AllowForSession {
                            rule: Box::new(rule),
                        };
                    }
                    Some(Err(e)) => _ = writeln!(self.out, "Rule rejected: {e:#}"),
                    None => return Decision::Abort,
                },
                ("n" | "no", _) => return Decision::Deny,
                ("f", _) => match self.ask("Feedback for the model: ").await {
                    Some(feedback) if !feedback.is_empty() => {
                        return Decision::DenyWithFeedback { feedback };
                    }
                    Some(_) => {}
                    None => return Decision::Abort,
                },
                ("x", _) => return Decision::Abort,
                _ => _ = writeln!(self.out, "Answer with one of the letters in brackets"),
            }
        }
    }
}

impl<R: AsyncBufRead + Unpin, W: Write> Prompter<'_, R, W> {
    /// Writes `prompt` and reads the trimmed answer, or `None` once the input ends or fails.
    async fn ask(&mut self, prompt: &str) -> Option<String> {
        _ = write!(self.out, "{prompt}");
        _ = self.out.flush();
        match self.lines.next_line().await {
            Ok(Some(line)) => Some(line.trim().to_owned()),
            Ok(None) => {
                _ = writeln!(self.out);
                None
            }
            Err(e) => {
                _ = writeln!(self.out, "\nCould not read the answer: {e}");
                None
            }
        }
    }

    /// Reads the pattern of the session rule to grant, where an empty answer takes the suggested
    /// one, or `None` once the input ends. An error says why the grant does not admit the rule.
    async fn rule(&mut self, grant: &SessionGrant) -> Option<Result<Rule>> {
        let suggested = grant.suggested().pattern().to_owned();
        let answer = self
            .ask(&format!("Pattern to allow [{suggested}]: "))
            .await?;
        let text = if answer.is_empty() {
            &suggested
        } else {
            &answer
        };
        Some(grant.accept(text))
    }
}

/// Writes the call `preview` shows, one line per command line or diff row, then why `basis` made
/// it ask when a rule holds it.
fn write_preview(out: &mut impl Write, preview: &ApprovalPreview, basis: &Basis) -> io::Result<()> {
    writeln!(out, "{} needs approval:", escape_hidden(&preview.title))?;
    match &preview.body {
        ApprovalBody::Command(command) => {
            for line in command.lines() {
                writeln!(out, "  {}", escape_hidden(line))?;
            }
        }
        ApprovalBody::Diff(chunk) => {
            for line in &chunk.old {
                writeln!(out, "  - {}", escape_hidden(&line.text))?;
            }
            for line in &chunk.new {
                writeln!(out, "  + {}", escape_hidden(&line.text))?;
            }
        }
    }
    if let Some(reason) = basis.reason() {
        writeln!(out, "{}", escape_hidden(&reason))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use indoc::{formatdoc, indoc};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ox_core::engine::EnginePanic;
    use ox_core::engine::testing::{
        engine, mount_reply, mount_stalled_reply_once, mount_tool_call_once, panic_message,
        panicked_engine, request_bodies, until_requested, with_pinned_home,
    };
    use ox_core::permission::testing::exec_grant;
    use ox_core::permission::{Mode, PermissionClass, Tier};
    use ox_core::protocol::testing::CapturingSink;
    use ox_core::session::store::testing::{saved_message_count, test_store};
    use ox_core::tool::testing::{bash_preview, bash_registry};
    use ox_core::tool::view::{DiffChunk, DiffLine};

    use super::*;

    mod drive {
        use super::*;

        async fn no_signal() -> Shutdown {
            std::future::pending().await
        }

        #[tokio::test]
        async fn skips_blank_lines_and_exits_cleanly_at_eof() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let input: &[u8] = b"\n   \nhello\n";

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &sink,
                    None,
                    input,
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1, "blank lines never reach the model");
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "hello");
            })
            .await;
        }

        #[tokio::test]
        async fn runs_the_first_prompt_before_reading_input() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let first = Some("Explain\nthis".to_owned());
                let input: &[u8] = b"second\n";

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &CapturingSink::default(),
                    first,
                    input,
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let prompts: Vec<serde_json::Value> = request_bodies(&server)
                    .await
                    .iter()
                    .map(|request| {
                        let last = request["messages"].as_array().unwrap().last().unwrap();
                        last["content"][0]["text"].clone()
                    })
                    .collect();
                assert_eq!(prompts, ["Explain\nthis", "second"]);
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_reading_after_a_failed_turn_and_exits_with_failure() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(400))
                    .up_to_n_times(1)
                    .mount(&server)
                    .await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id().to_owned();
                let sink = CapturingSink::default();
                let input: &[u8] = indoc! {b"
                    first
                    /resume zzzz
                    //literal
                "};

                let exit = Box::pin(drive(
                    engine.spawn(&[PathBuf::from("drifted.rs")]),
                    dir.path(),
                    &sink,
                    None,
                    input,
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 2, "/resume must not reach the model");
                let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "/literal");
                let reports: Vec<String> = sink
                    .events()
                    .into_iter()
                    .filter_map(|event| match event {
                        Event::Notice(msg) | Event::Error(msg) | Event::TurnFailed(msg) => {
                            Some(msg)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(reports.len(), 3, "{reports:?}");
                assert!(reports[0].ends_with(": drifted.rs"), "{reports:?}");
                assert!(reports[1].contains("HTTP 400"), "{reports:?}");
                assert_eq!(
                    reports[2],
                    format!(
                        "Resume failed (still on session {}): no session matching `zzzz`",
                        &session_id[..8],
                    ),
                );
                assert!(matches!(sink.events().last(), Some(Event::TurnComplete)));
                assert_eq!(
                    saved_message_count(&store, &session_id),
                    3,
                    "the failed prompt, the literal, and its reply are saved",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn runs_each_typed_command_as_its_op() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let first_id = engine.session_id().to_owned();
                let sink = CapturingSink::default();
                let log = (1..=150)
                    .map(|n| format!("request {n} served in 12 ms."))
                    .collect::<Vec<_>>()
                    .join(" ");
                let prefix = &first_id[..8];
                let input = formatdoc! {"
                    Why does the login service log stop here? {log}
                    second
                    /compact focus on login
                    /rename Fix login
                    /model sonnet
                    /effort low
                    /clear
                    /resume {prefix}
                "};

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &sink,
                    None,
                    input.as_bytes(),
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                assert_eq!(
                    requests.len(),
                    3,
                    "only the prompts and the summary reach the model"
                );
                assert!(
                    requests[2].to_string().contains("focus on login"),
                    "{}",
                    requests[2]
                );
                let confirmations: Vec<String> = sink
                    .events()
                    .into_iter()
                    .filter_map(|event| match event {
                        Event::SessionCompacted { instructions, .. } => {
                            Some(format!("compacted with {instructions:?}"))
                        }
                        Event::SessionTitleUpdated { title, .. } => {
                            Some(format!("renamed to {title}"))
                        }
                        Event::ConfigChanged { notice, .. } => Some(notice),
                        Event::SessionRolled { .. } => Some("rolled".to_owned()),
                        Event::SessionResumed { id, title, .. } => {
                            Some(format!("resumed {id} titled {title:?}"))
                        }
                        _ => None,
                    })
                    .collect();
                let resumed = format!("resumed {first_id} titled Some(\"Fix login\")");
                assert_eq!(
                    confirmations,
                    [
                        "compacted with Some(\"focus on login\")",
                        "renamed to Fix login",
                        "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high (model default)",
                        "Effort set to low",
                        "rolled",
                        resumed.as_str(),
                    ],
                );
            })
            .await;
        }

        #[tokio::test]
        async fn runs_init_as_the_update_prompt_when_instruction_files_exist() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                fs::write(dir.path().join("AGENTS.md"), "# AGENTS.md\n").unwrap();
                let input: &[u8] = b"/init\n";

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &CapturingSink::default(),
                    None,
                    input,
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let Some(Ok(Op::Prompt(prompt))) = command::parse("init", "", dir.path()) else {
                    panic!("bare /init must resolve once AGENTS.md exists");
                };
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1);
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], prompt);
            })
            .await;
        }

        #[tokio::test]
        async fn init_without_an_instruction_file_runs_only_with_a_file_name() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let input: &[u8] = indoc! {b"
                    /init
                    /init CLAUDE.md
                "};

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &sink,
                    None,
                    input,
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                let errors: Vec<String> = sink
                    .events()
                    .into_iter()
                    .filter_map(|event| match event {
                        Event::Error(msg) => Some(msg),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    errors,
                    ["/init: This project has no AGENTS.md or CLAUDE.md. \
                      Run `/init AGENTS.md` or `/init CLAUDE.md` to create one."],
                );
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1, "the bare /init must not reach the model");
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(
                    last["content"][0]["text"],
                    command::init::create_prompt(dir.path(), "CLAUDE.md"),
                );
            })
            .await;
        }

        #[tokio::test]
        async fn rejects_a_command_it_cannot_run_and_exits_with_failure() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let input: &[u8] = indoc! {b"
                    /model gpt-4
                    /model
                    /help
                "};

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &sink,
                    None,
                    input,
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                assert_eq!(request_bodies(&server).await, [] as [serde_json::Value; 0]);
                let reports: Vec<String> = sink
                    .events()
                    .into_iter()
                    .filter_map(|event| match event {
                        Event::Error(msg) => Some(msg),
                        _ => None,
                    })
                    .collect();
                assert_eq!(reports.len(), 3, "{reports:?}");
                assert!(
                    reports[0].starts_with("/model: Unknown model: `gpt-4`."),
                    "{reports:?}"
                );
                assert_eq!(
                    reports[1],
                    "/model: Missing model ID. Usage: `/model <id>`."
                );
                assert_eq!(
                    reports[2],
                    "/help is not a REPL command. \
                     Available: /clear, /compact, /effort, /init, /mode, /model, /rename, /resume. \
                     Use //help to send the literal text.",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn answers_an_approval_from_the_next_input_line() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#).await;
                mount_reply(&server, "Done.").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(bash_registry(Vec::new()));
                let input = indoc! {"
                    list files
                    f
                    Use git ls-files.
                "};

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &CapturingSink::default(),
                    None,
                    input.as_bytes(),
                    true,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                let result =
                    &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
                assert_eq!(result["type"], "tool_result");
                assert_eq!(
                    result["content"],
                    "The bash call was denied with this feedback: Use git ls-files.",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn without_a_terminal_denies_approvals_and_keeps_every_line_a_prompt() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_tool_call_once(&server, "call-1", "bash", r#"{"command":"git push"}"#).await;
                mount_reply(&server, "Done.").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(bash_registry(Vec::new()));
                let input = indoc! {"
                    list files
                    y
                "};

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &CapturingSink::default(),
                    None,
                    input.as_bytes(),
                    false,
                    no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 3, "the `y` line is the second prompt");
                let result =
                    &requests[1]["messages"].as_array().unwrap().last().unwrap()["content"][0];
                let content = result["content"].as_str().unwrap();
                assert!(
                    content.contains("This run cannot ask for approval."),
                    "{content}"
                );
                let last = requests[2]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "y");
            })
            .await;
        }

        #[tokio::test]
        async fn sigint_from_a_terminal_interrupts_the_turn_and_keeps_reading() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let store = test_store(dir.path());
                let engine = engine(server.uri(), &store, dir.path()).await;
                let session_id = engine.session_id().to_owned();
                let sink = CapturingSink::default();
                let input: &[u8] = indoc! {b"
                    first
                    second
                "};
                let mut interrupted = false;
                let signal = async || {
                    if !interrupted {
                        until_requested(&server, 1).await;
                        interrupted = true;
                        return Shutdown::Interrupt;
                    }
                    std::future::pending().await
                };

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &sink,
                    None,
                    input,
                    true,
                    signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let ends: Vec<Event> = sink
                    .events()
                    .into_iter()
                    .filter(|event| matches!(event, Event::Cancelled | Event::TurnComplete))
                    .collect();
                assert_eq!(ends, [Event::Cancelled, Event::TurnComplete]);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 2, "{requests:?}");
                let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "second");
                assert_eq!(saved_message_count(&store, &session_id), 4);
            })
            .await;
        }

        #[tokio::test]
        async fn sigint_at_a_terminal_prompt_keeps_reading() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let (writer, reader) = tokio::io::duplex(64);
                let mut writer = Some(writer);
                let signal = async || {
                    let Some(mut writer) = writer.take() else {
                        return std::future::pending().await;
                    };
                    tokio::io::AsyncWriteExt::write_all(&mut writer, b"hello\n")
                        .await
                        .unwrap();
                    Shutdown::Interrupt
                };

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &CapturingSink::default(),
                    None,
                    BufReader::new(reader),
                    true,
                    signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let requests = request_bodies(&server).await;
                assert_eq!(requests.len(), 1, "{requests:?}");
                let last = requests[0]["messages"].as_array().unwrap().last().unwrap();
                assert_eq!(last["content"][0]["text"], "hello");
            })
            .await;
        }

        #[tokio::test]
        async fn sigint_with_piped_input_ends_the_run() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_stalled_reply_once(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let input: &[u8] = b"first\n";
                let mut interrupted = false;
                let signal = async || {
                    if interrupted {
                        return std::future::pending().await;
                    }
                    until_requested(&server, 1).await;
                    interrupted = true;
                    Shutdown::Interrupt
                };

                let exit = Box::pin(drive(
                    engine.spawn(&[]),
                    dir.path(),
                    &CapturingSink::default(),
                    None,
                    input,
                    false,
                    signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Signal(Shutdown::Interrupt));
                assert_eq!(request_bodies(&server).await.len(), 1);
            })
            .await;
        }

        #[tokio::test]
        async fn hands_back_the_panic_of_an_engine_that_stops() {
            let input: &[u8] = b"hello\n";

            let result = Box::pin(drive(
                panicked_engine(),
                Path::new("/repo"),
                &CapturingSink::default(),
                None,
                input,
                true,
                no_signal,
            ))
            .await;

            let panic = result.unwrap_err().downcast::<EnginePanic>().unwrap();
            assert_eq!(panic_message(panic), "engine bug");
        }
    }

    mod prompter {
        use super::*;

        mod decide {
            use super::*;

            /// Runs one `decide` over `input`, returning the decision and what the prompter wrote.
            async fn decide(input: &str, grant: Option<&SessionGrant>) -> (Decision, String) {
                let mut lines = input.as_bytes().lines();
                let mut prompter = Prompter {
                    lines: &mut lines,
                    out: Vec::new(),
                    attended: true,
                };
                let decision = prompter
                    .decide(
                        &bash_preview("cargo test"),
                        &Basis::Mode(Mode::Manual),
                        grant,
                    )
                    .await;
                (decision, String::from_utf8(prompter.out).unwrap())
            }

            #[tokio::test]
            async fn maps_each_answer_to_its_decision() {
                for (input, decision) in [
                    ("y\n", Decision::Approve),
                    (" YES \n", Decision::Approve),
                    ("n\n", Decision::Deny),
                    (
                        "f\nRun the unit tests only.\n",
                        Decision::DenyWithFeedback {
                            feedback: "Run the unit tests only.".to_owned(),
                        },
                    ),
                    ("x\n", Decision::Abort),
                    ("f\n", Decision::Abort),
                    ("", Decision::Abort),
                ] {
                    let (got, out) = decide(input, None).await;
                    assert_eq!(got, decision, "{input:?}");
                    assert!(
                        out.starts_with("Bash needs approval:\n  cargo test\n"),
                        "{out}"
                    );
                    assert!(out.contains(CHOICES), "{out}");
                }
            }

            #[tokio::test]
            async fn asks_again_after_an_unknown_answer_or_empty_feedback() {
                let input = indoc! {"
                    a
                    f

                    maybe
                    run the tests
                    n
                "};
                let (decision, out) = decide(input, None).await;

                assert_eq!(decision, Decision::Deny);
                assert_eq!(out.matches(CHOICES).count(), 5, "{out}");
                assert_eq!(
                    out.matches("Answer with one of the letters").count(),
                    3,
                    "a prompt typed ahead re-asks: {out}"
                );
            }

            #[tokio::test]
            async fn allows_the_suggested_or_an_edited_rule_for_the_session() {
                let grant = exec_grant("cargo test");
                let allowed = |pattern: &str| Decision::AllowForSession {
                    rule: Box::new(Rule::parse(PermissionClass::Exec, pattern).unwrap()),
                };

                let (decision, out) = decide("a\n\n", Some(&grant)).await;
                assert_eq!(decision, allowed("cargo test"));
                assert!(out.contains(GRANT_CHOICES), "{out}");
                assert!(out.contains("Pattern to allow [cargo test]: "), "{out}");

                let input = indoc! {"
                    a
                    cargo build
                    a
                    cargo test *
                "};
                let (decision, out) = decide(input, Some(&grant)).await;
                assert_eq!(decision, allowed("cargo test *"));
                let (rejected, retried) = out
                    .split_once("the pattern no longer covers this call")
                    .unwrap();
                assert_eq!(rejected.matches(GRANT_CHOICES).count(), 1, "{out}");
                assert_eq!(
                    retried.matches(GRANT_CHOICES).count(),
                    1,
                    "a rejected rule returns to the choices: {out}"
                );

                let (decision, _) = decide("a\n", Some(&grant)).await;
                assert_eq!(
                    decision,
                    Decision::Abort,
                    "the input ending mid-rule aborts"
                );
            }

            #[tokio::test]
            async fn aborts_saying_why_when_the_input_cannot_be_read() {
                let mut lines = (&b"\xff\n"[..]).lines();
                let mut prompter = Prompter {
                    lines: &mut lines,
                    out: Vec::new(),
                    attended: true,
                };

                let decision = prompter
                    .decide(&bash_preview("ls"), &Basis::Mode(Mode::Manual), None)
                    .await;

                assert_eq!(decision, Decision::Abort);
                let out = String::from_utf8(prompter.out).unwrap();
                assert!(out.contains("Could not read the answer: "), "{out}");
            }
        }
    }

    mod write_preview {
        use super::*;

        #[test]
        fn lists_diff_rows_then_the_rule_and_escapes_hidden_characters() {
            let basis = Basis::Rule {
                tier: Tier::Ask,
                rule: Rule::parse(PermissionClass::Edit, "src/\u{200b}*.rs").unwrap(),
            };
            let mut out = Vec::new();

            write_preview(
                &mut out,
                &ApprovalPreview {
                    title: "Edit(src/a.rs)".to_owned(),
                    body: ApprovalBody::Diff(DiffChunk {
                        old: vec![DiffLine {
                            number: 3,
                            text: "let a = 1;".to_owned(),
                        }],
                        new: vec![DiffLine {
                            number: 3,
                            text: "let a = 2;\u{1b}[2K\u{200b}".to_owned(),
                        }],
                    }),
                },
                &basis,
            )
            .unwrap();

            assert_eq!(
                String::from_utf8(out).unwrap(),
                indoc! {r"
                    Edit(src/a.rs) needs approval:
                      - let a = 1;
                      + let a = 2;\u{1b}[2K\u{200b}
                    Held by the `ask.edit` pattern `src/\u{200b}*.rs`
                "},
            );
        }
    }
}
