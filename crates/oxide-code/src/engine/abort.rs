//! How an operation in flight stops early: the [`TurnAbort`] reasons, and the race of its future
//! against the cancel and shutdown ops.

use tokio::sync::mpsc;
use tracing::warn;

use crate::permission::Mode;
use crate::protocol::{Event, EventSink, Op};

/// Why a turn ended without a clean assistant reply. The engine keeps serving after `Cancelled`
/// but stops on `Shutdown`.
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

/// What an operation in flight keeps from the ops it receives: the prompts queued for its next
/// round boundary, and the latest mode switch, which applies before the next gated tool call or
/// once the operation ends.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    pub(super) prompts: Vec<String>,
    pub(super) mode: Option<Mode>,
}

impl Inbox {
    /// Handles an op received while an operation runs, where `None` means the channel closed. Keeps
    /// a queued prompt, and keeps a mode switch for the next gated call while confirming it on
    /// `sink` at once, since no gated call runs between the two. An interrupt or shutdown returns
    /// its abort, and any other op comes back for the caller.
    pub(super) fn absorb(
        &mut self,
        op: Option<Op>,
        sink: &dyn EventSink,
    ) -> AbortResult<Option<Op>> {
        match op {
            Some(Op::Interrupt) => Err(TurnAbort::Cancelled),
            Some(Op::Shutdown) | None => Err(TurnAbort::Shutdown),
            Some(Op::Queue(text)) => {
                self.prompts.push(text);
                Ok(None)
            }
            Some(Op::SetMode(mode)) => {
                self.mode = Some(mode);
                sink.emit(Event::ModeChanged(mode), "mode-changed");
                Ok(None)
            }
            Some(
                op @ (Op::Prompt(_)
                | Op::Clear
                | Op::Archive
                | Op::Resume { .. }
                | Op::Compact { .. }
                | Op::Rename { .. }
                | Op::SetModel(_)
                | Op::Login
                | Op::Logout
                | Op::Resolve { .. }),
            ) => Ok(Some(op)),
        }
    }
}

/// Races `fut` against incoming ops, which [`Inbox::absorb`] takes in. `fut` must be cancel-safe.
pub(super) async fn await_unless_aborted<F, T>(
    fut: F,
    ops: &mut mpsc::Receiver<Op>,
    inbox: &mut Inbox,
    sink: &dyn EventSink,
) -> AbortResult<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(fut);
    loop {
        tokio::select! {
            // Queued ops, an interrupt above all, must win over a `fut` that is already ready.
            biased;
            op = ops.recv() => {
                // Unreachable under current wiring, and a `Resolve` here is a stale reply for an
                // already-resolved call.
                if let Some(op) = inbox.absorb(op, sink)? {
                    warn!("dropped mid-turn op: {op:?}");
                }
            }
            output = &mut fut => return Ok(output),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;
    use crate::protocol::testing::CapturingSink;

    // ── TurnAbort ──

    #[test]
    fn turn_abort_display_alternate_propagates_anyhow_cause_chain() {
        let abort = TurnAbort::Failed(anyhow!("HTTP 503 from upstream").context("request failed"));

        let plain = format!("{abort}");
        let alternate = format!("{abort:#}");

        assert_eq!(plain, "request failed");
        assert_eq!(alternate, "request failed: HTTP 503 from upstream");
    }

    #[test]
    fn turn_abort_display_cancelled_and_shutdown_use_static_labels() {
        assert_eq!(format!("{}", TurnAbort::Cancelled), "turn cancelled");
        assert_eq!(format!("{}", TurnAbort::Shutdown), "engine shut down");
    }

    // ── Inbox::absorb ──

    #[test]
    fn absorb_aborts_on_interrupt_shutdown_or_a_closed_channel() {
        let mut inbox = Inbox::default();
        let sink = CapturingSink::default();

        let interrupted = inbox.absorb(Some(Op::Interrupt), &sink);
        let shut_down = inbox.absorb(Some(Op::Shutdown), &sink);
        let closed = inbox.absorb(None, &sink);

        assert!(
            matches!(interrupted, Err(TurnAbort::Cancelled)),
            "{interrupted:?}"
        );
        assert!(
            matches!(shut_down, Err(TurnAbort::Shutdown)),
            "{shut_down:?}"
        );
        assert!(matches!(closed, Err(TurnAbort::Shutdown)), "{closed:?}");
    }

    #[test]
    fn absorb_hands_back_an_op_it_does_not_keep() {
        let mut inbox = Inbox::default();
        let sink = CapturingSink::default();

        let op = inbox.absorb(Some(Op::Clear), &sink);

        assert!(matches!(op, Ok(Some(Op::Clear))), "{op:?}");
        assert_eq!(inbox.prompts, [] as [String; 0]);
        assert_eq!(inbox.mode, None);
        assert_eq!(sink.events(), []);
    }

    // ── await_unless_aborted ──

    #[tokio::test]
    async fn await_unless_aborted_keeps_queued_prompts_and_the_latest_mode_switch() {
        let (tx, mut rx) = mpsc::channel(3);
        tx.try_send(Op::Queue("next".to_owned())).unwrap();
        tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
        tx.try_send(Op::SetMode(Mode::Yolo)).unwrap();
        let mut inbox = Inbox::default();
        let sink = CapturingSink::default();

        let output = await_unless_aborted(std::future::ready(7), &mut rx, &mut inbox, &sink).await;

        assert!(matches!(output, Ok(7)), "{output:?}");
        assert_eq!(inbox.prompts, ["next"]);
        assert_eq!(inbox.mode, Some(Mode::Yolo));
        assert!(
            matches!(
                sink.events().as_slice(),
                [
                    Event::ModeChanged(Mode::Plan),
                    Event::ModeChanged(Mode::Yolo)
                ]
            ),
            "{:?}",
            sink.events(),
        );
        drop(tx);
    }
}
