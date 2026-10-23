//! Session actor that owns [`SessionState`] and its writer. Each `recv()` wakeup drains the queued
//! [`SessionCmd`]s into one batch and appends it in one write, so an isolated write flushes at once
//! with no interval timer. A compaction and `/archive` end their batch so a failed one can be
//! unqueued, and cmds after a compaction see its committed synthetic root.

use std::sync::Arc;

use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use uuid::Uuid;

use super::entry::{CompactBoundary, Entry, UsageRecord};
use super::handle::{Checkpoint, CompactOutcome, Outcome, RecordOutcome, SharedState};
use super::state::SessionState;
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;

// ── SessionCmd ──

/// Cross-task protocol for [`super::handle::SessionHandle`]. Acks fire after the batch flush.
pub(super) enum SessionCmd {
    Record(Record),
    /// Moves the chain anchor back to `checkpoint` and records the move.
    Rewind {
        checkpoint: Checkpoint,
        ack: oneshot::Sender<Outcome>,
    },
    /// Tool-result sidecars from one agent turn.
    ToolMetadata {
        items: Vec<(String, ToolMetadata)>,
        ack: oneshot::Sender<Outcome>,
    },
    Usage {
        records: Vec<UsageRecord>,
        ack: oneshot::Sender<Outcome>,
    },
    AppendAiTitle {
        title: String,
        ack: oneshot::Sender<Outcome>,
    },
    /// User-supplied title from `/rename`.
    SetManualTitle {
        title: String,
        ack: oneshot::Sender<Outcome>,
    },
    /// Bare `/archive`, sent just before the roll that finalizes the session. Ends its batch, and
    /// its ack carries every write failure.
    Archive {
        ack: oneshot::Sender<Outcome>,
    },
    Finish {
        /// Drained tracker snapshots, written as one `FileSnapshot` entry each plus a `Summary`.
        snapshots: Vec<FileSnapshot>,
        ack: oneshot::Sender<Outcome>,
    },
    /// Writes a compaction boundary and the synthetic post-compact message, ends its batch, and
    /// acks the pre-compact message count.
    Compact {
        boundary: CompactBoundary,
        synthetic_message: Message,
        ack: oneshot::Sender<CompactOutcome>,
    },
    /// Drains pending writes, acks, then exits the actor loop so shutdown returns without
    /// waiting for orphaned clones to drop.
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

/// A conversation message for the actor to record.
pub(super) struct Record {
    pub(super) msg: Message,
    /// Written ahead of `msg`, so prompt state only lands alongside the message carrying it.
    pub(super) context: Option<SessionContext>,
    /// The first-prompt title in place of `msg`'s text.
    pub(super) title: Option<String>,
    pub(super) ack: oneshot::Sender<RecordOutcome>,
}

// ── Actor Loop ──

/// Pending ack for one absorbed cmd, fired once the batch flush returns.
enum PendingAck {
    Record {
        ack: oneshot::Sender<RecordOutcome>,
        checkpoint: Checkpoint,
        ai_title_seed: Option<String>,
    },
    Outcome(oneshot::Sender<Outcome>),
    Archive(oneshot::Sender<Outcome>),
    Compact {
        ack: oneshot::Sender<CompactOutcome>,
        pre_count: u32,
        synthetic_uuid: Uuid,
    },
    Shutdown(oneshot::Sender<()>),
}

enum BatchFlow {
    Continue,
    FlushNow,
}

/// Actor task body. Exits when the channel closes or a [`SessionCmd::Shutdown`] is absorbed.
pub(super) async fn run(
    mut state: SessionState,
    mut rx: mpsc::Receiver<SessionCmd>,
    shared: Arc<SharedState>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Batch::default();
        let mut flow = batch.absorb(first, &mut state, &shared);
        // A failed compact or archive leaves its entries at the queue tail for `settle_acks` to
        // drop, which works only because each one ends its batch.
        while matches!(flow, BatchFlow::Continue)
            && let Ok(next) = rx.try_recv()
        {
            flow = batch.absorb(next, &mut state, &shared);
        }
        let failure = match state.flush_entries(batch.entries) {
            Err(e) => {
                let msg = format!("{e:#}");
                warn!("session write batch failed: {msg}");
                shared.record_flush_failure(&msg);
                Some(msg)
            }
            Ok(()) => None,
        };
        settle_acks(batch.acks, &mut state, failure.as_deref(), &shared);
        if batch.should_exit {
            break;
        }
    }
}

#[derive(Default)]
struct Batch {
    entries: Vec<Entry>,
    acks: Vec<PendingAck>,
    should_exit: bool,
}

impl Batch {
    fn absorb(
        &mut self,
        cmd: SessionCmd,
        state: &mut SessionState,
        shared: &SharedState,
    ) -> BatchFlow {
        let now = OffsetDateTime::now_utc();
        match cmd {
            SessionCmd::Record(record) => {
                let manual_title_set = shared.manual_title_set();
                self.absorb_record(record, state, manual_title_set, now);
                BatchFlow::Continue
            }
            SessionCmd::Rewind { checkpoint, ack } => {
                self.entries.push(state.rewind_entry(checkpoint, now));
                self.acks.push(PendingAck::Outcome(ack));
                BatchFlow::Continue
            }
            SessionCmd::ToolMetadata { items, ack } => {
                self.absorb_tool_metadata(items, ack, now);
                BatchFlow::Continue
            }
            SessionCmd::Usage { records, ack } => {
                self.entries
                    .extend(records.into_iter().map(|usage| Entry::Usage {
                        uuid: Uuid::new_v4(),
                        usage,
                        timestamp: now,
                    }));
                self.acks.push(PendingAck::Outcome(ack));
                BatchFlow::Continue
            }
            SessionCmd::AppendAiTitle { title, ack } => {
                if shared.manual_title_set() {
                    _ = ack.send(Outcome { failure: None });
                    return BatchFlow::Continue;
                }
                self.entries.push(Entry::Title {
                    title,
                    updated_at: now,
                });
                self.acks.push(PendingAck::Outcome(ack));
                BatchFlow::Continue
            }
            SessionCmd::SetManualTitle { title, ack } => {
                match state.try_defer_title(title) {
                    None => _ = ack.send(Outcome { failure: None }),
                    Some(title) => {
                        self.entries.push(Entry::Title {
                            title,
                            updated_at: now,
                        });
                        self.acks.push(PendingAck::Outcome(ack));
                    }
                }
                BatchFlow::Continue
            }
            SessionCmd::Archive { ack } => {
                self.entries.push(Entry::Archive {
                    archived: true,
                    at: now,
                });
                self.acks.push(PendingAck::Archive(ack));
                BatchFlow::FlushNow
            }
            SessionCmd::Finish { snapshots, ack } => {
                self.entries.extend(state.finish_entries(snapshots, now));
                self.acks.push(PendingAck::Outcome(ack));
                BatchFlow::Continue
            }
            SessionCmd::Compact {
                boundary,
                synthetic_message,
                ack,
            } => {
                let pre_count = state.message_count();
                let (compact_entries, synthetic_uuid) =
                    state.compact_entries(boundary, synthetic_message, now);
                self.entries.extend(compact_entries);
                self.acks.push(PendingAck::Compact {
                    ack,
                    pre_count,
                    synthetic_uuid,
                });
                BatchFlow::FlushNow
            }
            SessionCmd::Shutdown { ack } => {
                self.acks.push(PendingAck::Shutdown(ack));
                self.should_exit = true;
                BatchFlow::Continue
            }
        }
    }

    /// Queues the record's message behind the prompt context it is the first to carry, if any.
    fn absorb_record(
        &mut self,
        record: Record,
        state: &mut SessionState,
        manual_title_set: bool,
        now: OffsetDateTime,
    ) {
        let Record {
            msg,
            context,
            title,
            ack,
        } = record;
        let checkpoint = state.checkpoint();
        let (msg_entries, ai_title_seed) =
            state.queue_message_entries(&msg, title.as_deref(), now, manual_title_set);
        if let Some(context) = context {
            self.entries.push(Entry::Context {
                context: Box::new(context),
                timestamp: now,
            });
        }
        self.entries.extend(msg_entries);
        self.acks.push(PendingAck::Record {
            ack,
            checkpoint,
            ai_title_seed,
        });
    }

    /// Queues one sidecar per item, skipping default metadata since it adds no display fields.
    /// Acks at once when nothing needs a flush.
    fn absorb_tool_metadata(
        &mut self,
        items: Vec<(String, ToolMetadata)>,
        ack: oneshot::Sender<Outcome>,
        now: OffsetDateTime,
    ) {
        let queued = self.entries.len();
        self.entries.extend(
            items
                .into_iter()
                .filter(|(_, metadata)| *metadata != ToolMetadata::default())
                .map(|(tool_use_id, metadata)| Entry::ToolResultMetadata {
                    tool_use_id,
                    metadata,
                    timestamp: now,
                }),
        );
        if self.entries.len() > queued {
            self.acks.push(PendingAck::Outcome(ack));
        } else {
            _ = ack.send(Outcome { failure: None });
        }
    }
}

/// Acks each absorbed cmd once its batch flush returns `failure`. A compact commits when its batch
/// flushed or persistence has stopped, which leaves no file to keep it consistent with. Any other
/// failed compact and every failed archive are unqueued, matching what their acks tell the caller.
fn settle_acks(
    acks: Vec<PendingAck>,
    state: &mut SessionState,
    failure: Option<&str>,
    shared: &SharedState,
) {
    let stopped = state.persistence_stopped();
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
                    failure: surface_failure(failure, stopped, shared),
                });
            }
            PendingAck::Outcome(ack) => {
                _ = ack.send(Outcome {
                    failure: surface_failure(failure, stopped, shared),
                });
            }
            PendingAck::Archive(ack) => {
                if failure.is_some() {
                    state.abandon_archive();
                }
                _ = ack.send(Outcome {
                    failure: failure.map(str::to_owned),
                });
            }
            PendingAck::Compact {
                ack,
                pre_count,
                synthetic_uuid,
            } => {
                // Every failure that leaves the compact uncommitted reaches the agent, so each
                // discarded summary shows its cause.
                let (checkpoint, failure) = if failure.is_none() || stopped {
                    state.commit_compact(synthetic_uuid);
                    (
                        Some(state.checkpoint()),
                        surface_failure(failure, stopped, shared),
                    )
                } else {
                    state.abandon_compact();
                    (None, failure.map(str::to_owned))
                };
                _ = ack.send(CompactOutcome {
                    pre_count,
                    checkpoint,
                    failure,
                });
            }
            PendingAck::Shutdown(ack) => _ = ack.send(()),
        }
    }
}

/// Carries the first flush failure and the first failure after persistence stopped. Later ones
/// stay in the warn-log.
fn surface_failure(failure: Option<&str>, stopped: bool, shared: &SharedState) -> Option<String> {
    let msg = failure?;
    let first = if stopped {
        shared.surface_first_persistence_stop()
    } else {
        shared.surface_first_flush_failure()
    };
    first.then(|| msg.to_owned())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::session::handle::testing::boundary;
    use crate::session::state::MAX_UNWRITTEN_BYTES;
    use crate::session::store::SessionWriter;
    use crate::session::store::local_testing::{saved_entries, saved_titles, test_session_file};
    use crate::session::store::testing::{test_project_dir, test_store};

    mod run {
        use super::*;

        /// Runs the actor over `cmds` until the channel closes, returning its `SharedState`.
        async fn drive(state: SessionState, cmds: Vec<SessionCmd>) -> Arc<SharedState> {
            let shared = Arc::new(SharedState::default());
            drive_with(state, Arc::clone(&shared), cmds).await;
            shared
        }

        async fn drive_with(state: SessionState, shared: Arc<SharedState>, cmds: Vec<SessionCmd>) {
            let (tx, rx) = mpsc::channel(cmds.len().max(1));
            for cmd in cmds {
                tx.send(cmd).await.unwrap();
            }
            drop(tx);
            run(state, rx, shared).await;
        }

        /// Shared state with the manual-title latch that `SessionHandle::set_manual_title` sets.
        fn renamed() -> Arc<SharedState> {
            let shared = Arc::new(SharedState::default());
            shared.mark_manual_title_set();
            shared
        }

        fn record_cmd(text: &str) -> (SessionCmd, oneshot::Receiver<RecordOutcome>) {
            let (ack, rx) = oneshot::channel();
            (
                SessionCmd::Record(Record {
                    msg: Message::user(text),
                    context: None,
                    title: None,
                    ack,
                }),
                rx,
            )
        }

        fn finish_cmd() -> (SessionCmd, oneshot::Receiver<Outcome>) {
            let (ack, rx) = oneshot::channel();
            (
                SessionCmd::Finish {
                    snapshots: Vec::new(),
                    ack,
                },
                rx,
            )
        }

        fn shutdown_cmd() -> (SessionCmd, oneshot::Receiver<()>) {
            let (ack, rx) = oneshot::channel();
            (SessionCmd::Shutdown { ack }, rx)
        }

        #[tokio::test]
        async fn record_then_finish_writes_header_title_message_summary_in_order() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store, "m", None);
            let session_id = state.session_id.to_string();
            let (rec, _rec_rx) = record_cmd("hello");
            let (fin, _fin_rx) = finish_cmd();

            drive(state, vec![rec, fin]).await;

            let entries = saved_entries(dir.path(), &session_id);
            assert!(
                matches!(
                    entries.as_slice(),
                    [
                        Entry::Header(_),
                        Entry::Title { .. },
                        Entry::Message { .. },
                        Entry::Summary {
                            message_count: 1,
                            ..
                        },
                    ]
                ),
                "{entries:?}",
            );
        }

        #[tokio::test]
        async fn first_record_seeds_ai_title_and_subsequent_does_not() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store, "m", None);
            let (cmd_a, rx_a) = record_cmd("Fix login bug");
            let (cmd_b, rx_b) = record_cmd("follow up");

            drive(state, vec![cmd_a, cmd_b]).await;

            let outcome_a = rx_a.await.unwrap();
            let outcome_b = rx_b.await.unwrap();
            assert_eq!(outcome_a.ai_title_seed.as_deref(), Some("Fix login bug"));
            assert!(outcome_b.ai_title_seed.is_none());
        }

        #[tokio::test]
        async fn drains_burst_into_single_batch() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (a, _ra) = record_cmd("one");
            let (b, _rb) = record_cmd("two");
            let (c, _rc) = record_cmd("three");

            drive(state, vec![a, b, c]).await;

            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(data.messages.len(), 3);
        }

        #[tokio::test]
        async fn tool_metadata_with_default_short_circuits_without_writing() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (rec, _rec_rx) = record_cmd("trigger");
            let (meta_ack, meta_rx) = oneshot::channel();
            let meta_cmd = SessionCmd::ToolMetadata {
                items: vec![("t1".to_owned(), ToolMetadata::default())],
                ack: meta_ack,
            };

            drive(state, vec![rec, meta_cmd]).await;

            let outcome = meta_rx.await.unwrap();
            assert!(outcome.failure.is_none());
            let entries = saved_entries(dir.path(), &session_id);
            assert!(
                !entries
                    .iter()
                    .any(|entry| matches!(entry, Entry::ToolResultMetadata { .. })),
                "default metadata must not be written: {entries:?}",
            );
        }

        #[tokio::test]
        async fn appends_ai_title_after_record() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (rec, _rec_rx) = record_cmd("Fix login");
            let (ai_ack, _ai_rx) = oneshot::channel();
            let ai_cmd = SessionCmd::AppendAiTitle {
                title: "Fix the auth flow".to_owned(),
                ack: ai_ack,
            };
            let (fin, _fin_rx) = finish_cmd();

            drive(state, vec![rec, ai_cmd, fin]).await;

            // Tail scan picks the title with the latest `updated_at`, so the AI title wins.
            let title = store
                .list()
                .unwrap()
                .into_iter()
                .find(|s| s.session_id == session_id)
                .and_then(|s| s.title)
                .expect("title");
            assert_eq!(title.title, "Fix the auth flow");
        }

        #[tokio::test]
        async fn set_manual_title_alone_leaves_no_file_on_disk() {
            // The deferred title waits in `WriterStatus::Pending` and dies with the actor.
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let (manual_ack, manual_rx) = oneshot::channel();
            let manual_cmd = SessionCmd::SetManualTitle {
                title: "Doomed rename".to_owned(),
                ack: manual_ack,
            };
            let (fin, _fin_rx) = finish_cmd();

            drive(state, vec![manual_cmd, fin]).await;

            let outcome = manual_rx.await.expect("manual ack must arrive");
            assert!(
                outcome.failure.is_none(),
                "deferred-entry path acks healthily"
            );

            assert!(
                store.list().unwrap().is_empty(),
                "no message ever sent → no on-disk session",
            );
        }

        #[tokio::test]
        async fn set_manual_title_then_record_replaces_first_prompt_title() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (manual_ack, _manual_rx) = oneshot::channel();
            let manual_cmd = SessionCmd::SetManualTitle {
                title: "User-named".to_owned(),
                ack: manual_ack,
            };
            let (rec, _rec_rx) = record_cmd("hello");
            let (fin, _fin_rx) = finish_cmd();

            drive_with(state, renamed(), vec![manual_cmd, rec, fin]).await;

            let entries = saved_entries(dir.path(), &session_id);
            assert!(
                matches!(
                    entries[..],
                    [
                        Entry::Header(_),
                        Entry::Title { .. },
                        Entry::Message { .. },
                        ..
                    ]
                ),
                "deferred title must flush after the header and before the message: {entries:?}",
            );
            assert_eq!(
                saved_titles(&entries),
                ["User-named"],
                "deferred title replaces the first-prompt title",
            );
        }

        #[tokio::test]
        async fn two_set_manual_titles_keep_only_the_last() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (a_ack, _a_rx) = oneshot::channel();
            let (b_ack, _b_rx) = oneshot::channel();
            let a = SessionCmd::SetManualTitle {
                title: "First name".to_owned(),
                ack: a_ack,
            };
            let b = SessionCmd::SetManualTitle {
                title: "Final name".to_owned(),
                ack: b_ack,
            };
            let (rec, _rec_rx) = record_cmd("trigger");
            let (fin, _fin_rx) = finish_cmd();

            drive_with(state, renamed(), vec![a, b, rec, fin]).await;

            let entries = saved_entries(dir.path(), &session_id);
            assert_eq!(
                saved_titles(&entries),
                ["Final name"],
                "second rename overwrites the first",
            );
        }

        #[tokio::test]
        async fn set_manual_title_after_record_blocks_ai_title() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store, "m", None);
            let session_id = state.session_id.to_string();
            let shared = Arc::new(SharedState::default());
            let (tx, rx) = mpsc::channel(4);
            let actor = tokio::spawn(run(state, rx, Arc::clone(&shared)));
            let (rec, rec_rx) = record_cmd("Fix login");
            tx.send(rec).await.unwrap();
            rec_rx.await.unwrap();
            shared.mark_manual_title_set();
            let (manual_ack, _manual_rx) = oneshot::channel();
            let manual_cmd = SessionCmd::SetManualTitle {
                title: "User-picked title".to_owned(),
                ack: manual_ack,
            };
            let (ai_ack, _ai_rx) = oneshot::channel();
            let ai_cmd = SessionCmd::AppendAiTitle {
                title: "AI title that should lose".to_owned(),
                ack: ai_ack,
            };
            let (fin, _fin_rx) = finish_cmd();

            for cmd in [manual_cmd, ai_cmd, fin] {
                tx.send(cmd).await.unwrap();
            }
            drop(tx);
            actor.await.unwrap();

            let entries = saved_entries(dir.path(), &session_id);
            assert_eq!(
                saved_titles(&entries),
                ["Fix login", "User-picked title"],
                "AI title must not reach disk after a manual override",
            );
        }

        #[tokio::test]
        async fn shutdown_exits_loop_even_with_live_sender_clones() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store, "m", None);
            let shared = Arc::new(SharedState::default());
            let (tx, rx) = mpsc::channel::<SessionCmd>(4);
            let _orphan_clone = tx.clone();
            let actor = tokio::spawn(run(state, rx, Arc::clone(&shared)));

            let (cmd, ack_rx) = shutdown_cmd();
            tx.send(cmd).await.unwrap();
            ack_rx.await.unwrap();

            let exit = tokio::time::timeout(std::time::Duration::from_secs(1), actor).await;
            assert!(
                exit.is_ok(),
                "actor must exit on Shutdown without the orphan clone dropping",
            );
        }

        #[tokio::test]
        async fn shutdown_flushes_preceding_record_in_same_batch() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (rec, _rec_rx) = record_cmd("flush before exit");
            let (shut, _shut_rx) = shutdown_cmd();

            drive(state, vec![rec, shut]).await;

            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(data.messages.len(), 1, "record must reach disk before exit");
        }

        #[tokio::test]
        async fn finish_idempotent_writes_one_summary() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (rec, _rec_rx) = record_cmd("hi");
            let (f1, _r1) = finish_cmd();
            let (f2, _r2) = finish_cmd();

            drive(state, vec![rec, f1, f2]).await;

            let summary_count = saved_entries(dir.path(), &session_id)
                .iter()
                .filter(|entry| matches!(entry, Entry::Summary { .. }))
                .count();
            assert_eq!(summary_count, 1, "second finish must not duplicate");
        }

        #[tokio::test]
        async fn compact_writes_boundary_and_synthetic_message_to_disk() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (rec_a, _) = record_cmd("user one");
            let (rec_b, _) = record_cmd("user two");
            let (compact_ack, _compact_rx) = oneshot::channel();
            let compact_cmd = SessionCmd::Compact {
                boundary: CompactBoundary {
                    instructions: Some("focus on auth".to_owned()),
                    ..boundary("synth summary")
                },
                synthetic_message: Message::user("post"),
                ack: compact_ack,
            };

            drive(state, vec![rec_a, rec_b, compact_cmd]).await;

            let entries = saved_entries(dir.path(), &session_id);
            let boundaries: Vec<_> = entries
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Compact {
                        summary,
                        instructions,
                        ..
                    } => Some((summary.as_str(), instructions.as_deref())),
                    _ => None,
                })
                .collect();
            assert_eq!(boundaries, [("synth summary", Some("focus on auth"))]);
            assert!(
                matches!(
                    entries[..],
                    [
                        Entry::Header(_),
                        Entry::Title { .. },
                        Entry::Message { .. },
                        Entry::Message { .. },
                        Entry::Compact { .. },
                        Entry::Context { .. },
                        Entry::Message { .. },
                    ]
                ),
                "the refreshed context and the synthetic continuation \
                 follow the boundary: {entries:?}",
            );
        }

        #[tokio::test]
        async fn compact_acks_with_pre_compact_message_count() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store, "m", None);
            let (rec_a, _) = record_cmd("one");
            let (rec_b, _) = record_cmd("two");
            let (compact_ack, compact_rx) = oneshot::channel();
            let compact_cmd = SessionCmd::Compact {
                boundary: boundary("s"),
                synthetic_message: Message::user("synth"),
                ack: compact_ack,
            };

            drive(state, vec![rec_a, rec_b, compact_cmd]).await;

            let outcome = compact_rx.await.unwrap();
            assert_eq!(
                outcome.pre_count, 2,
                "pre_count reports the count BEFORE the compact reset",
            );
            assert!(outcome.failure.is_none());
        }

        #[tokio::test]
        async fn compact_flushes_before_following_record() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (rec_before, _) = record_cmd("before compact");
            let (compact_ack, _compact_rx) = oneshot::channel();
            let compact_cmd = SessionCmd::Compact {
                boundary: boundary("s"),
                synthetic_message: Message::user("synthetic summary"),
                ack: compact_ack,
            };
            let (rec_after, _) = record_cmd("after compact");

            drive(state, vec![rec_before, compact_cmd, rec_after]).await;

            let data = store.load_session_data(&session_id).unwrap();
            assert_eq!(data.messages.len(), 2);
            assert!(matches!(
                &data.messages[0].content[0],
                crate::message::ContentBlock::Text { text } if text == "synthetic summary"
            ));
            assert!(matches!(
                &data.messages[1].content[0],
                crate::message::ContentBlock::Text { text } if text == "after compact"
            ));
        }

        #[tokio::test]
        async fn failed_compact_is_dropped_while_earlier_records_retry() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);
            let session_id = state.session_id.to_string();
            let (tx, rx) = mpsc::channel(4);
            let actor = tokio::spawn(run(state, rx, Arc::new(SharedState::default())));
            let project_dir = test_project_dir(dir.path());
            std::fs::remove_dir_all(&project_dir).unwrap();

            let (rec, rec_rx) = record_cmd("before compact");
            tx.send(rec).await.unwrap();
            assert!(rec_rx.await.unwrap().failure.is_some());
            let (compact_ack, compact_rx) = oneshot::channel();
            tx.send(SessionCmd::Compact {
                boundary: boundary("s"),
                synthetic_message: Message::user("synthetic summary"),
                ack: compact_ack,
            })
            .await
            .unwrap();
            let outcome = compact_rx.await.unwrap();
            assert!(outcome.checkpoint.is_none());
            assert!(
                outcome.failure.is_some(),
                "the agent must see every compact failure, even after one was already surfaced",
            );

            std::fs::create_dir_all(&project_dir).unwrap();
            let (rec, rec_rx) = record_cmd("after compact");
            tx.send(rec).await.unwrap();
            assert!(rec_rx.await.unwrap().failure.is_none());
            drop(tx);
            actor.await.unwrap();

            let data = store.load_session_data(&session_id).unwrap();
            assert!(data.compact.is_none(), "failed compact must not reach disk");
            assert_eq!(
                data.messages,
                [
                    Message::user("before compact"),
                    Message::user("after compact")
                ]
            );
        }

        #[tokio::test]
        async fn compact_after_persistence_stopped_commits_in_memory() {
            async fn compact(tx: &mpsc::Sender<SessionCmd>) -> CompactOutcome {
                let (ack, rx) = oneshot::channel();
                tx.send(SessionCmd::Compact {
                    boundary: boundary("s"),
                    synthetic_message: Message::user("synthetic summary"),
                    ack,
                })
                .await
                .unwrap();
                rx.await.unwrap()
            }

            let dir = tempdir().unwrap();
            let state = SessionState::fresh(test_store(dir.path()), "m", None);
            let (tx, rx) = mpsc::channel(1);
            let actor = tokio::spawn(run(state, rx, Arc::new(SharedState::default())));
            std::fs::remove_dir_all(test_project_dir(dir.path())).unwrap();
            for text in ["small".to_owned(), "x".repeat(MAX_UNWRITTEN_BYTES)] {
                let (rec, rec_rx) = record_cmd(&text);
                tx.send(rec).await.unwrap();
                assert!(rec_rx.await.unwrap().failure.is_some());
            }

            let first = compact(&tx).await;
            let (rec, rec_rx) = record_cmd("after compact");
            tx.send(rec).await.unwrap();
            assert!(rec_rx.await.unwrap().failure.is_none());
            let second = compact(&tx).await;
            drop(tx);
            actor.await.unwrap();

            assert!(first.checkpoint.is_some());
            assert_eq!(first.failure, None, "the stop already surfaced");
            assert_eq!(first.pre_count, 2);
            assert!(second.checkpoint.is_some());
            assert_eq!(second.failure, None);
            assert_eq!(
                second.pre_count, 2,
                "the chain restarts at the synthetic root"
            );
        }

        #[tokio::test]
        async fn failed_archive_is_dropped_while_later_records_land() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let original = SessionState::fresh(store.clone(), "m", None);
            let session_id = original.session_id.to_string();
            let (rec, _) = record_cmd("before archive");
            drive(original, vec![rec]).await;
            let path = test_session_file(dir.path(), &session_id);
            let last_uuid = store.load_session_data(&session_id).unwrap().last_uuid;
            let state = SessionState::resumed(
                store.clone(),
                session_id.clone(),
                SessionWriter::failing_once(&path),
                last_uuid,
                1,
                true,
            );
            let (tx, rx) = mpsc::channel(4);
            let actor = tokio::spawn(run(state, rx, Arc::new(SharedState::default())));

            let (archive_ack, archive_rx) = oneshot::channel();
            tx.send(SessionCmd::Archive { ack: archive_ack })
                .await
                .unwrap();
            assert!(archive_rx.await.unwrap().failure.is_some());
            let (rec, rec_rx) = record_cmd("after archive");
            tx.send(rec).await.unwrap();
            assert!(rec_rx.await.unwrap().failure.is_none());
            drop(tx);
            actor.await.unwrap();

            let data = store.load_session_data(&session_id).unwrap();
            assert!(!data.archived, "failed archive must not reach disk");
            assert_eq!(
                data.messages,
                [
                    Message::user("before archive"),
                    Message::user("after archive")
                ]
            );
        }

        #[tokio::test]
        async fn failed_archive_reports_the_failure_after_an_earlier_one_surfaced() {
            let dir = tempdir().unwrap();
            let state = SessionState::fresh(test_store(dir.path()), "m", None);
            let (tx, rx) = mpsc::channel(4);
            let actor = tokio::spawn(run(state, rx, Arc::new(SharedState::default())));
            std::fs::remove_dir_all(test_project_dir(dir.path())).unwrap();

            let (rec, rec_rx) = record_cmd("unsaved");
            tx.send(rec).await.unwrap();
            assert!(rec_rx.await.unwrap().failure.is_some());
            let (archive_ack, archive_rx) = oneshot::channel();
            tx.send(SessionCmd::Archive { ack: archive_ack })
                .await
                .unwrap();
            drop(tx);
            actor.await.unwrap();

            assert!(
                archive_rx.await.unwrap().failure.is_some(),
                "the driver must see every archive failure, even after one was already surfaced",
            );
        }

        #[tokio::test]
        async fn flush_error_records_failure_in_ack() {
            let dir = tempdir().unwrap();
            let store = test_store(dir.path());
            let state = SessionState::fresh(store.clone(), "m", None);

            let project_dir = test_project_dir(dir.path());
            std::fs::remove_dir_all(&project_dir).unwrap();

            let (cmd, rx) = record_cmd("hello");
            drive(state, vec![cmd]).await;

            let outcome = rx.await.unwrap();
            assert!(
                outcome.failure.is_some(),
                "flush error must surface in the Record ack",
            );
        }

        #[tokio::test]
        async fn surfaces_the_persistence_stop_after_an_earlier_flush_failure() {
            let dir = tempdir().unwrap();
            let state = SessionState::fresh(test_store(dir.path()), "m", None);
            let (tx, rx) = mpsc::channel(1);
            let actor = tokio::spawn(run(state, rx, Arc::new(SharedState::default())));
            std::fs::remove_dir_all(test_project_dir(dir.path())).unwrap();

            let mut failures = Vec::new();
            let huge = "x".repeat(MAX_UNWRITTEN_BYTES);
            for text in ["small", huge.as_str(), "later"] {
                let (rec, rec_rx) = record_cmd(text);
                tx.send(rec).await.unwrap();
                failures.push(rec_rx.await.unwrap().failure);
            }
            drop(tx);
            actor.await.unwrap();

            assert!(
                failures[0].is_some(),
                "the first flush failure must surface"
            );
            let stop = &failures[1];
            assert!(
                stop.as_deref()
                    .is_some_and(|msg| msg.contains("persistence stopped")),
                "the stop must surface after an earlier failure, got: {stop:?}",
            );
            assert!(failures[2].is_none(), "failures after the stop stay silent");
        }
    }

    mod surface_failure {
        use super::*;

        #[test]
        fn surfaces_the_first_flush_failure_and_the_first_stop_then_silences() {
            let shared = SharedState::default();
            let surface = |msg, stopped| surface_failure(Some(msg), stopped, &shared);

            assert_eq!(surface("boom", false).as_deref(), Some("boom"));
            assert!(
                surface("boom", false).is_none(),
                "later flush failures stay silent"
            );
            assert_eq!(surface("stopped", true).as_deref(), Some("stopped"));
            assert!(
                surface("stopped", true).is_none(),
                "later stop failures stay silent"
            );
        }

        #[test]
        fn stays_silent_without_a_failure() {
            let shared = SharedState::default();
            assert!(surface_failure(None, false, &shared).is_none());
        }
    }
}
