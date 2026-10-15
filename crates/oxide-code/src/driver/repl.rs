//! Bare REPL driver for `--no-tui`: prompts from stdin, replies streamed to stdout.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use super::{Conversation, Exit, TurnEnd, report_drift, shutdown_signal};
use crate::agent::event::{AgentEvent, AgentSink, StdioSink, inert_user_action_channel};
use crate::slash;

/// Reads prompts until EOF or a signal, then finalizes the session. EOF after a failed turn exits
/// with failure.
pub(crate) async fn run(
    conversation: Conversation,
    show_thinking: bool,
    drifted: &[PathBuf],
) -> Result<Exit> {
    let sink = StdioSink::new(show_thinking, Arc::clone(&conversation.tools));
    let stdin = BufReader::new(tokio::io::stdin());
    drive(conversation, &sink, stdin, drifted).await
}

async fn drive<S, R>(
    mut conversation: Conversation,
    sink: &S,
    input: R,
    drifted: &[PathBuf],
) -> Result<Exit>
where
    S: AgentSink + Clone + 'static,
    R: AsyncBufRead + Unpin,
{
    report_drift(sink, drifted);
    let mut lines = input.lines();
    let (_user_tx, mut user_rx) = inert_user_action_channel();
    let mut signal = None;
    let mut failed = false;

    let result: Result<()> = async {
        loop {
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

            tokio::select! {
                end = conversation.submit_prompt(input, sink, &mut user_rx) => {
                    if end == TurnEnd::Failed {
                        println!();
                        failed = true;
                    }
                }
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

    let failure = conversation.finalize().await;
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

    use super::super::testing::{conversation, mount_reply, request_bodies};
    use super::*;
    use crate::agent::event::CapturingSink;
    use crate::session::store::test_store;

    // ── drive ──

    #[tokio::test]
    async fn drive_keeps_reading_after_a_failed_turn_and_exits_with_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_reply(&server, "reply").await;
        let dir = tempfile::tempdir().unwrap();
        let conversation = conversation(server.uri(), &test_store(dir.path())).await;
        let sink = CapturingSink::new();
        let input: &[u8] = b"first\n/clear\n//literal\n";

        let exit = drive(conversation, &sink, input, &[PathBuf::from("drifted.rs")])
            .await
            .unwrap();

        assert_eq!(exit, Exit::Failure);
        let requests = request_bodies(&server).await;
        assert_eq!(requests.len(), 2, "/clear must not reach the model");
        let last = requests[1]["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"][0]["text"], "//literal");
        let errors: Vec<String> = sink
            .events()
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Error(msg) => Some(msg),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].ends_with(": drifted.rs"), "{errors:?}");
        assert!(errors[1].contains("HTTP 500"), "{errors:?}");
        assert!(
            errors[2].contains("only in the TUI. Use //clear"),
            "{errors:?}"
        );
        assert!(
            sink.events()
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnComplete))
        );
    }
}
