//! How an operation in flight stops early: the [`TurnAbort`] reasons, and the [`Mailbox`] that
//! races its futures against the cancel and shutdown ops, or against an op the caller takes.

use tokio::sync::mpsc;
use tracing::warn;

use crate::permission::Mode;
use crate::protocol::{Event, EventSink, Op};

/// Why a turn ended without a clean assistant reply. The engine keeps serving after `Cancelled` but
/// stops on `Shutdown`.
#[derive(Debug)]
pub(super) enum TurnAbort {
    Cancelled,
    Shutdown,
    /// The model declined the turn's request, so the engine rolls the turn back. Holds the notice.
    Refused(String),
    Failed(anyhow::Error),
}

pub(super) type AbortResult<T> = std::result::Result<T, TurnAbort>;

/// The ops one operation in flight races its futures against, and what it keeps from them: the
/// prompts queued for its next round boundary, and the latest mode switch, which applies before
/// the next gated tool call or once the operation ends.
pub(super) struct Mailbox<'a> {
    ops: &'a mut mpsc::Receiver<Op>,
    prompts: Vec<String>,
    mode: Option<Mode>,
}

impl<'a> Mailbox<'a> {
    pub(super) const fn new(ops: &'a mut mpsc::Receiver<Op>) -> Self {
        Self {
            ops,
            prompts: Vec::new(),
            mode: None,
        }
    }

    /// Races `fut` against incoming ops, dropping any op the operation cannot act on. `fut` must
    /// be cancel-safe.
    pub(super) async fn race<T>(
        &mut self,
        fut: impl Future<Output = T>,
        sink: &dyn EventSink,
    ) -> AbortResult<T> {
        self.race_or(fut, sink, |op| {
            // A `Resolve` here is a stale reply for an already-resolved call, and a `LoginCode` a
            // paste that arrived once the sign-in had its code.
            warn!(?op, "dropped mid-turn op");
            None
        })
        .await
    }

    /// Races `fut` like [`Self::race`], handing each op the mailbox does not keep to `on_op`, whose
    /// `Some` ends the wait in place of `fut`.
    pub(super) async fn race_or<T>(
        &mut self,
        fut: impl Future<Output = T>,
        sink: &dyn EventSink,
        mut on_op: impl FnMut(Op) -> Option<T>,
    ) -> AbortResult<T> {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                // Queued ops, an interrupt above all, must win over a `fut` that is already ready.
                biased;
                op = self.ops.recv() => {
                    if let Some(output) = self.absorb(op, sink)?.and_then(&mut on_op) {
                        return Ok(output);
                    }
                }
                output = &mut fut => return Ok(output),
            }
        }
    }

    /// The latest mode switch the operation received, which the caller applies.
    pub(super) const fn take_mode(&mut self) -> Option<Mode> {
        self.mode.take()
    }

    /// The prompts queued since the last drain, in arrival order.
    pub(super) fn drain_prompts(&mut self) -> std::vec::Drain<'_, String> {
        self.prompts.drain(..)
    }

    /// Handles an op received while an operation runs, where `None` means the channel closed. Keeps
    /// a queued prompt, and keeps a mode switch for the next gated call while confirming it on
    /// `sink` at once, since no gated call runs between the two. An interrupt or shutdown returns
    /// its abort, and any other op comes back for the caller.
    fn absorb(&mut self, op: Option<Op>, sink: &dyn EventSink) -> AbortResult<Option<Op>> {
        match op {
            Some(Op::Interrupt) => Err(TurnAbort::Cancelled),
            Some(Op::Shutdown) | None => Err(TurnAbort::Shutdown),
            Some(Op::Queue(text)) => {
                self.prompts.push(text);
                Ok(None)
            }
            Some(Op::SetMode(mode)) => {
                self.mode = Some(mode);
                sink.emit(Event::ModeChanged(mode));
                Ok(None)
            }
            Some(op) => Ok(Some(op)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SignInCode;
    use crate::protocol::testing::CapturingSink;

    mod mailbox {
        use super::*;

        mod race {
            use super::*;

            #[tokio::test]
            async fn keeps_queued_prompts_and_the_latest_mode_switch() {
                let (tx, mut rx) = mpsc::channel(3);
                tx.try_send(Op::Queue("next".to_owned())).unwrap();
                tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
                tx.try_send(Op::SetMode(Mode::Yolo)).unwrap();
                let mut mailbox = Mailbox::new(&mut rx);
                let sink = CapturingSink::default();

                let output = mailbox.race(std::future::ready(7), &sink).await;

                assert!(matches!(output, Ok(7)), "{output:?}");
                assert_eq!(mailbox.drain_prompts().collect::<Vec<_>>(), ["next"]);
                assert_eq!(mailbox.take_mode(), Some(Mode::Yolo));
                assert_eq!(mailbox.take_mode(), None, "taking the switch clears it");
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

            #[tokio::test]
            async fn aborts_on_interrupt_shutdown_or_a_closed_channel() {
                let sink = CapturingSink::default();
                let mut outcomes = Vec::new();

                for op in [Some(Op::Interrupt), Some(Op::Shutdown), None] {
                    let (tx, mut rx) = mpsc::channel(1);
                    match op {
                        Some(op) => tx.try_send(op).unwrap(),
                        None => drop(tx),
                    }
                    let outcome = Mailbox::new(&mut rx)
                        .race(std::future::pending::<()>(), &sink)
                        .await;
                    outcomes.push(outcome);
                }

                assert!(
                    matches!(
                        outcomes.as_slice(),
                        [
                            Err(TurnAbort::Cancelled),
                            Err(TurnAbort::Shutdown),
                            Err(TurnAbort::Shutdown)
                        ]
                    ),
                    "{outcomes:?}"
                );
            }
        }

        mod race_or {
            use super::*;

            #[tokio::test]
            async fn ends_with_the_first_op_the_handler_takes() {
                let (tx, mut rx) = mpsc::channel(3);
                tx.try_send(Op::Clear).unwrap();
                tx.try_send(Op::LoginCode(SignInCode("code#state".to_owned())))
                    .unwrap();
                tx.try_send(Op::Logout).unwrap();
                let sink = CapturingSink::default();
                let mut passed = Vec::new();

                let output = Mailbox::new(&mut rx)
                    .race_or(std::future::pending(), &sink, |op| match op {
                        Op::LoginCode(SignInCode(code)) => Some(code),
                        op => {
                            passed.push(op);
                            None
                        }
                    })
                    .await;

                assert!(
                    matches!(&output, Ok(code) if code == "code#state"),
                    "{output:?}"
                );
                assert_eq!(passed, [Op::Clear]);
                assert_eq!(rx.try_recv().ok(), Some(Op::Logout));
                assert_eq!(sink.events(), []);
            }
        }
    }
}
