//! TUI driver: the [`App`] event loop beside an agent task that owns the [`Conversation`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{Conversation, Exit, TurnEnd, report_drift, shutdown_signal};
use crate::agent::event::UserAction;
use crate::config::ConfigSnapshot;
use crate::session::entry::CompactInfo;
use crate::session::store::SessionStore;
use crate::slash::LiveSessionInfo;
use crate::tool::ToolMetadata;
use crate::tui::app::{App, AppHistory};
use crate::tui::event::ChannelSink;
use crate::tui::terminal;
use crate::tui::theme::Theme;
use crate::util;
use crate::util::path::tildify;

/// Resumed state the chat view repaints on startup, besides the conversation's messages.
pub(crate) struct History {
    pub(crate) compact: Option<CompactInfo>,
    pub(crate) tool_metadata: HashMap<String, ToolMetadata>,
    pub(crate) title: Option<String>,
}

/// Runs the TUI until the user quits or a signal arrives, then finalizes the live session.
pub(crate) async fn run(
    conversation: Conversation,
    store: SessionStore,
    theme: &Theme,
    config: ConfigSnapshot,
    show_thinking: bool,
    history: History,
    drifted: &[PathBuf],
) -> Result<Exit> {
    terminal::install_panic_hook();

    let (sink, agent_rx) = crate::tui::event::channel();
    let (user_tx, user_rx) = mpsc::channel::<UserAction>(32);

    let cwd = &conversation.cwd;
    let session_info = LiveSessionInfo {
        cwd: tildify(cwd),
        git_cwd: Some(cwd.clone()),
        git_branch: util::git::current_branch(cwd),
        version: env!("CARGO_PKG_VERSION"),
        session_id: conversation.session.session_id().to_owned(),
        config,
    };

    let mut tui = terminal::init()?;
    let mut app = App::new(
        theme,
        session_info,
        show_thinking,
        agent_rx,
        user_tx,
        Arc::clone(&conversation.tools),
        AppHistory {
            messages: &conversation.messages,
            compact: history.compact.as_ref(),
            tool_metadata: &history.tool_metadata,
            title: history.title,
        },
    );

    report_drift(&sink, drifted);
    let agent = tokio::spawn(serve_actions(conversation, sink, user_rx, store));

    let result = tokio::select! {
        result = app.run(&mut tui) => result.map(|()| Exit::Success),
        signal = shutdown_signal() => {
            debug!("TUI received {signal:?}, tearing down");
            Ok(Exit::Signal(signal))
        }
    };

    terminal::restore();

    // The agent task reads the closed action channel as `Quit`, then finalizes and returns.
    drop(app);
    if let Err(e) = agent.await {
        warn!("agent task panicked: {e}");
    }

    result
}

/// Applies actions until `Quit` or the channel closes, then finalizes whichever session is live.
async fn serve_actions(
    mut conversation: Conversation,
    sink: ChannelSink,
    mut user_rx: mpsc::Receiver<UserAction>,
    store: SessionStore,
) {
    while let Some(action) = user_rx.recv().await {
        match action {
            UserAction::SubmitPrompt(text) => {
                let end = conversation.submit_prompt(&text, &sink, &mut user_rx).await;
                if end == TurnEnd::Quit {
                    break;
                }
            }
            UserAction::Compact { instructions } => {
                let end = conversation
                    .compact(&sink, &mut user_rx, instructions)
                    .await;
                if end == TurnEnd::Quit {
                    break;
                }
            }
            UserAction::Clear => conversation.clear(&sink, &store).await,
            UserAction::Resume { session_id } => {
                conversation.resume(&sink, &store, &session_id).await;
            }
            UserAction::Rename { title } => conversation.rename(&sink, title).await,
            UserAction::SwapConfig { model, effort } => {
                conversation.swap_config(&sink, model, effort);
            }
            // The App applies theme actions itself, and an approval only matters mid-turn, where
            // `agent_turn` consumes it.
            UserAction::Cancel
            | UserAction::ConfirmExit
            | UserAction::PreviewTheme { .. }
            | UserAction::SwapTheme { .. }
            | UserAction::ApprovalDecision { .. } => {}
            UserAction::Quit => break,
        }
    }

    if let Some(failure) = conversation.finalize().await {
        warn!("session finish failed: {failure}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::testing::{conversation, mount_reply};
    use super::*;
    use crate::agent::event::AgentEvent;
    use crate::client::anthropic::testing::text_stream_body;
    use crate::file_tracker::testing::seed_full_read;
    use crate::session::store::test_store;

    async fn next_matching<T>(
        event_rx: &mut mpsc::Receiver<AgentEvent>,
        mut pick: impl FnMut(AgentEvent) -> Option<T>,
    ) -> T {
        loop {
            let event = event_rx.recv().await.expect("agent task ended early");
            if let Some(found) = pick(event) {
                return found;
            }
        }
    }

    // ── serve_actions ──

    #[tokio::test]
    async fn serve_actions_finalizes_the_session_live_when_the_channel_closes() {
        let server = MockServer::start().await;
        mount_reply(&server, "reply").await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let conversation = conversation(server.uri(), &store).await;
        let tracker = Arc::clone(&conversation.file_tracker);
        let read_path = dir.path().join("read.txt");
        std::fs::write(&read_path, "contents").unwrap();
        let (sink, mut event_rx) = crate::tui::event::channel();
        let (user_tx, user_rx) = mpsc::channel(8);
        let agent = tokio::spawn(serve_actions(conversation, sink, user_rx, store.clone()));

        user_tx.send(UserAction::Clear).await.unwrap();
        let live_id = next_matching(&mut event_rx, |event| match event {
            AgentEvent::SessionRolled { id } => Some(id),
            _ => None,
        })
        .await;
        seed_full_read(&tracker, &read_path);
        user_tx
            .send(UserAction::SubmitPrompt("after clear".to_owned()))
            .await
            .unwrap();
        next_matching(&mut event_rx, |event| {
            matches!(event, AgentEvent::TurnComplete).then_some(())
        })
        .await;
        drop(user_tx);
        agent.await.unwrap();

        let snapshots = store.load_session_data(&live_id).unwrap().file_snapshots;
        let paths: Vec<_> = snapshots.iter().map(|snapshot| &snapshot.path).collect();
        assert_eq!(paths, [&read_path]);
    }

    #[tokio::test]
    async fn serve_actions_finalizes_when_the_channel_closes_mid_stream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(text_stream_body("", "too late"))
                    .insert_header("content-type", "text/event-stream")
                    .set_delay(Duration::from_secs(60)),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let conversation = conversation(server.uri(), &store).await;
        let live_id = conversation.session.session_id().to_owned();
        let tracker = Arc::clone(&conversation.file_tracker);
        let read_path = dir.path().join("read.txt");
        std::fs::write(&read_path, "contents").unwrap();
        seed_full_read(&tracker, &read_path);
        let (sink, _event_rx) = crate::tui::event::channel();
        let (user_tx, user_rx) = mpsc::channel(8);
        let agent = tokio::spawn(serve_actions(conversation, sink, user_rx, store.clone()));

        user_tx
            .send(UserAction::SubmitPrompt("mid stream".to_owned()))
            .await
            .unwrap();
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        drop(user_tx);
        tokio::time::timeout(Duration::from_secs(5), agent)
            .await
            .expect("the agent task must stop waiting on the stream")
            .unwrap();

        let data = store.load_session_data(&live_id).unwrap();
        assert_eq!(data.messages.len(), 1);
        let paths: Vec<_> = data
            .file_snapshots
            .iter()
            .map(|snapshot| &snapshot.path)
            .collect();
        assert_eq!(paths, [&read_path]);
    }
}
