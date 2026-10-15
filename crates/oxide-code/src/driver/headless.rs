//! Headless driver for `--prompt`: one turn, streamed to stdout.

use std::sync::Arc;

use super::{Conversation, Exit, TurnEnd, shutdown_signal};
use crate::agent::event::{AgentSink, StdioSink, inert_user_action_channel};
use crate::slash;

/// Runs `prompt` as a single turn. A failed or cancelled turn exits with failure, and a signal
/// exits with its own status.
pub(crate) async fn run(conversation: Conversation, show_thinking: bool, prompt: &str) -> Exit {
    let sink = StdioSink::new(show_thinking, Arc::clone(&conversation.tools));
    drive(conversation, &sink, prompt).await
}

async fn drive<S>(mut conversation: Conversation, sink: &S, prompt: &str) -> Exit
where
    S: AgentSink + Clone + 'static,
{
    let (_user_tx, mut user_rx) = inert_user_action_channel();
    let exit = tokio::select! {
        end = conversation.submit_prompt(slash::unescape_literal(prompt), sink, &mut user_rx) => {
            if end == TurnEnd::Completed {
                Exit::Success
            } else {
                Exit::Failure
            }
        }
        signal = shutdown_signal() => {
            eprintln!();
            Exit::Signal(signal)
        }
    };

    let failure = conversation.finalize().await;
    sink.session_write_error(failure.as_deref());
    exit
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::testing::{conversation, mount_reply};
    use super::*;
    use crate::agent::event::{AgentEvent, CapturingSink};
    use crate::session::store::testing::test_store;

    // ── drive ──

    #[tokio::test]
    async fn drive_exit_reflects_whether_the_turn_completed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount_reply(&server, "reply").await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());

        let failed = CapturingSink::new();
        let exit = drive(conversation(server.uri(), &store).await, &failed, "first").await;
        assert_eq!(exit, Exit::Failure);
        assert!(matches!(failed.events().last(), Some(AgentEvent::Error(_))));

        let completed = CapturingSink::new();
        let exit = drive(
            conversation(server.uri(), &store).await,
            &completed,
            "second",
        )
        .await;
        assert_eq!(exit, Exit::Success);
        assert!(matches!(
            completed.events().last(),
            Some(AgentEvent::TurnComplete)
        ));
    }
}
