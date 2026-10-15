//! Headless driver for `--prompt`: one turn, streamed to stdout.

use super::{Exit, render_turn, shutdown_signal};
use crate::agent::event::{AgentSink, StdioSink, UserAction};
use crate::engine::{Engine, EngineHandle, TurnEnd};
use crate::slash;

/// Runs `prompt` as a single turn. A failed or cancelled turn exits with failure, and a signal
/// exits with its own status.
pub(crate) async fn run(engine: Engine, show_thinking: bool, prompt: &str) -> Exit {
    let sink = StdioSink::new(show_thinking);
    drive(engine.spawn(&[]), &sink, prompt).await
}

async fn drive(mut handle: EngineHandle, sink: &dyn AgentSink, prompt: &str) -> Exit {
    let prompt = slash::unescape_literal(prompt).to_owned();
    // A send fails only once the engine has stopped, which `render_turn` then reports.
    _ = handle.ops.send(UserAction::SubmitPrompt(prompt)).await;
    let exit = tokio::select! {
        end = render_turn(&mut handle.events, sink) => {
            if end == Some(TurnEnd::Completed) {
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

    let failure = handle.shutdown().await;
    sink.session_write_error(failure.as_deref());
    exit
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::agent::event::{AgentEvent, CapturingSink};
    use crate::engine::testing::{
        engine, mount_reply, request_bodies, stopped_engine, with_pinned_home,
    };
    use crate::session::store::testing::test_store;

    // ── drive ──

    #[tokio::test]
    async fn drive_exit_reflects_whether_the_turn_completed() {
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

            let failed = CapturingSink::new();
            let engine_failed = engine(server.uri(), &store, dir.path()).await;
            let exit = Box::pin(drive(engine_failed.spawn(&[]), &failed, "first")).await;
            assert_eq!(exit, Exit::Failure);
            assert!(matches!(
                failed.events().last(),
                Some(AgentEvent::TurnFailed(_))
            ));

            let completed = CapturingSink::new();
            let engine_completed = engine(server.uri(), &store, dir.path()).await;
            let exit = Box::pin(drive(engine_completed.spawn(&[]), &completed, "//second")).await;
            assert_eq!(exit, Exit::Success);
            assert!(matches!(
                completed.events().last(),
                Some(AgentEvent::TurnComplete)
            ));
            let requests = request_bodies(&server).await;
            assert_eq!(requests[1]["messages"][1]["content"][0]["text"], "/second");

            let exit = Box::pin(drive(stopped_engine(), &CapturingSink::new(), "third")).await;
            assert_eq!(exit, Exit::Failure);
        })
        .await;
    }
}
