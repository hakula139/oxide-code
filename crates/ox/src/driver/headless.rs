//! Headless driver for `--print`: one turn, printed to stdout as text or JSON.

mod wire;

use std::io::{self, IsTerminal as _, Write};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::ValueEnum;
use indoc::formatdoc;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::mpsc;

use ox_core::command;
use ox_core::engine::{Engine, EngineHandle, EnginePanic};
use ox_core::protocol::TurnEnd;
use ox_core::protocol::notice::ENGINE_STOPPED;
use ox_core::protocol::{Event, EventSink, Op, UsageSnapshot};
use ox_core::util::sync::lock_or_recover;

use super::stdio::{Renderer, StdioSink, Unattended, render_op};
use super::{Exit, decode_input, shutdown_signal};

/// How long [`read_prompt`] waits on an open stdin before saying what it waits for.
const STDIN_NOTICE_DELAY: Duration = Duration::from_secs(3);

/// Runs `prompt` as a single turn, printed in `format`. An incomplete turn or failed output exits
/// with failure, and a signal exits with its own status. If the engine panics, the panic is returned
/// after the failed result is printed.
pub(crate) async fn run(
    engine: Engine,
    show_thinking: bool,
    prompt: &str,
    format: OutputFormat,
) -> Result<Exit, EnginePanic> {
    let session_id = engine.session_id().to_owned();
    let printer = Printer::new(
        format,
        show_thinking,
        session_id,
        io::stdout(),
        io::stderr(),
    );
    drive(engine.spawn(), &printer, prompt).await
}

/// The prompt a `--print` run answers: `prompt` with stdin attached when stdin is not a terminal,
/// or `None` when neither holds text.
pub(crate) async fn read_prompt(prompt: Option<&str>) -> Result<Option<String>> {
    let stdin = if io::stdin().is_terminal() {
        None
    } else {
        let text = read_input(tokio::io::stdin(), &mut io::stderr(), STDIN_NOTICE_DELAY)
            .await
            .context("failed to read the prompt from stdin")?;
        Some(text)
    };
    Ok(compose_prompt(prompt, stdin.as_deref()))
}

/// Reads `input` to its end. If `input` stays open past `delay`, as a pipe that a parent program
/// never closes does, it writes one notice to `notice` and keeps reading.
async fn read_input(
    mut input: impl AsyncRead + Unpin,
    notice: &mut impl Write,
    delay: Duration,
) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut read = pin!(input.read_to_end(&mut bytes));
    tokio::select! {
        read = &mut read => read?,
        () = tokio::time::sleep(delay) => {
            _ = writeln!(
                notice,
                "Waiting for stdin to close, since `--print` reads it as input. \
                 Redirect stdin from /dev/null to skip it."
            );
            read.await?
        }
    };
    Ok(decode_input(bytes))
}

/// The prompt a headless run sends: `prompt` followed by `stdin` in a `<stdin>` block, or whichever
/// of the two is given. Blank stdin counts as absent, so `None` means neither holds text.
fn compose_prompt(prompt: Option<&str>, stdin: Option<&str>) -> Option<String> {
    let stdin = stdin
        .filter(|text| !text.trim().is_empty())
        .map(|text| text.trim_end_matches(['\r', '\n']));
    match (prompt, stdin) {
        (Some(prompt), Some(stdin)) => Some(formatdoc! {"
            {prompt}

            <stdin>
            {stdin}
            </stdin>"
        }),
        (Some(text), None) | (None, Some(text)) => Some(text.to_owned()),
        (None, None) => None,
    }
}

async fn drive<O, E>(
    mut handle: EngineHandle,
    printer: &Printer<O, E>,
    prompt: &str,
) -> Result<Exit, EnginePanic>
where
    O: Write + Send,
    E: Write + Send,
{
    let prompt = Op::prompt(command::unescape_literal(prompt));
    let sink = InterruptOnClosedStdout::new(printer, &handle.ops);
    let renderer = Renderer::new(&sink);
    // A send fails only once the engine has stopped, which `render_op` then reports.
    _ = handle.ops.send(prompt.clone()).await;
    let operation = render_op(
        &mut handle.events,
        &renderer,
        &handle.ops,
        &prompt,
        None::<&mut Unattended>,
    );
    let (end, mut exit) = tokio::select! {
        end = operation => match end {
            Some(TurnEnd::Completed) => (TurnEnd::Completed, Exit::Success),
            Some(end) => (end, Exit::Failure),
            None => {
                renderer.render(Event::TurnFailed(ENGINE_STOPPED.to_owned()));
                (TurnEnd::Failed, Exit::Failure)
            }
        },
        signal = shutdown_signal() => {
            _ = printer.break_line();
            (TurnEnd::Shutdown, Exit::Signal(signal))
        }
    };
    let finished = handle.shutdown().await;
    let session_failure = finished.as_ref().ok().and_then(Option::as_deref);
    if let Some(failure) = session_failure {
        renderer.session_write_error(Some(failure));
        printer.record_session_write_error(failure);
    }
    let output = renderer.finish().and(printer.finish(end));
    if let Err(error) = &output {
        printer.report_failure(error);
    }
    if (output.is_err() || session_failure.is_some()) && !matches!(exit, Exit::Signal(_)) {
        exit = Exit::Failure;
    }
    finished.map(|_| exit)
}

/// [`EventSink`] that interrupts the turn once stdout's reader goes away, since nothing will read
/// the rest. The session still finalizes, as it does for an interrupt.
struct InterruptOnClosedStdout<'a, O, E> {
    printer: &'a Printer<O, E>,
    /// Weak, so shutting the engine down still closes the channel while the sink outlives it.
    ops: mpsc::WeakSender<Op>,
    /// Whether an interrupt was sent, so later broken-pipe writes do not repeat it.
    interrupted: AtomicBool,
}

impl<'a, O: Write + Send, E: Write + Send> InterruptOnClosedStdout<'a, O, E> {
    fn new(printer: &'a Printer<O, E>, ops: &mpsc::Sender<Op>) -> Self {
        Self {
            printer,
            ops: ops.downgrade(),
            interrupted: AtomicBool::new(false),
        }
    }
}

impl<O: Write + Send, E: Write + Send> EventSink for InterruptOnClosedStdout<'_, O, E> {
    fn send(&self, event: Event) -> Result<()> {
        let printed = self.printer.print(event);
        if let Printed::Stdout(error) = &printed
            && is_broken_pipe(error)
            && !self.interrupted.load(Ordering::Relaxed)
            && let Some(ops) = self.ops.upgrade()
            && ops.try_send(Op::Interrupt).is_ok()
        {
            self.interrupted.store(true, Ordering::Relaxed);
        }
        printed.into_result()
    }
}

/// Whether `error` is a write that failed because stdout's reader went away.
fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
}

// ── Output Format ──

/// What a headless run prints on stdout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// The reply as plain text.
    #[default]
    Text,
    /// One JSON result once the run ends.
    Json,
    /// Each event as a JSON line, then the result.
    StreamJson,
}

// ── Printer ──

/// [`EventSink`] that prints a headless run in its [`OutputFormat`]. Progress, tool calls, and
/// errors go to stderr as text in every format, so stdout carries only the reply or the JSON.
struct Printer<O, E> {
    format: OutputFormat,
    session_id: String,
    stdio: StdioSink,
    output: Mutex<Output<O, E>>,
}

struct Output<O, E> {
    stdout: O,
    stderr: E,
    /// Reply text since the last tool call, which the result reports as the final message.
    message: String,
    error: Option<String>,
    session_write_error: Option<String>,
    usage: Option<UsageSnapshot>,
}

impl<O: Write, E: Write> Printer<O, E> {
    fn new(
        format: OutputFormat,
        show_thinking: bool,
        session_id: String,
        stdout: O,
        stderr: E,
    ) -> Self {
        Self {
            format,
            session_id,
            // Thinking renders on stdout, so the JSON formats leave it off, which also keeps the
            // progress notes it would replace on stderr.
            stdio: StdioSink::new(show_thinking && format == OutputFormat::Text),
            output: Mutex::new(Output {
                stdout,
                stderr,
                message: String::new(),
                error: None,
                session_write_error: None,
                usage: None,
            }),
        }
    }

    /// Prints the result that ends a JSON-format run, or ends the reply line a text run left open.
    fn finish(&self, stop_reason: TurnEnd) -> Result<()> {
        let mut output = self.lock();
        if self.format == OutputFormat::Text {
            return Ok(self.stdio.end_line(&mut output.stdout)?);
        }
        let Output {
            stdout,
            message,
            error,
            session_write_error,
            usage,
            ..
        } = &mut *output;
        let line = Line::Result(RunResult {
            stop_reason,
            error: error.as_deref(),
            session_write_error: session_write_error.as_deref(),
            message: (stop_reason == TurnEnd::Completed).then_some(message.as_str()),
            usage: usage.as_ref(),
            session_id: &self.session_id,
        });
        write_json_line(stdout, &line)
    }

    /// Moves past the `^C` a terminal echoes after a signal.
    fn break_line(&self) -> Result<()> {
        let mut output = self.lock();
        let Output { stdout, stderr, .. } = &mut *output;
        Ok(self.stdio.break_line(stdout, stderr)?)
    }

    /// Reports that output failed on the stream every other line uses.
    fn report_failure(&self, error: &anyhow::Error) {
        let mut output = self.lock();
        _ = writeln!(output.stderr, "Error: failed to print output: {error:#}");
    }

    fn record_session_write_error(&self, failure: &str) {
        self.lock().session_write_error = Some(failure.to_owned());
    }

    /// Prints one event, naming the stream that failed.
    fn print(&self, event: Event) -> Printed {
        let mut output = self.lock();
        output.observe(&event);
        let Output { stdout, stderr, .. } = &mut *output;
        let mut stdout = Watched::new(stdout);
        let result = match self.format {
            OutputFormat::Text => self.stdio.render(event, &mut stdout, stderr),
            OutputFormat::Json => self.stdio.render(event, &mut io::sink(), stderr),
            OutputFormat::StreamJson => {
                let written =
                    wire::wire(&event).map_or(Ok(()), |line| write_json_line(&mut stdout, &line));
                self.stdio
                    .render(event, &mut io::sink(), stderr)
                    .and(written)
            }
        };
        match result {
            Ok(()) => Printed::Complete,
            Err(error) if stdout.failed => Printed::Stdout(error),
            Err(error) => Printed::Stderr(error),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Output<O, E>> {
        lock_or_recover(&self.output, "headless output")
    }
}

impl<O: Write + Send, E: Write + Send> EventSink for Printer<O, E> {
    fn send(&self, event: Event) -> Result<()> {
        self.print(event).into_result()
    }
}

/// Which stream printing an event failed on, since only a lost stdout ends the run.
enum Printed {
    Complete,
    Stdout(anyhow::Error),
    Stderr(anyhow::Error),
}

impl Printed {
    fn into_result(self) -> Result<()> {
        match self {
            Self::Complete => Ok(()),
            Self::Stdout(error) | Self::Stderr(error) => Err(error),
        }
    }
}

/// Records a failed write or flush to the wrapped stream, so a render that interleaves stdout and
/// stderr can say which one broke.
struct Watched<W> {
    inner: W,
    failed: bool,
}

impl<W: Write> Watched<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            failed: false,
        }
    }
}

impl<W: Write> Write for Watched<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf).inspect_err(|_| self.failed = true)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().inspect_err(|_| self.failed = true)
    }
}

impl<O, E> Output<O, E> {
    fn observe(&mut self, event: &Event) {
        match event {
            Event::StreamToken(text) => self.message.push_str(text),
            Event::ToolCallStart(_) => self.message.clear(),
            Event::TurnFailed(error) => self.error = Some(error.clone()),
            Event::UsageUpdated(usage) => self.usage = Some(*usage),
            _ => {}
        }
    }
}

/// The line that ends a JSON-format run, tagged like an [`Event`] line.
#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum Line<'a> {
    Result(RunResult<'a>),
}

/// How a headless run ended.
#[derive(Serialize)]
struct RunResult<'a> {
    stop_reason: TurnEnd,
    /// Why the turn failed.
    error: Option<&'a str>,
    /// Why the session could not be saved, which leaves its run missing from the transcript.
    session_write_error: Option<&'a str>,
    /// The reply after the last tool call, kept only when the turn completed.
    message: Option<&'a str>,
    /// The latest usage snapshot any request reported.
    usage: Option<&'a UsageSnapshot>,
    session_id: &'a str,
}

fn write_json_line(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use indoc::indoc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ox_core::engine::testing::{
        engine, mount_reply, mount_tool_call_once, panic_message, panicked_engine, request_bodies,
        unsaved_session_handle, with_pinned_home,
    };
    use ox_core::permission::PermissionClass;
    use ox_core::protocol::testing::ls_call;
    use ox_core::session::store::testing::test_store;
    use ox_core::tool::testing::bash_registry;
    use ox_core::tool::{Prepared, Tool, ToolCtx, ToolOutput, ToolSpec};

    use super::*;
    use crate::driver::testing::ClosedPipe;

    const SESSION_ID: &str = "session-1";

    fn printer(format: OutputFormat) -> Printer<Vec<u8>, Vec<u8>> {
        Printer::new(format, false, SESSION_ID.to_owned(), Vec::new(), Vec::new())
    }

    /// What `printer` wrote, as `(stdout, stderr)`.
    fn printed<O>(printer: Printer<O, Vec<u8>>) -> (O, String) {
        let Output { stdout, stderr, .. } = printer.output.into_inner().unwrap();
        (stdout, String::from_utf8(stderr).unwrap())
    }

    /// The `data` of the result line that ends `stdout`.
    fn result_data(stdout: &[u8]) -> serde_json::Value {
        let stdout = std::str::from_utf8(stdout).unwrap();
        let line = stdout.lines().last().expect("a result line");
        let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(record["type"], "result");
        record["data"].take()
    }

    mod read_input {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn says_once_that_it_waits_for_an_open_stdin_to_close() {
            let (mut writer, reader) = tokio::io::duplex(64);
            tokio::io::AsyncWriteExt::write_all(&mut writer, b"+fix\n")
                .await
                .unwrap();
            let closer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(writer);
            });
            let mut notice = Vec::new();

            let text = read_input(reader, &mut notice, Duration::from_secs(3)).await;

            assert_eq!(text.unwrap(), "+fix\n");
            assert_eq!(
                String::from_utf8(notice).unwrap(),
                "Waiting for stdin to close, since `--print` reads it as input. \
                 Redirect stdin from /dev/null to skip it.\n",
            );
            closer.await.unwrap();
        }

        #[tokio::test(start_paused = true)]
        async fn reads_a_closed_stdin_without_a_notice() {
            let input: &[u8] = b"+fix\n";
            let mut notice = Vec::new();

            let text = read_input(input, &mut notice, Duration::from_secs(3)).await;

            assert_eq!(
                (text.unwrap().as_str(), notice.as_slice()),
                ("+fix\n", &b""[..])
            );
        }

        #[tokio::test(start_paused = true)]
        async fn replaces_invalid_utf8_and_keeps_the_text() {
            let input: &[u8] = b"caf\xe9 log line\n";
            let mut notice = Vec::new();

            let text = read_input(input, &mut notice, Duration::from_secs(3)).await;

            assert_eq!(
                (text.unwrap().as_str(), notice.as_slice()),
                ("caf\u{FFFD} log line\n", &b""[..])
            );
        }
    }

    mod compose_prompt {
        use super::*;

        #[test]
        fn attaches_stdin_after_the_prompt_in_a_tagged_block() {
            let stdin = "    let x = 1;\r\n+fix\r\n";

            let prompt = compose_prompt(Some("Review this diff."), Some(stdin));

            assert_eq!(
                prompt.as_deref(),
                Some("Review this diff.\n\n<stdin>\n    let x = 1;\r\n+fix\n</stdin>"),
            );
        }

        #[test]
        fn sends_either_part_alone() {
            assert_eq!(
                compose_prompt(Some("Summarize."), None).as_deref(),
                Some("Summarize."),
            );
            assert_eq!(
                compose_prompt(None, Some("Explain this log.\n")).as_deref(),
                Some("Explain this log."),
            );
        }

        #[test]
        fn treats_blank_stdin_as_absent() {
            assert_eq!(
                compose_prompt(Some("Summarize."), Some(" \n")).as_deref(),
                Some("Summarize."),
            );
            assert_eq!(compose_prompt(None, Some("\n\n")), None);
            assert_eq!(compose_prompt(None, None), None);
        }
    }

    mod drive {
        use super::*;

        /// A read-class tool, which the default policy runs without asking, with a fixed result.
        struct LookTool;

        impl Tool for LookTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "look".to_owned(),
                    description: "Looks around the project.".to_owned(),
                    input_schema: serde_json::json!({"type": "object"}),
                    class: PermissionClass::Read,
                    parallel_safe: true,
                }
            }

            fn run<'a>(
                &'a self,
                _prepared: Prepared,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
                Box::pin(async { ToolOutput::success("README.md") })
            }
        }

        #[tokio::test]
        async fn prints_each_format_for_a_turn_with_a_tool_call_and_a_denied_call() {
            with_pinned_home(async {
                let mut stderrs = Vec::new();
                for &format in OutputFormat::value_variants() {
                    let server = MockServer::start().await;
                    mount_tool_call_once(&server, "call-1", "look", "{}").await;
                    mount_tool_call_once(&server, "call-2", "bash", r#"{"command":"git push"}"#)
                        .await;
                    mount_reply(&server, "Done.").await;
                    let dir = tempfile::tempdir().unwrap();
                    let tools = bash_registry(vec![Box::new(LookTool)]);
                    let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                        .await
                        .with_tools(tools)
                        .unattended();
                    let session_id = engine.session_id().to_owned();
                    let printer =
                        Printer::new(format, false, session_id.clone(), Vec::new(), Vec::new());

                    let exit = Box::pin(drive(engine.spawn(), &printer, "Look around."))
                        .await
                        .unwrap();

                    assert_eq!(exit, Exit::Success);
                    let (stdout, stderr) = printed(printer);
                    let stdout = String::from_utf8(stdout)
                        .unwrap()
                        .replace(&session_id, SESSION_ID);
                    let name = format.to_possible_value().unwrap();
                    insta::assert_snapshot!(format!("{}_stdout", name.get_name()), stdout);
                    stderrs.push(stderr);
                }
                insta::assert_snapshot!("stderr", stderrs[0]);
                assert!(stderrs.iter().all(|stderr| *stderr == stderrs[0]));
            })
            .await;
        }

        #[tokio::test]
        async fn unescapes_the_prompt_and_reports_the_completed_reply() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let printer = printer(OutputFormat::Json);

                let exit = Box::pin(drive(engine.spawn(), &printer, "//hello"))
                    .await
                    .unwrap();

                assert_eq!(exit, Exit::Success);
                let (stdout, stderr) = printed(printer);
                let result = result_data(&stdout);
                assert_eq!(result["stop_reason"], "completed");
                assert_eq!(result["error"], serde_json::Value::Null);
                assert_eq!(result["message"], "reply");
                assert_eq!(stderr, "");
                let requests = request_bodies(&server).await;
                assert_eq!(requests[0]["messages"][1]["content"][0]["text"], "/hello");
            })
            .await;
        }

        #[tokio::test]
        async fn fails_with_the_api_error_when_the_request_is_rejected() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/messages"))
                    .respond_with(ResponseTemplate::new(400))
                    .mount(&server)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let printer = printer(OutputFormat::Json);

                let exit = Box::pin(drive(engine.spawn(), &printer, "hello"))
                    .await
                    .unwrap();

                assert_eq!(exit, Exit::Failure);
                let (stdout, stderr) = printed(printer);
                let result = result_data(&stdout);
                assert_eq!(result["stop_reason"], "failed");
                assert_eq!(
                    result["error"],
                    "Anthropic API error (HTTP 400 Bad Request)"
                );
                assert_eq!(result["message"], serde_json::Value::Null);
                assert_eq!(
                    stderr,
                    "Error: Anthropic API error (HTTP 400 Bad Request)\n"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn prints_the_failed_result_before_handing_back_an_engine_panic() {
            let printer = printer(OutputFormat::StreamJson);

            let panic = Box::pin(drive(panicked_engine(), &printer, "hello"))
                .await
                .unwrap_err();

            assert_eq!(panic_message(panic), "engine bug");
            let (stdout, stderr) = printed(printer);
            assert_eq!(
                String::from_utf8(stdout).unwrap(),
                indoc! {r#"
                    {"type":"turn_failed","data":"ox stopped unexpectedly"}
                    {"type":"result","data":{"stop_reason":"failed","error":"ox stopped unexpectedly","session_write_error":null,"message":null,"usage":null,"session_id":"session-1"}}
                "#},
            );
            assert_eq!(stderr, "Error: ox stopped unexpectedly\n");
        }

        #[tokio::test]
        async fn fails_a_completed_turn_whose_result_cannot_be_printed() {
            with_pinned_home(async {
                for &format in OutputFormat::value_variants() {
                    let server = MockServer::start().await;
                    mount_reply(&server, "reply").await;
                    let dir = tempfile::tempdir().unwrap();
                    let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    let printer =
                        Printer::new(format, false, SESSION_ID.to_owned(), ClosedPipe, Vec::new());

                    let exit = Box::pin(drive(engine.spawn(), &printer, "hello"))
                        .await
                        .unwrap();

                    assert_eq!(exit, Exit::Failure, "{format:?}");
                    let (_, stderr) = printed(printer);
                    assert!(
                        stderr.starts_with("Error: failed to print output: "),
                        "{format:?}: {stderr}"
                    );
                }
            })
            .await;
        }

        #[tokio::test]
        async fn stops_the_turn_once_stdout_is_gone() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_tool_call_once(&server, "call-1", "look", "{}").await;
                mount_reply(&server, "Done.").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                    .await
                    .with_tools(bash_registry(vec![Box::new(LookTool)]))
                    .unattended();
                let printer = Printer::new(
                    OutputFormat::StreamJson,
                    false,
                    SESSION_ID.to_owned(),
                    ClosedPipe,
                    Vec::new(),
                );

                let exit = Box::pin(drive(engine.spawn(), &printer, "Look around."))
                    .await
                    .unwrap();

                assert_eq!(exit, Exit::Failure);
                assert_eq!(
                    request_bodies(&server).await.len(),
                    1,
                    "the interrupt ends the turn before the round that follows the tool call",
                );
            })
            .await;
        }

        #[tokio::test]
        async fn fails_the_run_when_the_session_cannot_be_saved() {
            let failure = "session writer task has stopped";
            let printer = printer(OutputFormat::Json);
            let handle = unsaved_session_handle(failure);

            let exit = Box::pin(drive(handle, &printer, "hello")).await.unwrap();

            assert_eq!(exit, Exit::Failure);
            let (stdout, stderr) = printed(printer);
            let result = result_data(&stdout);
            assert_eq!(result["stop_reason"], "completed");
            assert_eq!(result["session_write_error"], failure);
            assert_eq!(stderr, format!("Error: Session write failed: {failure}\n"));
        }
    }

    mod interrupt_on_closed_stdout {
        use super::*;

        /// A stdout that fails for a reason other than a closed reader.
        struct FailingStdout;

        impl Write for FailingStdout {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("disk full"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn sends_one_interrupt_after_stdout_closes() {
            let printer = Printer::new(
                OutputFormat::StreamJson,
                false,
                SESSION_ID.to_owned(),
                ClosedPipe,
                Vec::new(),
            );
            let (ops, mut received) = mpsc::channel(4);
            let sink = InterruptOnClosedStdout::new(&printer, &ops);

            sink.send(Event::StreamToken("first".to_owned()))
                .unwrap_err();
            sink.send(Event::StreamToken("second".to_owned()))
                .unwrap_err();

            assert_eq!(received.try_recv().ok(), Some(Op::Interrupt));
            assert_eq!(received.try_recv().ok(), None);
        }

        #[test]
        fn keeps_running_when_stdout_fails_for_another_reason() {
            let printer = Printer::new(
                OutputFormat::StreamJson,
                false,
                SESSION_ID.to_owned(),
                FailingStdout,
                Vec::new(),
            );
            let (ops, mut received) = mpsc::channel(4);
            let sink = InterruptOnClosedStdout::new(&printer, &ops);

            sink.send(Event::StreamToken("first".to_owned()))
                .unwrap_err();

            assert_eq!(received.try_recv().ok(), None);
        }

        #[test]
        fn keeps_running_when_only_stderr_closes() {
            let printer = Printer::new(
                OutputFormat::Json,
                false,
                SESSION_ID.to_owned(),
                Vec::new(),
                ClosedPipe,
            );
            let (ops, mut received) = mpsc::channel(4);
            let sink = InterruptOnClosedStdout::new(&printer, &ops);

            sink.send(Event::Notice("Reading.".to_owned())).unwrap_err();

            assert_eq!(received.try_recv().ok(), None);
        }

        #[test]
        fn retries_the_interrupt_after_a_full_channel() {
            let printer = Printer::new(
                OutputFormat::StreamJson,
                false,
                SESSION_ID.to_owned(),
                ClosedPipe,
                Vec::new(),
            );
            let (ops, mut received) = mpsc::channel(1);
            ops.try_send(Op::Shutdown).unwrap();
            let sink = InterruptOnClosedStdout::new(&printer, &ops);

            sink.send(Event::StreamToken("first".to_owned()))
                .unwrap_err();
            assert_eq!(received.try_recv().ok(), Some(Op::Shutdown));

            sink.send(Event::StreamToken("second".to_owned()))
                .unwrap_err();
            assert_eq!(received.try_recv().ok(), Some(Op::Interrupt));
        }
    }

    mod rendering {
        use super::*;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Failure {
            Write,
            Flush,
        }

        struct FailOnce {
            output: Vec<u8>,
            failure: Option<Failure>,
        }

        impl Write for FailOnce {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.failure == Some(Failure::Write) {
                    self.failure = None;
                    return Err(io::Error::other("first write failed"));
                }
                self.output.write(buf)
            }

            fn flush(&mut self) -> io::Result<()> {
                if self.failure == Some(Failure::Flush) {
                    self.failure = None;
                    return Err(io::Error::other("first flush failed"));
                }
                Ok(())
            }
        }

        #[test]
        fn retains_early_write_and_flush_failures_after_later_output_succeeds() {
            for format in [OutputFormat::Text, OutputFormat::StreamJson] {
                for failure in [Failure::Write, Failure::Flush] {
                    let stdout = FailOnce {
                        output: Vec::new(),
                        failure: Some(failure),
                    };
                    let printer =
                        Printer::new(format, false, SESSION_ID.to_owned(), stdout, Vec::new());
                    let renderer = Renderer::new(&printer);

                    for event in [
                        Event::StreamToken("lost".to_owned()),
                        Event::StreamToken("reply".to_owned()),
                        Event::Error("boom".to_owned()),
                        Event::TurnComplete,
                    ] {
                        renderer.render(event);
                    }
                    let finished = renderer.finish().and(printer.finish(TurnEnd::Completed));
                    let expected = match failure {
                        Failure::Write => "first write failed",
                        Failure::Flush => "first flush failed",
                    };
                    assert_eq!(
                        finished.unwrap_err().to_string(),
                        expected,
                        "{format:?}, {failure:?}",
                    );

                    let (stdout, stderr) = printed(printer);
                    if format == OutputFormat::Text {
                        let expected = match failure {
                            Failure::Write => "reply\n",
                            Failure::Flush => "lostreply\n",
                        };
                        assert_eq!(stdout.output, expected.as_bytes(), "{failure:?}");
                    } else {
                        let result = result_data(&stdout.output);
                        assert_eq!(result["stop_reason"], "completed", "{failure:?}");
                        assert_eq!(result["message"], "lostreply", "{failure:?}");
                    }
                    assert_eq!(stderr, "Error: boom\n", "{format:?}, {failure:?}");
                }
            }
        }
    }

    mod printer {
        use super::*;

        mod finish {
            use super::*;

            #[test]
            fn reports_the_reply_after_the_last_tool_call_and_the_latest_usage() {
                let usage = |tokens| UsageSnapshot {
                    context_tokens: Some(tokens),
                    context_window: Some(200_000),
                    estimated_cost_usd: Some(0.5),
                };
                let printer = printer(OutputFormat::Json);
                for event in [
                    Event::StreamToken("Let me ".to_owned()),
                    Event::StreamToken("look.".to_owned()),
                    Event::ToolCallStart(ls_call()),
                    Event::UsageUpdated(usage(1_000)),
                    Event::StreamToken("Found ".to_owned()),
                    Event::StreamToken("it.".to_owned()),
                    Event::UsageUpdated(usage(2_000)),
                    Event::TurnComplete,
                ] {
                    printer.send(event).unwrap();
                }

                printer.finish(TurnEnd::Completed).unwrap();

                let stdout = String::from_utf8(printed(printer).0).unwrap();
                assert_eq!(stdout.lines().count(), 1, "{stdout}");
                assert_eq!(
                    result_data(stdout.as_bytes()),
                    serde_json::json!({
                        "stop_reason": "completed",
                        "error": null,
                        "session_write_error": null,
                        "message": "Found it.",
                        "usage": {
                            "context_tokens": 2_000,
                            "context_window": 200_000,
                            "estimated_cost_usd": 0.5,
                        },
                        "session_id": SESSION_ID,
                    }),
                );
            }

            #[test]
            fn ends_the_open_reply_line_in_the_text_format() {
                let printer = printer(OutputFormat::Text);
                printer
                    .send(Event::StreamToken("partial reply".to_owned()))
                    .unwrap();

                printer.finish(TurnEnd::Shutdown).unwrap();
                printer.finish(TurnEnd::Shutdown).unwrap();

                assert_eq!(
                    printed(printer),
                    (b"partial reply\n".to_vec(), String::new()),
                );
            }

            #[test]
            fn reports_a_session_write_failure_beside_the_completed_turn() {
                let printer = printer(OutputFormat::Json);
                printer
                    .send(Event::StreamToken("reply".to_owned()))
                    .unwrap();
                printer.record_session_write_error("disk full");

                printer.finish(TurnEnd::Completed).unwrap();

                let result = result_data(&printed(printer).0);
                assert_eq!(result["stop_reason"], "completed");
                assert_eq!(result["message"], "reply");
                assert_eq!(result["session_write_error"], "disk full");
            }
        }

        mod break_line {
            use super::*;

            #[test]
            fn ends_an_open_reply_line_once() {
                let printer = printer(OutputFormat::Text);
                printer
                    .send(Event::StreamToken("partial".to_owned()))
                    .unwrap();

                printer.break_line().unwrap();
                printer.finish(TurnEnd::Shutdown).unwrap();

                assert_eq!(printed(printer), (b"partial\n".to_vec(), String::new()),);
            }
        }

        mod send {
            use super::*;

            #[test]
            fn keeps_progress_notes_on_stderr_when_json_runs_show_thinking() {
                let printer = Printer::new(
                    OutputFormat::Json,
                    true,
                    SESSION_ID.to_owned(),
                    Vec::new(),
                    Vec::new(),
                );
                for event in [
                    Event::ThinkingToken("Hmm.".to_owned()),
                    Event::ProgressUpdate("Checking the loader.".to_owned()),
                ] {
                    printer.send(event).unwrap();
                }

                assert_eq!(
                    printed(printer),
                    (Vec::new(), "Checking the loader.\n".to_owned()),
                );
            }

            #[test]
            fn renders_stderr_even_when_stdout_is_closed() {
                let printer = Printer::new(
                    OutputFormat::StreamJson,
                    false,
                    SESSION_ID.to_owned(),
                    ClosedPipe,
                    Vec::new(),
                );

                let sent = printer.send(Event::Error("boom".to_owned()));

                assert!(is_broken_pipe(&sent.unwrap_err()));
                assert_eq!(printed(printer).1, "Error: boom\n");
            }
        }
    }
}
