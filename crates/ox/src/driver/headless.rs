//! Headless driver for `--print`: one turn, printed to stdout as text or JSON.

use std::io::{self, IsTerminal as _, Write};
use std::pin::pin;
use std::time::Duration;

use anyhow::{Context as _, Result};
use indoc::formatdoc;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use ox_core::command;
use ox_core::engine::{Engine, EngineHandle, EnginePanic};
use ox_core::protocol::notice::ENGINE_STOPPED;
use ox_core::protocol::{Event, Op, OpEnd};

use super::stdio::{Output, OutputFormat, RunEnd, Unattended, run_op};
use super::{Exit, Shutdown, Signals, decode_input};

/// How long [`read_prompt`] waits on an open stdin before saying what it waits for.
const STDIN_NOTICE_DELAY: Duration = Duration::from_secs(3);

/// Runs `prompt` as a single turn, printed in `format`. An incomplete turn or failed output exits
/// with failure, and a signal exits with its own status. An engine panic is returned after the
/// failed result prints.
pub(crate) async fn run(
    engine: Engine,
    show_thinking: bool,
    prompt: &str,
    format: OutputFormat,
) -> Result<Exit, EnginePanic> {
    let session_id = engine.session_id();
    let mut output = Output::new(format, show_thinking, io::stdout(), io::stderr());
    let mut signals = Signals::new();
    drive(
        engine.spawn(),
        &mut output,
        &session_id,
        prompt,
        &mut async || signals.recv().await,
    )
    .await
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

/// Runs `prompt` through `handle` as [`run`] describes. Each `signal` call awaits the next signal.
async fn drive<O: Write, E: Write>(
    mut handle: EngineHandle,
    output: &mut Output<O, E>,
    session_id: &str,
    prompt: &str,
    signal: &mut impl AsyncFnMut() -> Shutdown,
) -> Result<Exit, EnginePanic> {
    let prompt = Op::prompt(command::unescape_literal(prompt));
    let (end, mut exit) = match run_op(
        &mut handle.events,
        &handle.ops,
        output,
        signal,
        prompt,
        None::<&mut Unattended>,
    )
    .await
    {
        RunEnd::Ended(OpEnd::Completed) => (OpEnd::Completed, Exit::Success),
        RunEnd::Ended(end) => (end, Exit::Failure),
        RunEnd::EngineStopped => {
            output.render(Event::TurnFailed(ENGINE_STOPPED.to_owned()));
            (OpEnd::Failed, Exit::Failure)
        }
        RunEnd::Signal(signal) => (OpEnd::Shutdown, Exit::Signal(signal)),
    };
    let finished = handle.shutdown().await;
    let session_failure = finished.as_ref().ok().and_then(Option::as_deref);
    if let Some(failure) = session_failure {
        output.session_write_error(failure);
    }
    output.print_result(end, session_id);
    let printed = output.finish();
    if let Err(error) = &printed {
        output.report_failure(error);
    }
    if (printed.is_err() || session_failure.is_some()) && !matches!(exit, Exit::Signal(_)) {
        exit = Exit::Failure;
    }
    finished.map(|_| exit)
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use clap::ValueEnum as _;
    use indoc::indoc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use ox_core::engine::testing::{
        engine, mount_reply, mount_stalled_reply_once, mount_tool_call_once, panic_message,
        panicked_engine, request_bodies, unsaved_session_handle, until_requested, with_pinned_home,
    };
    use ox_core::permission::PermissionClass;
    use ox_core::session::store::testing::test_store;
    use ox_core::tool::testing::bash_registry;
    use ox_core::tool::{Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec};

    use super::*;
    use crate::driver::testing::{ClosedPipe, Screen, no_signal, output, result_data};

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
        async fn reads_a_closed_stdin_without_a_notice_and_replaces_invalid_utf8() {
            for (input, expected) in [
                (&b"+fix\n"[..], "+fix\n"),
                (b"caf\xe9 log line\n", "caf\u{FFFD} log line\n"),
            ] {
                let mut notice = Vec::new();

                let text = read_input(input, &mut notice, Duration::from_secs(3)).await;

                assert_eq!(
                    (text.unwrap().as_str(), notice.as_slice()),
                    (expected, &b""[..]),
                    "{input:?}",
                );
            }
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

        const SESSION_ID: &str = "session-1";

        /// A read-class tool, which the default policy runs without asking, with a fixed result.
        struct LookTool;

        impl Tool for LookTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    definition: ToolDefinition {
                        name: "look".to_owned(),
                        description: "Looks around the project.".to_owned(),
                        input_schema: serde_json::json!({"type": "object"}),
                    },
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
                    let (mut output, stdout, stderr) = output(format);

                    let exit = Box::pin(drive(
                        engine.spawn(),
                        &mut output,
                        SESSION_ID,
                        "Look around.",
                        &mut no_signal,
                    ))
                    .await
                    .unwrap();

                    assert_eq!(exit, Exit::Success);
                    let name = format.to_possible_value().unwrap();
                    insta::assert_snapshot!(format!("{}_stdout", name.get_name()), stdout.text());
                    stderrs.push(stderr.text());
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
                let (mut output, stdout, stderr) = output(OutputFormat::Json);

                let exit = Box::pin(drive(
                    engine.spawn(),
                    &mut output,
                    SESSION_ID,
                    "//hello",
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Success);
                let result = result_data(&stdout.text());
                assert_eq!(result["stop_reason"], "completed");
                assert_eq!(result["error"], serde_json::Value::Null);
                assert_eq!(result["message"], "reply");
                assert_eq!(stderr.text(), "");
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
                let (mut output, stdout, stderr) = output(OutputFormat::Json);

                let exit = Box::pin(drive(
                    engine.spawn(),
                    &mut output,
                    SESSION_ID,
                    "hello",
                    &mut no_signal,
                ))
                .await
                .unwrap();

                assert_eq!(exit, Exit::Failure);
                let result = result_data(&stdout.text());
                assert_eq!(result["stop_reason"], "failed");
                assert_eq!(
                    result["error"],
                    "Anthropic API error (HTTP 400 Bad Request)"
                );
                assert_eq!(result["message"], serde_json::Value::Null);
                assert_eq!(
                    stderr.text(),
                    "Error: Anthropic API error (HTTP 400 Bad Request)\n"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn reports_the_signal_that_stops_the_run_in_each_format() {
            with_pinned_home(async {
                for format in [
                    OutputFormat::Text,
                    OutputFormat::Json,
                    OutputFormat::StreamJson,
                ] {
                    let server = MockServer::start().await;
                    mount_stalled_reply_once(&server).await;
                    let dir = tempfile::tempdir().unwrap();
                    let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    let (mut output, stdout, stderr) = output(format);
                    let mut signal = async || {
                        until_requested(&server, 1).await;
                        Shutdown::Interrupt
                    };

                    let exit = Box::pin(drive(
                        engine.spawn(),
                        &mut output,
                        SESSION_ID,
                        "hello",
                        &mut signal,
                    ))
                    .await
                    .unwrap();

                    assert_eq!(exit, Exit::Signal(Shutdown::Interrupt), "{format:?}");
                    let stdout = stdout.text();
                    if format == OutputFormat::Text {
                        assert_eq!(stdout, "", "{format:?}");
                    } else {
                        assert_eq!(
                            result_data(&stdout)["stop_reason"],
                            "shutdown",
                            "{format:?}"
                        );
                    }
                    assert_eq!(stderr.text(), "\n", "{format:?}");
                }
            })
            .await;
        }

        #[tokio::test]
        async fn prints_the_failed_result_before_handing_back_an_engine_panic() {
            let (mut output, stdout, stderr) = output(OutputFormat::StreamJson);

            let panic = Box::pin(drive(
                panicked_engine(),
                &mut output,
                SESSION_ID,
                "hello",
                &mut no_signal,
            ))
            .await
            .unwrap_err();

            assert_eq!(panic_message(panic), "engine bug");
            assert_eq!(
                stdout.text(),
                indoc! {r#"
                    {"type":"turn_failed","data":"ox stopped unexpectedly"}
                    {"type":"result","data":{"stop_reason":"failed","error":"ox stopped unexpectedly","session_write_error":null,"message":null,"usage":null,"session_id":"session-1"}}
                "#},
            );
            assert_eq!(stderr.text(), "Error: ox stopped unexpectedly\n");
        }

        #[tokio::test]
        async fn fails_a_completed_turn_whose_result_cannot_be_printed() {
            with_pinned_home(async {
                for &format in OutputFormat::value_variants() {
                    let server = MockServer::start().await;
                    mount_reply(&server, "reply").await;
                    let dir = tempfile::tempdir().unwrap();
                    let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    let stderr = Screen::default();
                    let mut output = Output::new(format, false, ClosedPipe, stderr.clone());

                    let exit = Box::pin(drive(
                        engine.spawn(),
                        &mut output,
                        SESSION_ID,
                        "hello",
                        &mut no_signal,
                    ))
                    .await
                    .unwrap();

                    assert_eq!(exit, Exit::Failure, "{format:?}");
                    let stderr = stderr.text();
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
                let mut output =
                    Output::new(OutputFormat::StreamJson, false, ClosedPipe, io::sink());

                let exit = Box::pin(drive(
                    engine.spawn(),
                    &mut output,
                    SESSION_ID,
                    "Look around.",
                    &mut no_signal,
                ))
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
            let failure = "disk full";
            let (mut output, stdout, stderr) = output(OutputFormat::Json);
            let handle = unsaved_session_handle(failure);

            let exit = Box::pin(drive(
                handle,
                &mut output,
                SESSION_ID,
                "hello",
                &mut no_signal,
            ))
            .await
            .unwrap();

            assert_eq!(exit, Exit::Failure);
            let result = result_data(&stdout.text());
            assert_eq!(result["stop_reason"], "completed");
            assert_eq!(result["session_write_error"], failure);
            assert_eq!(
                stderr.text(),
                format!("Error: Session write failed: {failure}\n")
            );
        }
    }
}
