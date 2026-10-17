//! How an operation in flight stops early: the [`TurnAbort`] reasons, and the race of its future
//! against the cancel and shutdown ops.

use tokio::sync::mpsc;
use tracing::warn;

use crate::permission::Mode;
use crate::protocol::{Event, EventSink, Op};

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

/// What an operation in flight keeps from the ops it receives: the prompts queued for its next
/// round boundary, and the latest mode switch, which applies before the next gated tool call or
/// once the operation ends.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    pub(super) prompts: Vec<String>,
    pub(super) mode: Option<Mode>,
}

impl Inbox {
    /// Keeps `mode` for the next gated call and confirms it on `sink` at once, since no call is
    /// weighed between the two.
    pub(super) fn switch_mode(&mut self, mode: Mode, sink: &dyn EventSink) {
        self.mode = Some(mode);
        sink.emit(Event::ModeChanged(mode), "mode-changed");
    }
}

/// Races `fut` against incoming ops. An interrupt or shutdown produces a `TurnAbort`, and a queued
/// prompt or a mode switch goes to `inbox`. `fut` must be cancel-safe.
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
            // Biased: ops observed before same-poll-ready futures.
            biased;
            op = ops.recv() => match op {
                Some(Op::Interrupt) => return Err(TurnAbort::Cancelled),
                Some(Op::Shutdown) | None => return Err(TurnAbort::Shutdown),
                Some(Op::Queue(text)) => inbox.prompts.push(text),
                Some(Op::SetMode(mode)) => inbox.switch_mode(mode, sink),
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
    use crate::protocol::CapturingSink;

    // ── TurnAbort ──

    #[test]
    fn turn_abort_display_alternate_propagates_anyhow_cause_chain() {
        // {abort:#} must delegate to the inner anyhow Error's alternate form so the full cause
        // chain reaches the user. Plain {abort} surfaces only the outermost context.
        let inner = anyhow!("HTTP 503 from upstream");
        let chained = inner.context("request failed");
        let abort = TurnAbort::Failed(chained);

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

    // ── await_unless_aborted ──

    #[tokio::test]
    async fn await_unless_aborted_keeps_queued_prompts_and_the_latest_mode_switch() {
        let (tx, mut rx) = mpsc::channel(3);
        tx.try_send(Op::Queue("next".to_owned())).unwrap();
        tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
        tx.try_send(Op::SetMode(Mode::Yolo)).unwrap();
        let mut inbox = Inbox::default();
        let sink = CapturingSink::new();

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
