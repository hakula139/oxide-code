//! Test-only [`SessionHandle`] constructors for sibling modules
//! (`agent`, `session::title_generator`) that need a stand-in handle
//! without poking at private fields.
//!
//! Lives as a child of `handle` so it can read those private fields;
//! every item is `#[cfg(test)] pub(crate)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

use super::super::actor::SessionCmd;
use super::{Outcome, RecordOutcome, SessionHandle, SharedState};

/// Returns a handle whose actor channel is already closed, so every
/// write call immediately surfaces the actor-gone failure on the
/// first attempt. Useful for asserting the once-per-session
/// failure-surfacing behaviour without spinning up a real actor.
pub(crate) fn dead(session_id: &str) -> SessionHandle {
    let (cmd_tx, _) = mpsc::channel(1);
    SessionHandle {
        cmd_tx,
        session_id: Arc::from(session_id),
        shared: Arc::new(SharedState::default()),
        actor_join: Arc::new(std::sync::Mutex::new(None)),
    }
}

/// Returns a handle whose stand-in actor acks the first `succeed`
/// non-Shutdown cmds with a healthy outcome, then drops every
/// subsequent cmd without acking — the receiver's rx-await fallback
/// fires. Exercises the cross-task path where the actor task panics
/// or stalls between receiving a cmd and sending its ack.
///
/// Shutdown is honoured unconditionally so `handle.shutdown().await` returns on the stand-in.
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
