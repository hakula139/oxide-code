//! Test-only [`SessionHandle`] constructors. Child of `handle` so it can build the private
//! fields directly; every item is `#[cfg(test)] pub(crate)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

use super::super::actor::SessionCmd;
use super::{Outcome, RecordOutcome, SessionHandle, SharedState};

/// Handle whose actor channel is already closed — every write surfaces actor-gone immediately,
/// so failure surfacing can be asserted without a real actor.
pub(crate) fn dead(session_id: &str) -> SessionHandle {
    let (cmd_tx, _) = mpsc::channel(1);
    SessionHandle {
        cmd_tx,
        session_id: Arc::from(session_id),
        shared: Arc::new(SharedState::default()),
        actor_join: Arc::new(std::sync::Mutex::new(None)),
    }
}

/// Stand-in actor that acks the first `succeed` non-Shutdown cmds, then drops cmds without
/// acking — exercises the rx-await fallback when the actor stalls between receive and ack.
/// Shutdown is always honoured so `handle.shutdown()` still returns.
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
                        ai_title_seed: None,
                        failure: None,
                    });
                }
                SessionCmd::ToolMetadata { ack, .. }
                | SessionCmd::AppendAiTitle { ack, .. }
                | SessionCmd::Finish { ack, .. } => {
                    _ = ack.send(Outcome { failure: None });
                }
                SessionCmd::Shutdown { .. } => unreachable!("filtered above"),
                SessionCmd::Panic => panic!("deliberate actor panic for testing"),
            }
        }
    });

    SessionHandle {
        cmd_tx,
        session_id: Arc::from(session_id),
        shared: Arc::new(SharedState::default()),
        actor_join: Arc::new(std::sync::Mutex::new(Some(join))),
    }
}
