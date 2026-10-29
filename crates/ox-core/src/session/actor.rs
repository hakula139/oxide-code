//! Session actor that owns [`SessionState`]. Each `recv()` wakeup drains the queued [`SessionCmd`]s
//! into one batch and commits it as one transaction on a blocking task, so an isolated write runs
//! with no interval timer.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::handle::{
    AiTitleOutcome, Checkpoint, CompactBoundary, CompactOutcome, Fork, RecordOutcome, SharedState,
    WriteReport,
};
use super::state::SessionState;
use super::store::UsageRecord;
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;

// ── SessionCmd ──

/// Cross-task protocol for [`super::handle::SessionHandle`]. Acks fire after the batch commits.
pub(super) enum SessionCmd {
    Record(Record),
    /// Moves the head back to `checkpoint`.
    Rewind {
        checkpoint: Checkpoint,
        ack: oneshot::Sender<WriteReport>,
    },
    /// One tool round's assistant message and the tool results answering it, which carry the
    /// results' metadata.
    RecordRound {
        assistant: Message,
        tool_results: Message,
        metadata: Vec<(String, ToolMetadata)>,
        ack: oneshot::Sender<WriteReport>,
    },
    Usage {
        records: Vec<UsageRecord>,
        ack: oneshot::Sender<WriteReport>,
    },
    /// AI title, which a manual title keeps from applying.
    AppendAiTitle {
        title: String,
        ack: oneshot::Sender<AiTitleOutcome>,
    },
    /// User-supplied title from `/rename`.
    SetManualTitle {
        title: String,
        ack: oneshot::Sender<WriteReport>,
    },
    /// Bare `/archive`, sent just before the roll that finalizes the session. Its ack carries every
    /// write failure.
    Archive {
        ack: oneshot::Sender<WriteReport>,
    },
    Finish {
        /// Drained tracker snapshots, which replace the session's saved ones.
        snapshots: Vec<FileSnapshot>,
        ack: oneshot::Sender<WriteReport>,
    },
    /// Writes the synthetic post-compact root carrying the compaction boundary, and acks the
    /// pre-compact message count.
    Compact {
        boundary: CompactBoundary,
        synthetic_message: Message,
        ack: oneshot::Sender<CompactOutcome>,
    },
    /// Commits pending writes, acks, then exits the actor loop so shutdown returns without waiting
    /// for orphaned clones to drop.
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

/// A conversation message for the actor to record.
pub(super) struct Record {
    pub(super) msg: Message,
    /// The prompt state `msg` is the first to carry.
    pub(super) context: Option<SessionContext>,
    /// The first-prompt title in place of `msg`'s text.
    pub(super) title: Option<String>,
    pub(super) ack: oneshot::Sender<RecordOutcome>,
}

// ── Actor Loop ──

/// Pending ack for one absorbed cmd, fired once the batch commit returns.
enum PendingAck {
    Record {
        ack: oneshot::Sender<RecordOutcome>,
        checkpoint: Checkpoint,
        ai_title_seed: Option<String>,
    },
    /// A report for a cmd that `moves_head` or leaves the head alone.
    Report {
        ack: oneshot::Sender<WriteReport>,
        moves_head: bool,
    },
    AiTitle {
        ack: oneshot::Sender<AiTitleOutcome>,
        applied: bool,
    },
    Archive(oneshot::Sender<WriteReport>),
    Compact {
        ack: oneshot::Sender<CompactOutcome>,
        pre_count: u32,
        checkpoint: Checkpoint,
    },
    Shutdown(oneshot::Sender<()>),
}

/// Actor task body. Exits when the channel closes or a [`SessionCmd::Shutdown`] is absorbed.
pub(super) async fn run(
    mut state: SessionState,
    mut rx: mpsc::Receiver<SessionCmd>,
    shared: Arc<SharedState>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Batch::default();
        batch.absorb(first, &mut state);
        while let Ok(next) = rx.try_recv() {
            batch.absorb(next, &mut state);
        }
        let (committed, result) = tokio::task::spawn_blocking(move || {
            let result = state.flush();
            (state, result)
        })
        .await
        .expect("session commit task panicked");
        state = committed;
        let (failure, fork) = match result {
            Ok(fork) => (None, fork),
            Err(e) => {
                let msg = format!("{e:#}");
                warn!(error = %msg, "session write batch failed");
                if shared.last_flush_failure().is_none() {
                    shared.record_flush_failure(&msg);
                }
                (Some(msg), None)
            }
        };
        if fork.is_some() {
            shared.set_live(state.session_id(), state.artifact_dir());
        }
        let title_manual = state.title_manual();
        settle_acks(batch.acks, failure.as_deref(), fork, title_manual, &shared);
        if batch.should_exit {
            break;
        }
    }
}

#[derive(Default)]
struct Batch {
    acks: Vec<PendingAck>,
    should_exit: bool,
}

impl Batch {
    fn absorb(&mut self, cmd: SessionCmd, state: &mut SessionState) {
        match cmd {
            SessionCmd::Record(Record {
                msg,
                context,
                title,
                ack,
            }) => {
                let checkpoint = state.checkpoint();
                let ai_title_seed =
                    state.queue_message(msg, context, BTreeMap::new(), title.as_deref());
                self.acks.push(PendingAck::Record {
                    ack,
                    checkpoint,
                    ai_title_seed,
                });
            }
            SessionCmd::Rewind { checkpoint, ack } => {
                state.rewind(checkpoint);
                self.report(ack, true);
            }
            SessionCmd::RecordRound {
                assistant,
                tool_results,
                metadata,
                ack,
            } => {
                let metadata = metadata
                    .into_iter()
                    .filter(|(_, metadata)| *metadata != ToolMetadata::default())
                    .collect();
                _ = state.queue_message(assistant, None, BTreeMap::new(), None);
                _ = state.queue_message(tool_results, None, metadata, None);
                self.report(ack, true);
            }
            SessionCmd::Usage { records, ack } => {
                state.queue_usage(records);
                self.report(ack, false);
            }
            SessionCmd::AppendAiTitle { title, ack } => {
                let applied = state.set_ai_title(title);
                self.acks.push(PendingAck::AiTitle { ack, applied });
            }
            SessionCmd::SetManualTitle { title, ack } => {
                state.set_title(title, true);
                self.report(ack, false);
            }
            SessionCmd::Archive { ack } => {
                state.queue_archive();
                self.acks.push(PendingAck::Archive(ack));
            }
            SessionCmd::Finish { snapshots, ack } => {
                state.finish(snapshots);
                self.report(ack, false);
            }
            SessionCmd::Compact {
                boundary,
                synthetic_message,
                ack,
            } => {
                let pre_count = state.compact(boundary, synthetic_message);
                self.acks.push(PendingAck::Compact {
                    ack,
                    pre_count,
                    checkpoint: state.checkpoint(),
                });
            }
            SessionCmd::Shutdown { ack } => {
                self.acks.push(PendingAck::Shutdown(ack));
                self.should_exit = true;
            }
        }
    }

    fn report(&mut self, ack: oneshot::Sender<WriteReport>, moves_head: bool) {
        self.acks.push(PendingAck::Report { ack, moves_head });
    }
}

/// Acks each absorbed cmd once its batch commit returns `failure`. The first ack of a cmd that
/// moved the head carries `fork`, which only such a cmd can trigger. An AI title applied only if
/// the title the batch left is not manual (`title_manual`), since the store or a later cmd in the
/// batch may have kept or set a manual one.
fn settle_acks(
    acks: Vec<PendingAck>,
    failure: Option<&str>,
    mut fork: Option<Fork>,
    title_manual: bool,
    shared: &SharedState,
) {
    let surfaced = failure.filter(|_| shared.surface_first_flush_failure());
    let report = |fork: Option<Fork>| WriteReport {
        failure: surfaced.map(str::to_owned),
        fork,
    };
    for pending in acks {
        match pending {
            PendingAck::Record {
                ack,
                checkpoint,
                ai_title_seed,
            } => {
                _ = ack.send(RecordOutcome {
                    checkpoint: Some(checkpoint),
                    ai_title_seed,
                    report: report(fork.take()),
                });
            }
            PendingAck::Report { ack, moves_head } => {
                let fork = if moves_head { fork.take() } else { None };
                _ = ack.send(report(fork));
            }
            PendingAck::AiTitle { ack, applied } => {
                _ = ack.send(AiTitleOutcome {
                    applied: applied && !title_manual,
                    report: report(None),
                });
            }
            PendingAck::Archive(ack) => {
                _ = ack.send(WriteReport {
                    failure: failure.map(str::to_owned),
                    fork: None,
                });
            }
            PendingAck::Compact {
                ack,
                pre_count,
                checkpoint,
            } => {
                _ = ack.send(CompactOutcome {
                    pre_count,
                    checkpoint: Some(checkpoint),
                    report: report(fork.take()),
                });
            }
            PendingAck::Shutdown(ack) => _ = ack.send(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::session::handle::start;
    use crate::session::state::STOPPED;
    use crate::session::store::local_testing::fail_writes;
    use crate::session::store::testing::{isolated_store, saved_messages};

    mod run {
        use super::*;

        #[tokio::test]
        async fn surfaces_a_failed_commit_once_but_to_every_archive() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            fail_writes(&store);

            let first = handle.record_message(Message::user("a")).await;
            let second = handle.record_message(Message::user("b")).await;
            let archived = handle.archive().await;
            handle.shutdown().await;

            assert_eq!(
                first.report.failure.as_deref(),
                Some("disk full: Error code 1811: constraint failed")
            );
            assert_eq!(second.report, WriteReport::default());
            assert_eq!(archived.failure.as_deref(), Some(STOPPED));
        }

        #[tokio::test]
        async fn commits_the_commands_queued_behind_the_first_in_order() {
            let (_dir, store) = isolated_store();
            let handle = start(&store, "m", None);
            let session_id = handle.session_id();

            let (first, second) = tokio::join!(
                handle.record_message(Message::user("a")),
                handle.record_message(Message::user("b")),
            );
            handle.shutdown().await;

            assert_eq!(first.report, WriteReport::default());
            assert_eq!(second.report, WriteReport::default());
            let prompts: Vec<_> = saved_messages(&store, &session_id)
                .into_iter()
                .map(|message| message["content"][0]["text"].clone())
                .collect();
            assert_eq!(prompts, ["a", "b"]);
        }
    }

    mod settle_acks {
        use super::*;

        fn checkpoint() -> Checkpoint {
            Checkpoint {
                tip: None,
                message_count: 0,
                first_user_prompt_seen: false,
            }
        }

        fn shared() -> SharedState {
            SharedState::new("source", PathBuf::from("/artifacts/source"))
        }

        fn report(moves_head: bool) -> (PendingAck, oneshot::Receiver<WriteReport>) {
            let (ack, rx) = oneshot::channel();
            (PendingAck::Report { ack, moves_head }, rx)
        }

        #[test]
        fn hands_the_fork_to_the_first_ack_that_moved_the_head() {
            let fork = Fork {
                from: "source".to_owned(),
                to: "fork".to_owned(),
            };
            let (usage, usage_rx) = report(false);
            let (round, round_rx) = report(true);
            let (record_ack, mut record_rx) = oneshot::channel();
            let record = PendingAck::Record {
                ack: record_ack,
                checkpoint: checkpoint(),
                ai_title_seed: None,
            };

            settle_acks(
                vec![usage, round, record],
                None,
                Some(fork.clone()),
                false,
                &shared(),
            );

            assert_eq!(usage_rx.blocking_recv().unwrap().fork, None);
            assert_eq!(round_rx.blocking_recv().unwrap().fork, Some(fork));
            assert_eq!(record_rx.try_recv().unwrap().report.fork, None);
        }

        #[test]
        fn hands_the_fork_to_a_record_or_compaction_that_leads_the_batch() {
            let fork = Fork {
                from: "source".to_owned(),
                to: "fork".to_owned(),
            };
            let (record_ack, record_rx) = oneshot::channel();
            let (compact_ack, compact_rx) = oneshot::channel();
            let acks = vec![
                PendingAck::Record {
                    ack: record_ack,
                    checkpoint: checkpoint(),
                    ai_title_seed: None,
                },
                PendingAck::Compact {
                    ack: compact_ack,
                    pre_count: 0,
                    checkpoint: checkpoint(),
                },
            ];
            let (compact_first, compact_first_rx) = oneshot::channel();

            settle_acks(acks, None, Some(fork.clone()), false, &shared());
            settle_acks(
                vec![PendingAck::Compact {
                    ack: compact_first,
                    pre_count: 0,
                    checkpoint: checkpoint(),
                }],
                None,
                Some(fork.clone()),
                false,
                &shared(),
            );

            assert_eq!(
                record_rx.blocking_recv().unwrap().report.fork,
                Some(fork.clone())
            );
            assert_eq!(compact_rx.blocking_recv().unwrap().report.fork, None);
            assert_eq!(
                compact_first_rx.blocking_recv().unwrap().report.fork,
                Some(fork)
            );
        }

        #[test]
        fn surfaces_a_failure_once_except_to_archive() {
            let shared = shared();
            let mut received = Vec::new();

            for _ in 0..2 {
                let (write, write_rx) = report(true);
                let (archive_ack, archive_rx) = oneshot::channel();
                settle_acks(
                    vec![write, PendingAck::Archive(archive_ack)],
                    Some("disk full"),
                    None,
                    false,
                    &shared,
                );
                received.push((
                    write_rx.blocking_recv().unwrap().failure,
                    archive_rx.blocking_recv().unwrap().failure,
                ));
            }

            let failure = Some("disk full".to_owned());
            assert_eq!(
                received,
                [(failure.clone(), failure.clone()), (None, failure)]
            );
        }
    }
}
