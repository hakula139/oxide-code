//! Session actor — owns [`SessionState`] + the writer, drains
//! [`SessionCmd`]s, and coalesces a turn's worth of cmds into one flush.
//!
//! The receive-and-drain loop (`recv().await` then `try_recv()` until
//! empty) gives batching for free: bursts queued during one agent
//! iteration commit together; isolated writes still flush immediately.
//! No interval timer — see `docs/research/session-persistence.md`.

use std::sync::Arc;

use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use super::entry::Entry;
use super::handle::{Outcome, RecordOutcome, SharedState};
use super::state::SessionState;
use crate::message::Message;
use crate::tool::ToolMetadata;

/// Cross-task protocol for [`super::handle::SessionHandle`]. Each ack
/// fires after the batch flush, so a returned ack implies the entry is
/// in at least the OS write cache.
pub(super) enum SessionCmd {
    Record {
        msg: Message,
        ack: oneshot::Sender<RecordOutcome>,
    },
    /// One or more tool-result sidecars from a single agent turn. The
    /// batch shape lets the agent loop emit all sidecars for a tool
    /// round in one cmd → one ack → one flush, instead of N awaits in
    /// a row. Items whose `metadata == ToolMetadata::default()` add no
    /// display fields and are skipped at absorb time.
    ToolMetadata {
        items: Vec<(String, ToolMetadata)>,
        ack: oneshot::Sender<Outcome>,
    },
    AppendAiTitle {
        title: String,
        ack: oneshot::Sender<Outcome>,
    },
    Finish {
        ack: oneshot::Sender<Outcome>,
    },
}

/// One absorbed cmd whose ack fires once the batch flush returns. Held
/// per-batch so the same flush result reaches every caller in the
/// batch.
enum PendingAck {
    Record {
        ack: oneshot::Sender<RecordOutcome>,
        ai_title_seed: Option<String>,
    },
    Outcome(oneshot::Sender<Outcome>),
}

pub(super) async fn run(
    mut state: SessionState,
    mut rx: mpsc::Receiver<SessionCmd>,
    shared: Arc<SharedState>,
) {
    while let Some(first) = rx.recv().await {
        let mut entries: Vec<Entry> = Vec::new();
        let mut acks: Vec<PendingAck> = Vec::new();
        absorb(first, &mut entries, &mut acks, &mut state);
        // Drain whatever is already queued — cmds sent after this point
        // wait for the next outer `recv().await`.
        while let Ok(next) = rx.try_recv() {
            absorb(next, &mut entries, &mut acks, &mut state);
        }
        let result = state.flush_entries(&entries);
        let failure = match &result {
            Err(e) => {
                let msg = format!("{e:#}");
                warn!("session write batch failed: {msg}");
                shared.record_flush_failure(&msg);
                Some(msg)
            }
            Ok(()) => None,
        };
        deliver_acks(acks, failure.as_deref(), &shared);
    }
}

fn absorb(
    cmd: SessionCmd,
    entries: &mut Vec<Entry>,
    acks: &mut Vec<PendingAck>,
    state: &mut SessionState,
) {
    let now = OffsetDateTime::now_utc();
    match cmd {
        SessionCmd::Record { msg, ack } => {
            let (msg_entries, ai_title_seed) = state.queue_message_entries(&msg, now);
            entries.extend(msg_entries);
            acks.push(PendingAck::Record { ack, ai_title_seed });
        }
        SessionCmd::ToolMetadata { items, ack } => {
            // Default metadata adds no display fields; emitting it would
            // bloat the transcript with empty sidecar lines.
            let mut wrote_any = false;
            for (tool_use_id, metadata) in items {
                if metadata == ToolMetadata::default() {
                    continue;
                }
                entries.push(Entry::ToolResultMetadata {
                    tool_use_id,
                    metadata,
                    timestamp: now,
                });
                wrote_any = true;
            }
            if wrote_any {
                acks.push(PendingAck::Outcome(ack));
            } else {
                // Empty / all-default batch — nothing to flush, no
                // batch result to await.
                _ = ack.send(Outcome { failure: None });
            }
        }
        SessionCmd::AppendAiTitle { title, ack } => {
            entries.push(Entry::Title {
                title,
                source: super::entry::TitleSource::AiGenerated,
                updated_at: now,
            });
            acks.push(PendingAck::Outcome(ack));
        }
        SessionCmd::Finish { ack } => {
            if let Some(entry) = state.finish_entry(now) {
                entries.push(entry);
            }
            acks.push(PendingAck::Outcome(ack));
        }
    }
}

fn deliver_acks(acks: Vec<PendingAck>, failure: Option<&str>, shared: &SharedState) {
    for pending in acks {
        match pending {
            PendingAck::Record { ack, ai_title_seed } => {
                _ = ack.send(RecordOutcome {
                    ai_title_seed,
                    failure: surface_failure(failure, shared),
                });
            }
            PendingAck::Outcome(ack) => {
                _ = ack.send(Outcome {
                    failure: surface_failure(failure, shared),
                });
            }
        }
    }
}

/// At-most-once flush-failure surfacing: the first failure carries
/// through to the caller, subsequent ones go to the warn-log only so a
/// disk-full mid-conversation doesn't drown the user. Independent of
/// the actor-gone surface flag — see [`SharedState`].
fn surface_failure(failure: Option<&str>, shared: &SharedState) -> Option<String> {
    let msg = failure?;
    if shared.surface_first_flush_failure() {
        Some(msg.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::super::handle::SharedState;
    use super::super::store::test_store;
    use super::*;
    use crate::message::Message;
    use crate::session::state::SessionState;

    /// Run the actor against an in-memory state until the receiver
    /// closes (caller drops `tx`), then return the final state for
    /// assertions on file contents.
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
                ack,
            },
            rx,
        )
    }

    fn finish_cmd() -> (SessionCmd, oneshot::Receiver<Outcome>) {
        let (ack, rx) = oneshot::channel();
        (SessionCmd::Finish { ack }, rx)
    }

    // ── run ──

    #[tokio::test]
    async fn run_flush_error_records_failure_in_ack() {
        // Force store.create() to fail by removing its parent dir.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");

        let project_dir = super::super::store::test_project_dir(dir.path());
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
    async fn run_record_then_finish_writes_header_message_summary_in_order() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("hello");
        let (fin, _fin_rx) = finish_cmd();

        drive(state, vec![rec, fin]).await;

        let data = store.load_session_data(&session_id).unwrap();
        assert_eq!(data.messages.len(), 1, "user message recorded");
        assert!(
            data.title.is_some(),
            "first-prompt title written before message",
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
        // Three records queued at once must all land in the file in
        // send order — single-batch coalescing is implicit.
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
        // Default metadata has no display fields, so emitting an entry
        // would be noise.
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
        let path = super::super::store::test_session_file(dir.path(), &session_id);
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

        // Tail scan picks the latest-updated_at title, so the AI
        // title wins over the first-prompt title.
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
    async fn run_finish_idempotent_writes_one_summary() {
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("hi");
        let (f1, _r1) = finish_cmd();
        let (f2, _r2) = finish_cmd();

        drive(state, vec![rec, f1, f2]).await;

        let path = super::super::store::test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        let summary_count = content
            .lines()
            .filter(|l| l.contains(r#""type":"summary""#))
            .count();
        assert_eq!(summary_count, 1, "second finish must not duplicate");
    }

    #[tokio::test]
    async fn run_full_turn_produces_byte_compatible_jsonl() {
        // Pins literal JSONL bytes (with UUIDs / timestamps / cwd
        // masked for stability) so a stray field rename would fail.
        let dir = tempdir().unwrap();
        let store = test_store(dir.path());
        let state = SessionState::fresh(store.clone(), "m");
        let session_id = state.session_id.to_string();
        let (rec, _rec_rx) = record_cmd("Edit something");
        let (meta_ack, _meta_rx) = oneshot::channel();
        let meta_cmd = SessionCmd::ToolMetadata {
            items: vec![(
                "t1".to_owned(),
                ToolMetadata {
                    title: Some("Edited f.rs".to_owned()),
                    replacements: Some(2),
                    ..ToolMetadata::default()
                },
            )],
            ack: meta_ack,
        };
        let (fin, _fin_rx) = finish_cmd();

        drive(state, vec![rec, meta_cmd, fin]).await;

        let path = super::super::store::test_session_file(dir.path(), &session_id);
        let content = std::fs::read_to_string(path).unwrap();
        insta::assert_snapshot!(mask_volatile(&content));
    }

    // ── surface_failure ──

    #[test]
    fn surface_failure_first_call_after_record_returns_message_then_silences() {
        let shared = SharedState::default();
        shared.record_flush_failure("boom");
        let first = surface_failure(Some("boom"), &shared);
        let second = surface_failure(Some("boom"), &shared);
        assert_eq!(first.as_deref(), Some("boom"));
        assert!(second.is_none(), "subsequent failures stay silent");
    }

    #[test]
    fn surface_failure_no_failure_returns_none() {
        let shared = SharedState::default();
        assert!(surface_failure(None, &shared).is_none());
    }

    // ── Helpers ──

    /// Replaces UUIDs, timestamps, and the cwd value with placeholders
    /// so the snapshot stays stable across runs.
    fn mask_volatile(content: &str) -> String {
        let uuid_re =
            regex::Regex::new(r#""[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}""#)
                .unwrap();
        let ts_re =
            regex::Regex::new(r#""\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z""#).unwrap();
        let cwd_re = regex::Regex::new(r#""cwd":"[^"]*""#).unwrap();
        let masked = uuid_re.replace_all(content, r#""<UUID>""#);
        let masked = ts_re.replace_all(&masked, r#""<TS>""#);
        cwd_re.replace_all(&masked, r#""cwd":"<CWD>""#).into_owned()
    }
}
