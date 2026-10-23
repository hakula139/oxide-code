//! Test-only [`SessionHandle`] constructors.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Checkpoint, CompactOutcome, Outcome, RecordOutcome, SessionHandle, SharedState};
use crate::file_tracker::FileTracker;
use crate::file_tracker::testing::record_tracked_file;
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SessionContext;
use crate::session::actor::{Record, SessionCmd};
use crate::session::entry::CompactBoundary;

/// A `/compact` boundary for `summary` with no focus or task list.
pub(crate) fn boundary(summary: &str) -> CompactBoundary {
    CompactBoundary {
        summary: summary.to_owned(),
        instructions: None,
        tasks: Vec::new(),
        context: SessionContext::default(),
    }
}

/// Records a Write call of `tool_use_id` on `handle` that leaves `alpha` at `path`, as `tracker`
/// records it. An `answered` call also gets its tool result.
pub(crate) async fn record_write_call(
    handle: &SessionHandle,
    tracker: &FileTracker,
    tool_use_id: &str,
    path: &Path,
    answered: bool,
) {
    handle
        .record_message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                tool_use_id,
                "write",
                serde_json::Value::Null,
            )],
        })
        .await;
    record_tracked_file(tracker, path, b"alpha", tool_use_id);
    if answered {
        handle
            .record_message(Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result(tool_use_id, "wrote", false)],
            })
            .await;
    }
}

/// Handle whose actor channel is already closed, so every write fails as actor-gone.
pub(crate) fn dead(session_id: &str) -> SessionHandle {
    let (cmd_tx, _) = mpsc::channel(1);
    handle(session_id, cmd_tx, None)
}

/// Handle whose actor task panicked.
pub(crate) fn panicked(session_id: &str) -> SessionHandle {
    let (cmd_tx, _) = mpsc::channel(1);
    let join = tokio::spawn(async { panic!("deliberate actor panic for testing") });
    handle(session_id, cmd_tx, Some(join))
}

/// Handle on a stand-in actor that acks the first `succeed` cmds, then drops each cmd without
/// acking it. It always honors `Shutdown`.
pub(crate) fn acks_then_drops(session_id: &str, succeed: usize) -> SessionHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCmd>(8);
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = Arc::clone(&count);
    let join = tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let SessionCmd::Shutdown { ack } = cmd {
                _ = ack.send(());
                break;
            }
            let n = count_clone.fetch_add(1, Ordering::SeqCst);
            if n >= succeed {
                drop(cmd);
                continue;
            }
            match cmd {
                SessionCmd::Record(Record { ack, .. }) => {
                    _ = ack.send(RecordOutcome {
                        checkpoint: None,
                        ai_title_seed: None,
                        failure: None,
                    });
                }
                SessionCmd::Rewind { ack, .. }
                | SessionCmd::ToolMetadata { ack, .. }
                | SessionCmd::Usage { ack, .. }
                | SessionCmd::AppendAiTitle { ack, .. }
                | SessionCmd::SetManualTitle { ack, .. }
                | SessionCmd::Archive { ack }
                | SessionCmd::Finish { ack, .. } => {
                    _ = ack.send(Outcome { failure: None });
                }
                SessionCmd::Compact { ack, .. } => {
                    _ = ack.send(CompactOutcome {
                        pre_count: 0,
                        checkpoint: Some(Checkpoint {
                            tip: None,
                            message_count: 1,
                            first_user_prompt_seen: true,
                        }),
                        failure: None,
                    });
                }
                SessionCmd::Shutdown { .. } => unreachable!("filtered above"),
            }
        }
    });

    handle(session_id, cmd_tx, Some(join))
}

/// A handle on a stand-in session with no file, whose artifact directory is in the temp directory.
fn handle(
    session_id: &str,
    cmd_tx: mpsc::Sender<SessionCmd>,
    join: Option<JoinHandle<()>>,
) -> SessionHandle {
    SessionHandle {
        cmd_tx,
        session_id: Arc::from(session_id),
        artifact_dir: std::env::temp_dir().join(session_id).into(),
        shared: Arc::new(SharedState::default()),
        actor_join: Arc::new(Mutex::new(join)),
        title_task: Arc::default(),
    }
}
