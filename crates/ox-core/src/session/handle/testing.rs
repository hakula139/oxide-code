//! Test-only [`SessionHandle`] constructors.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{CompactOutcome, Outcome, RecordOutcome, SessionHandle, SharedState};
use crate::session::actor::SessionCmd;

/// Handle whose actor channel is already closed, so every write fails as actor-gone.
pub(crate) fn dead(session_id: &str) -> SessionHandle {
    let (cmd_tx, _) = mpsc::channel(1);
    handle(session_id, cmd_tx, None)
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
                SessionCmd::Record { ack, .. } => {
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
                        failure: None,
                    });
                }
                SessionCmd::Shutdown { .. } => unreachable!("filtered above"),
                SessionCmd::Panic => panic!("deliberate actor panic for testing"),
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
        actor_join: Arc::new(std::sync::Mutex::new(join)),
        title_task: Arc::default(),
    }
}
