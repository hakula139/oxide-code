//! Headless driver for `--prompt`: one turn, printed to stdout as text or JSON.

use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use clap::ValueEnum;
use serde::Serialize;

use super::stdio::{StdioSink, Unattended, render_op};
use super::{Exit, shutdown_signal};
use crate::engine::{Engine, EngineHandle, EnginePanic, TurnEnd};
use crate::protocol::{Event, EventSink, Op, UsageSnapshot};
use crate::slash;
use crate::util::sync::lock_or_recover;

/// Runs `prompt` as a single turn, printed in `format`. A turn that does not complete exits with
/// failure, and a signal exits with its own status. If the engine panics, the panic is returned
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
    drive(engine.spawn(&[]), &printer, prompt).await
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
    let prompt = Op::Prompt(slash::unescape_literal(prompt).to_owned());
    // A send fails only once the engine has stopped, which `render_op` then reports.
    _ = handle.ops.send(prompt.clone()).await;
    let mut approver = Unattended;
    let rendered = render_op(
        &mut handle.events,
        printer,
        &handle.ops,
        &prompt,
        &mut approver,
    );
    let (end, mut exit) = tokio::select! {
        end = rendered => match end {
            Some(TurnEnd::Completed) => (TurnEnd::Completed, Exit::Success),
            Some(end) => (end, Exit::Failure),
            None => {
                printer.emit(
                    Event::TurnFailed("The engine stopped before the turn ended".to_owned()),
                    "engine-stopped",
                );
                (TurnEnd::Failed, Exit::Failure)
            }
        },
        signal = shutdown_signal() => {
            eprintln!();
            (TurnEnd::Shutdown, Exit::Signal(signal))
        }
    };

    let finished = handle.shutdown().await;
    if let Ok(failure) = &finished {
        printer.session_write_error(failure.as_deref());
    }
    if let Err(e) = printer.finish(end) {
        eprintln!("Error: failed to print the result: {e:#}");
        if !matches!(exit, Exit::Signal(_)) {
            exit = Exit::Failure;
        }
    }
    finished.map(|_| exit)
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
    /// Each engine event as a line of JSON, then the result.
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
                usage: None,
            }),
        }
    }

    /// Prints the result that ends a JSON-format run.
    fn finish(&self, stop_reason: TurnEnd) -> Result<()> {
        if self.format == OutputFormat::Text {
            return Ok(());
        }
        let mut output = self.lock();
        let Output {
            stdout,
            message,
            error,
            usage,
            ..
        } = &mut *output;
        let line = Line::Result(RunResult {
            stop_reason,
            error: error.as_deref(),
            message: (stop_reason == TurnEnd::Completed).then_some(message.as_str()),
            usage: usage.as_ref(),
            session_id: &self.session_id,
        });
        write_json_line(stdout, &line)
    }

    fn lock(&self) -> MutexGuard<'_, Output<O, E>> {
        lock_or_recover(&self.output, "headless output")
    }
}

impl<O: Write + Send, E: Write + Send> EventSink for Printer<O, E> {
    fn send(&self, event: Event) -> Result<()> {
        let mut output = self.lock();
        output.observe(&event);
        let Output { stdout, stderr, .. } = &mut *output;
        match self.format {
            OutputFormat::Text => self.stdio.render(event, stdout, stderr),
            OutputFormat::Json => self.stdio.render(event, &mut io::sink(), stderr),
            OutputFormat::StreamJson => {
                let written = write_json_line(stdout, &event);
                self.stdio
                    .render(event, &mut io::sink(), stderr)
                    .and(written)
            }
        }
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
    /// The reply after the last tool call, kept only when the turn completed.
    message: Option<&'a str>,
    /// The latest usage snapshot any request reported.
    usage: Option<&'a UsageSnapshot>,
    session_id: &'a str,
}

fn write_json_line(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use indoc::indoc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::engine::testing::{
        engine, mount_reply, mount_tool_call_once, panic_message, panicked_engine, request_bodies,
        with_pinned_home,
    };
    use crate::permission::PermissionClass;
    use crate::protocol::testing::ls_call;
    use crate::session::store::testing::test_store;
    use crate::tool::bash::BashTool;
    use crate::tool::{Prepared, Tool, ToolCtx, ToolOutput, ToolRegistry, ToolSpec};

    const SESSION_ID: &str = "session-1";

    /// A stdout whose reader went away.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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
                    let tools =
                        ToolRegistry::new(vec![Box::new(LookTool), Box::new(BashTool::default())]);
                    let engine = engine(server.uri(), &test_store(dir.path()), dir.path())
                        .await
                        .with_tools(tools);
                    let session_id = engine.session_id().to_owned();
                    let printer =
                        Printer::new(format, false, session_id.clone(), Vec::new(), Vec::new());

                    let exit = Box::pin(drive(engine.spawn(&[]), &printer, "Look around."))
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

                let exit = Box::pin(drive(engine.spawn(&[]), &printer, "//hello"))
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

                let exit = Box::pin(drive(engine.spawn(&[]), &printer, "hello"))
                    .await
                    .unwrap();

                assert_eq!(exit, Exit::Failure);
                let (stdout, stderr) = printed(printer);
                let result = result_data(&stdout);
                assert_eq!(result["stop_reason"], "failed");
                assert_eq!(result["error"], "API error (HTTP 400 Bad Request)");
                assert_eq!(result["message"], serde_json::Value::Null);
                assert_eq!(stderr, "Error: API error (HTTP 400 Bad Request)\n");
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
                    {"type":"turn_failed","data":"The engine stopped before the turn ended"}
                    {"type":"result","data":{"stop_reason":"failed","error":"The engine stopped before the turn ended","message":null,"usage":null,"session_id":"session-1"}}
                "#},
            );
            assert_eq!(stderr, "Error: The engine stopped before the turn ended\n");
        }

        #[tokio::test]
        async fn fails_a_completed_turn_whose_result_cannot_be_printed() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let printer = Printer::new(
                    OutputFormat::Json,
                    false,
                    SESSION_ID.to_owned(),
                    ClosedPipe,
                    Vec::new(),
                );

                let exit = Box::pin(drive(engine.spawn(&[]), &printer, "hello"))
                    .await
                    .unwrap();

                assert_eq!(exit, Exit::Failure);
            })
            .await;
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
            fn prints_nothing_in_the_text_format() {
                let printer = printer(OutputFormat::Text);
                printer
                    .send(Event::StreamToken("reply".to_owned()))
                    .unwrap();

                printer.finish(TurnEnd::Completed).unwrap();

                assert_eq!(printed(printer), (b"reply".to_vec(), String::new()));
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

                assert!(sent.is_err());
                assert_eq!(printed(printer).1, "Error: boom\n");
            }
        }
    }
}
