//! Bare REPL driver for `--no-tui`: prompts from stdin, replies streamed to stdout.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use super::{Exit, render_queued, render_turn, shutdown_signal};
use crate::agent::event::{AgentEvent, AgentSink, StdioSink, UserAction};
use crate::engine::{Engine, EngineHandle, TurnEnd};
use crate::slash;

/// Reads prompts until EOF or a signal, then finalizes the session. EOF after a failed turn exits
/// with failure.
pub(crate) async fn run(engine: Engine, show_thinking: bool, drifted: &[PathBuf]) -> Result<Exit> {
    let sink = StdioSink::new(show_thinking, Arc::clone(engine.tools()));
    let stdin = BufReader::new(tokio::io::stdin());
    drive(engine.spawn(drifted), &sink, stdin).await
}

async fn drive<R>(mut handle: EngineHandle, sink: &dyn AgentSink, input: R) -> Result<Exit>
where
    R: AsyncBufRead + Unpin,
{
    let mut lines = input.lines();
    let mut signal = None;
    let mut failed = false;

    let result: Result<()> = async {
        loop {
            render_queued(&mut handle.events, sink);
            eprint!("> ");
            std::io::stderr().flush()?;

            let line = tokio::select! {
                line = lines.next_line() => line?,
                caught = shutdown_signal() => {
                    eprintln!();
                    signal = Some(caught);
                    None
                }
            };
            let Some(line) = line else {
                break;
            };

            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            if let Some(parsed) = slash::parse_slash(input) {
                sink.emit(
                    AgentEvent::Error(format!(
                        "Slash commands are available only in the TUI. Use //{} to send the \
                         literal text.",
                        parsed.name,
                    )),
                    "slash-command-rejected",
                );
                continue;
            }

            let prompt = slash::unescape_literal(input).to_owned();
            // A send fails only once the engine has stopped, which `render_turn` then reports.
            _ = handle.ops.send(UserAction::SubmitPrompt(prompt)).await;
            tokio::select! {
                end = render_turn(&mut handle.events, sink) => match end {
                    Some(TurnEnd::Failed) => {
                        println!();
                        failed = true;
                    }
                    Some(_) => {}
                    None => bail!("the engine stopped"),
                },
                caught = shutdown_signal() => {
                    eprintln!();
                    signal = Some(caught);
                    break;
                }
            }
        }
        Ok(())
    }
    .await;

    let failure = handle.shutdown().await;
    sink.session_write_error(failure.as_deref());
    result?;
    Ok(match signal {
        Some(signal) => Exit::Signal(signal),
        None if failed => Exit::Failure,
        None => Exit::Success,
    })
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::engine::testing::{
        engine, mount_reply, request_bodies, stopped_engine, with_pinned_home,
    };
    use crate::session::store::testing::test_store;

    // ── drive ──

    #[tokio::test]
    async fn drive_skips_blank_lines_and_exits_cleanly_at_eof() {
        with_pinned_home(async {
            let server = MockServer::start().await;
            mount_reply(&server, "reply").await;
            let dir = tempfile::tempdir().unwrap();
            let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
            let sink = CapturingSink::new();
            let input: &[u8] = b"\n   \nhello\n";

            let exit = Box::pin(drive(engine.spawn(&[]), &sink, input))
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
    async fn drive_keeps_reading_after_a_failed_turn_and_exits_with_failure() {
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
            let sink = CapturingSink::new();
            let input: &[u8] = b"first\n/clear\n//literal\n";

            let exit = Box::pin(drive(
                engine.spawn(&[PathBuf::from("drifted.rs")]),
                &sink,
                input,
            ))
            .await
            .unwrap();

            assert_eq!(exit, Exit::Failure);
            let requests = request_bodies(&server).await;
            assert_eq!(requests.len(), 2, "/clear must not reach the model");
            let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(last["content"][0]["text"], "/literal");
            let reports: Vec<String> = sink
                .events()
                .into_iter()
                .filter_map(|event| match event {
                    AgentEvent::Error(msg) | AgentEvent::TurnFailed(msg) => Some(msg),
                    _ => None,
                })
                .collect();
            assert_eq!(reports.len(), 3, "{reports:?}");
            assert!(reports[0].ends_with(": drifted.rs"), "{reports:?}");
            assert!(reports[1].contains("HTTP 400"), "{reports:?}");
            assert!(
                reports[2].contains("only in the TUI. Use //clear"),
                "{reports:?}"
            );
            assert!(matches!(
                sink.events().last(),
                Some(AgentEvent::TurnComplete)
            ));
            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(
                data.messages.len(),
                3,
                "the failed prompt, the literal, and its reply are saved",
            );
        })
        .await;
    }

    #[tokio::test]
    async fn drive_fails_once_the_engine_stops() {
        let input: &[u8] = b"hello\n";

        let result = Box::pin(drive(stopped_engine(), &CapturingSink::new(), input)).await;

        assert_eq!(result.unwrap_err().to_string(), "the engine stopped");
    }
}
