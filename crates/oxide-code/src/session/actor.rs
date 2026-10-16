//! Session actor — owns [`SessionState`] + writer, drains [`SessionCmd`]s, batches one flush
//! per `recv()` wakeup. Receive-and-drain coalesces a turn's queued cmds before the first flush,
//! except `/compact` and `/archive`. Each ends its batch so a failed one can be unqueued. Cmds
//! after a compact see its committed synthetic root.
//! Isolated writes flush immediately. No interval timer — see `docs/design/session/persistence.md`.

use std::sync::Arc;

use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;
use uuid::Uuid;

use super::entry::{Entry, TitleSource, UsageRecord};
use super::handle::{CompactOutcome, Outcome, RecordOutcome, SharedState};
use super::state::SessionState;
use crate::file_tracker::FileSnapshot;
use crate::message::Message;
use crate::prompt::SessionContext;
use crate::tool::ToolMetadata;

/// Cross-task protocol for [`super::handle::SessionHandle`]. Acks fire after the batch flush.
pub(super) enum SessionCmd {
    Record {
        msg: Message,
        /// Written ahead of `msg`, so prompt state only lands alongside the message carrying it.
        context: Option<SessionContext>,
        ack: oneshot::Sender<RecordOutcome>,
    },
    /// One or more tool-result sidecars from a single agent turn. Default-metadata items are
    /// skipped at absorb time.
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
    /// User-supplied title from `/rename`. Latches `manual_title_set` to suppress AI titles.
    SetManualTitle {
        title: String,
        ack: oneshot::Sender<Outcome>,
    },
    /// Bare `/archive`, sent just before the roll that finalizes the session. Ends its batch, and
    /// its ack carries every write failure, since the driver rolls only once the entry is on disk.
    Archive { ack: oneshot::Sender<Outcome> },
    Finish {
        /// Drained tracker snapshots; written as one `FileSnapshot` entry each plus a `Summary`.
        snapshots: Vec<FileSnapshot>,
        ack: oneshot::Sender<Outcome>,
    },
    /// `/compact`: write the compaction boundary + synthetic post-compact message in one
    /// batched flush, reset the chain anchor in `SessionState`, and ack the pre-compact
    /// message count for the post-compact UI line.
    Compact {
        summary: String,
        instructions: Option<String>,
        synthetic_message: Message,
        ack: oneshot::Sender<CompactOutcome>,
    },
    /// Drains pending writes, acks, then exits the actor loop so shutdown returns without
    /// waiting for orphaned clones to drop.
    Shutdown { ack: oneshot::Sender<()> },
    /// Test-only: panics inside the actor task so callers can exercise
    /// the `JoinError::is_panic()` path in `shutdown`.
    #[cfg(test)]
    Panic,
}

/// Pending ack for one absorbed cmd; fires once the batch flush returns.
enum PendingAck {
    Record {
        ack: oneshot::Sender<RecordOutcome>,
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

/// Actor task body. Owns [`SessionState`], which owns the writer, appends each `recv`-and-drain
/// batch in one write, then fires its acks. Exits when the channel closes or a
/// [`SessionCmd::Shutdown`] is absorbed.
pub(super) async fn run(
    mut state: SessionState,
    mut rx: mpsc::Receiver<SessionCmd>,
    shared: Arc<SharedState>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Batch::default();
        let mut flow = batch.absorb(first, &mut state, &shared);
        // Compact and archive are batch barriers. If either fails, its entries stay at the queue
        // tail for `settle_barriers` to drop. Records after a compact see its synthetic root.
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
        settle_barriers(&batch.acks, &mut state, failure.is_none());
        deliver_acks(
            batch.acks,
            failure.as_deref(),
            state.persistence_stopped(),
            &shared,
        );
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
            SessionCmd::Record { msg, context, ack } => {
                let (msg_entries, ai_title_seed) =
                    state.queue_message_entries(&msg, now, shared.manual_title_set());
                if let Some(context) = context {
                    self.entries.push(Entry::Context {
                        context: Box::new(context),
                        timestamp: now,
                    });
                }
                self.entries.extend(msg_entries);
                self.acks.push(PendingAck::Record { ack, ai_title_seed });
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
                // Re-check: a `/rename` can flip the latch after the generator already queued this.
                if shared.manual_title_set() {
                    _ = ack.send(Outcome { failure: None });
                    return BatchFlow::Continue;
                }
                self.entries.push(Entry::Title {
                    title,
                    source: TitleSource::AiGenerated,
                    updated_at: now,
                });
                self.acks.push(PendingAck::Outcome(ack));
                BatchFlow::Continue
            }
            SessionCmd::SetManualTitle { title, ack } => {
                shared.mark_manual_title_set();
                match state.try_defer_title(title) {
                    None => _ = ack.send(Outcome { failure: None }),
                    Some(title) => {
                        self.entries.push(Entry::Title {
                            title,
                            source: TitleSource::UserProvided,
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
                summary,
                instructions,
                synthetic_message,
                ack,
            } => {
                let pre_count = state.message_count();
                let (compact_entries, synthetic_uuid) =
                    state.compact_entries(&summary, instructions, synthetic_message, now);
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
            #[cfg(test)]
            SessionCmd::Panic => panic!("deliberate actor panic for testing"),
        }
    }

    /// Queues one sidecar per item, skipping default metadata since it adds no display fields. Acks at
    /// once when nothing needs a flush.
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

/// Commits a flushed compact, or abandons a failed compact or archive, matching what its ack tells
/// the caller.
fn settle_barriers(acks: &[PendingAck], state: &mut SessionState, flushed: bool) {
    for pending in acks {
        match pending {
            PendingAck::Compact { synthetic_uuid, .. } if flushed => {
                state.commit_compact(*synthetic_uuid);
            }
            PendingAck::Compact { .. } => state.abandon_compact(),
            PendingAck::Archive(_) if !flushed => state.abandon_archive(),
            _ => {}
        }
    }
}

fn deliver_acks(acks: Vec<PendingAck>, failure: Option<&str>, stopped: bool, shared: &SharedState) {
    for pending in acks {
        match pending {
            PendingAck::Record { ack, ai_title_seed } => {
                _ = ack.send(RecordOutcome {
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
                _ = ack.send(Outcome {
                    failure: failure.map(str::to_owned),
                });
            }
            PendingAck::Compact {
                ack,
                pre_count,
                synthetic_uuid: _,
            } => {
                // Every compact failure reaches the agent, which keeps its old transcript only when
                // told the write failed.
                _ = ack.send(CompactOutcome {
                    pre_count,
                    failure: failure.map(str::to_owned),
                });
            }
            PendingAck::Shutdown(ack) => {
                // Best-effort exit signal — no failure surfacing.
                _ = ack.send(());
            }
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
    use crate::session::state::MAX_UNWRITTEN_BYTES;
    use crate::session::store::SessionWriter;
    use crate::session::store::testing::{test_project_dir, test_session_file, test_store};

    /// Drives the actor against `cmds` until the receiver closes (we drop `tx` after sending),
    /// returning `SharedState` so callers can assert flush-failure flags.
    async fn drive(state: SessionState, cmds: Vec<SessionCmd>) -> Arc<SharedState> {
        let shared = Arc::new(SharedState::default());
        let (tx, rx) = mpsc::channel(cmds.len().max(1));
        for cmd in cmds {
            tx.send(cmd).await.unwrap();
        }
        drop(tx);
        let actor_shared = Arc::clone(&shared);
        run(state, rx, actor_shared).await;
        shared
    }

    fn record_cmd(text: &str) -> (SessionCmd, oneshot::Receiver<RecordOutcome>) {
        let (ack, rx) = oneshot::channel();
        (
            SessionCmd::Record {
                msg: Message::user(text),
                context: None,
                ack,
            },
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

    // ── run ──

    #[tokio::test]
    async fn run_record_then_finish_writes_header_title_message_summary_in_order() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store, "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("hello");
        let (fin, _fin_rx) = finish_cmd();

        drive(state, vec![rec, fin]).await;

        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            matches!(
                entries.as_slice(),
                [
                    Entry::Header { .. },
                    Entry::Title { .. },
                    Entry::Message { .. },
                    Entry::Summary {
                        message_count: 1,
                        ..
                    },
                ]
            ),
            "{content}",
        );
    }

    #[tokio::test]
    async fn run_first_record_seeds_ai_title_and_subsequent_does_not() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store, "m");
        let (cmd_a, rx_a) = record_cmd("Fix login bug");
        let (cmd_b, rx_b) = record_cmd("follow up");

        drive(state, vec![cmd_a, cmd_b]).await;

        let outcome_a = rx_a.await.unwrap();
        let outcome_b = rx_b.await.unwrap();
        assert_eq!(outcome_a.ai_title_seed.as_deref(), Some("Fix login bug"));
        assert!(outcome_b.ai_title_seed.is_none());
    }

    #[tokio::test]
    async fn run_drains_burst_into_single_batch() {
        // Three records queued at once must all reach disk in send order via one flush.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (a, _ra) = record_cmd("one");
        let (b, _rb) = record_cmd("two");
        let (c, _rc) = record_cmd("three");

        drive(state, vec![a, b, c]).await;

        let data = store.load_session_data(&session_id).unwrap();
        assert_eq!(data.messages.len(), 3);
    }

    #[tokio::test]
    async fn run_tool_metadata_with_default_short_circuits_without_writing() {
        // Default metadata has no display fields, so emitting an entry would be noise.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
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
        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        assert!(
            !content.contains(r#""type":"tool_result_metadata""#),
            "default metadata must not be written: {content}",
        );
    }

    #[tokio::test]
    async fn run_appends_ai_title_after_record() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("Fix login");
        let (ai_ack, _ai_rx) = oneshot::channel();
        let ai_cmd = SessionCmd::AppendAiTitle {
            title: "Fix the auth flow".to_owned(),
            ack: ai_ack,
        };
        let (fin, _fin_rx) = finish_cmd();

        drive(state, vec![rec, ai_cmd, fin]).await;

        // Tail scan picks the latest-updated_at title — AI wins over first-prompt.
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
    async fn run_set_manual_title_alone_leaves_no_file_on_disk() {
        // `/rename`-then-quit must leave no JSONL artifact. The deferred entry rides on
        // `WriterStatus::Pending` and dies with the actor when no record ever arrives.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
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
    async fn run_set_manual_title_then_record_replaces_first_prompt_title() {
        // `/rename` then send produces ONE title (UserProvided), not two: the deferred title
        // flushes ahead of the message, and `manual_title_set` suppresses the FirstPrompt push.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (manual_ack, _manual_rx) = oneshot::channel();
        let manual_cmd = SessionCmd::SetManualTitle {
            title: "User-named".to_owned(),
            ack: manual_ack,
        };
        let (rec, _rec_rx) = record_cmd("hello");
        let (fin, _fin_rx) = finish_cmd();

        drive(state, vec![manual_cmd, rec, fin]).await;

        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let header_idx = lines
            .iter()
            .position(|l| l.contains(r#""type":"header""#))
            .expect("header line");
        let title_idx = lines
            .iter()
            .position(|l| l.contains(r#""type":"title""#))
            .expect("title line");
        let message_idx = lines
            .iter()
            .position(|l| l.contains(r#""type":"message""#))
            .expect("message line");
        assert!(
            header_idx < title_idx && title_idx < message_idx,
            "deferred title must flush AFTER header and BEFORE message: {content}",
        );
        let title_count = lines
            .iter()
            .filter(|l| l.contains(r#""type":"title""#))
            .count();
        assert_eq!(
            title_count, 1,
            "deferred title replaces FirstPrompt: {content}"
        );
        assert!(
            lines[title_idx].contains("user_provided") && lines[title_idx].contains("User-named"),
            "the lone title is the user-provided one: {content}",
        );
    }

    #[tokio::test]
    async fn run_two_set_manual_titles_keep_only_the_last() {
        // Multiple `/rename` calls before any record must collapse to last-wins via the
        // deferred slot's overwrite semantics.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
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

        drive(state, vec![a, b, rec, fin]).await;

        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let title_lines: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(r#""type":"title""#))
            .collect();
        assert_eq!(
            title_lines.len(),
            1,
            "second rename overwrites the first: {content}"
        );
        assert!(
            title_lines[0].contains("Final name"),
            "last-wins semantic: {content}",
        );
        assert!(
            !content.contains("First name"),
            "earlier rename must not reach disk: {content}",
        );
    }

    #[tokio::test]
    async fn run_set_manual_title_after_record_blocks_ai_title() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("Fix login");
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

        drive(state, vec![rec, manual_cmd, ai_cmd, fin]).await;

        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let title_lines: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(r#""type":"title""#))
            .collect();
        assert_eq!(
            title_lines.len(),
            2,
            "FirstPrompt + UserProvided; AI must not append a third: {content}",
        );
        assert!(
            title_lines
                .iter()
                .any(|l| l.contains("user_provided") && l.contains("User-picked title")),
            "manual title must persist with `user_provided` source: {content}",
        );
        assert!(
            !content.contains("AI title that should lose"),
            "AI title must not appear on disk after manual override: {content}",
        );
    }

    #[tokio::test]
    async fn run_shutdown_exits_loop_even_with_live_sender_clones() {
        // Otherwise an orphaned title-generator's mid-HTTP clone would pin the actor through
        // the full HTTP timeout.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store, "m");
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
    async fn run_shutdown_flushes_preceding_record_in_same_batch() {
        // Pending writes ahead of Shutdown must reach disk; the actor only breaks after the
        // batch flush.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("flush before exit");
        let (shut, _shut_rx) = shutdown_cmd();

        drive(state, vec![rec, shut]).await;

        let data = store.load_session_data(&session_id).unwrap();
        assert_eq!(data.messages.len(), 1, "record must reach disk before exit");
    }

    #[tokio::test]
    async fn run_finish_idempotent_writes_one_summary() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("hi");
        let (f1, _r1) = finish_cmd();
        let (f2, _r2) = finish_cmd();

        drive(state, vec![rec, f1, f2]).await;

        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1, "second finish must not duplicate");
    }

    #[tokio::test]
    async fn run_compact_writes_boundary_and_synthetic_message_to_disk() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec_a, _) = record_cmd("user one");
        let (rec_b, _) = record_cmd("user two");
        let (compact_ack, _compact_rx) = oneshot::channel();
        let compact_cmd = SessionCmd::Compact {
            summary: "synth summary".to_owned(),
            instructions: Some("focus on auth".to_owned()),
            synthetic_message: Message::user("post"),
            ack: compact_ack,
        };

        drive(state, vec![rec_a, rec_b, compact_cmd]).await;

        let path = test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let compact_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"compact""#))
            .count();
        let message_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"message""#))
            .count();
        assert_eq!(compact_count, 1, "exactly one boundary written: {content}");
        assert_eq!(
            message_count, 3,
            "two recorded + one synthetic continuation: {content}",
        );
        assert!(
            content.contains(r#""summary":"synth summary""#),
            "boundary carries the summary text: {content}",
        );
        assert!(
            content.contains(r#""instructions":"focus on auth""#),
            "boundary carries the focus instructions: {content}",
        );
    }

    #[tokio::test]
    async fn run_compact_acks_with_pre_compact_message_count() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store, "m");
        let (rec_a, _) = record_cmd("one");
        let (rec_b, _) = record_cmd("two");
        let (compact_ack, compact_rx) = oneshot::channel();
        let compact_cmd = SessionCmd::Compact {
            summary: "s".to_owned(),
            instructions: None,
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
    async fn run_compact_flushes_before_following_record() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec_before, _) = record_cmd("before compact");
        let (compact_ack, _compact_rx) = oneshot::channel();
        let compact_cmd = SessionCmd::Compact {
            summary: "s".to_owned(),
            instructions: None,
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
    async fn run_failed_compact_is_dropped_while_earlier_records_retry() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
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
            summary: "s".to_owned(),
            instructions: None,
            synthetic_message: Message::user("synthetic summary"),
            ack: compact_ack,
        })
        .await
        .unwrap();
        assert!(
            compact_rx.await.unwrap().failure.is_some(),
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
    async fn run_failed_archive_is_dropped_while_later_records_land() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let original = SessionState::fresh(store.clone(), "m");
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
    async fn run_failed_archive_reports_the_failure_after_an_earlier_one_surfaced() {
        let dir = tempdir().unwrap();
        let state = SessionState::fresh(test_store(dir.path()), "m");
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
    async fn run_flush_error_records_failure_in_ack() {
        // Force store.create() to fail by removing its parent dir.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");

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
    async fn run_surfaces_the_persistence_stop_after_an_earlier_flush_failure() {
        let dir = tempdir().unwrap();
        let state = SessionState::fresh(test_store(dir.path()), "m");
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

    // ── surface_failure ──

    #[test]
    fn surface_failure_surfaces_the_first_flush_failure_and_the_first_stop_then_silences() {
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
    fn surface_failure_no_failure_is_none() {
        let shared = SharedState::default();
        assert!(surface_failure(None, false, &shared).is_none());
    }
}
