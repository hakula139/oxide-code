//! How an operation in flight stops early: the [`TurnAbort`] reasons, and the race of its future
//! against the cancel and shutdown ops.

use tokio::sync::mpsc;
use tracing::warn;

use crate::protocol::Op;

/// Why a turn ended without a clean assistant reply. `Cancelled` and `Shutdown` are user-driven
/// and distinct, so the engine keeps serving after a cancel but stops on shutdown. `Failed` wraps
/// any other error so the caller can render it via `{e:#}`.
#[derive(Debug, thiserror::Error)]
pub(super) enum TurnAbort {
    #[error("turn cancelled")]
    Cancelled,
    #[error("engine shut down")]
    Shutdown,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

pub(super) type AbortResult<T> = std::result::Result<T, TurnAbort>;

/// Races `fut` against incoming ops. An interrupt or shutdown produces a `TurnAbort`, and a queued
/// prompt buffers into `pending`. `fut` must be cancel-safe.
pub(super) async fn await_unless_aborted<F, T>(
    fut: F,
    ops: &mut mpsc::Receiver<Op>,
    pending: &mut Vec<String>,
) -> AbortResult<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        tokio::select! {
            // Biased: ops observed before same-poll-ready futures.
            biased;
            op = ops.recv() => match op {
                Some(Op::Interrupt) => return Err(TurnAbort::Cancelled),
                Some(Op::Shutdown) | None => return Err(TurnAbort::Shutdown),
                Some(Op::Queue(text)) => pending.push(text),
                // Unreachable under current wiring. Log so regressions surface. A `Resolve`
                // here is a stale reply for an already-resolved call.
                Some(
                    op @ (Op::Prompt(_)
                    | Op::Clear
                    | Op::Archive
                    | Op::Resume { .. }
                    | Op::Compact { .. }
                    | Op::Rename { .. }
                    | Op::SetModel { .. }
                    | Op::Login
                    | Op::Logout
                    | Op::Resolve { .. }),
                ) => warn!("dropped mid-turn op: {op:?}"),
            },
            output = &mut fut => return Ok(output),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    // ── TurnAbort ──

    #[test]
    fn turn_abort_display_alternate_propagates_anyhow_cause_chain() {
        // {abort:#} must delegate to the inner anyhow Error's alternate form so the full cause
        // chain reaches the user. Plain {abort} surfaces only the outermost context.
        let inner = anyhow!("HTTP 503 from upstream");
        let chained = inner.context("stream error");
        let abort = TurnAbort::Failed(chained);

        let plain = format!("{abort}");
        let alternate = format!("{abort:#}");

        assert_eq!(plain, "stream error");
        assert!(
            alternate.contains("stream error") && alternate.contains("HTTP 503 from upstream"),
            "alternate must include both layers: {alternate:?}",
        );
    }

    #[test]
    fn turn_abort_display_cancelled_and_shutdown_use_static_labels() {
        assert_eq!(format!("{}", TurnAbort::Cancelled), "turn cancelled");
        assert_eq!(format!("{}", TurnAbort::Shutdown), "engine shut down");
    }
}
