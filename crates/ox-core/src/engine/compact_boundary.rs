//! Compact-boundary persistence and live transcript replacement.

use crate::file_tracker::FileTracker;
use crate::message::Message;
use crate::protocol::{Event, EventSink};
use crate::session::handle::SessionHandle;
use crate::session::post_compact::synthesize_post_compact_message;
use crate::tool::bash::background::BackgroundShell;
use crate::tool::tasks::Task;

/// A finished summary and what the boundary records beside it.
pub(super) struct Boundary {
    pub(super) summary: String,
    /// The `/compact` focus the summary followed.
    pub(super) instructions: Option<String>,
    /// The task list the summarized transcript left, which the summary message carries over.
    pub(super) tasks: Vec<Task>,
    /// The open background shells, which the summary message lists.
    pub(super) shells: Vec<BackgroundShell>,
    /// Whether the compaction ran on its own, ahead of a prompt.
    pub(super) automatic: bool,
}

/// Persists a compact boundary and swaps the live transcript to the synthetic summary root. A
/// failed write discards the streamed summary and leaves the transcript unchanged, unless
/// persistence has stopped, when the swap happens in memory alone.
pub(super) async fn replace_session_with_summary(
    session: &SessionHandle,
    file_tracker: &FileTracker,
    messages: &mut Vec<Message>,
    sink: &dyn EventSink,
    boundary: Boundary,
) -> bool {
    let Boundary {
        summary,
        instructions,
        tasks,
        shells,
        automatic,
    } = boundary;
    let synthetic = synthesize_post_compact_message(&summary, &tasks, &shells);
    let outcome = session
        .compact(
            summary.clone(),
            instructions.clone(),
            tasks,
            synthetic.clone(),
        )
        .await;
    if !outcome.committed {
        sink.emit(Event::SummaryDiscarded, "summary-discarded");
        sink.session_write_error(outcome.failure.as_deref());
        return false;
    }

    file_tracker.clear();
    *messages = vec![synthetic];
    sink.emit(
        Event::SessionCompacted {
            summary,
            pre_count: outcome.pre_count,
            instructions,
            automatic,
        },
        "session-compacted",
    );
    // Reports the write error after the compaction event, which clears the TUI's chat.
    sink.session_write_error(outcome.failure.as_deref());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_tracker::LastView;
    use crate::message::{ContentBlock, Message};
    use crate::protocol::notice::session_write_failed;
    use crate::protocol::testing::{CapturingSink, FailingSink};
    use crate::session::handle;
    use crate::session::store::testing::{test_project_dir, test_store};
    use crate::tool::tasks::testing::plan;

    mod replace_session_with_summary {
        use super::*;

        fn boundary(tasks: Vec<Task>) -> Boundary {
            Boundary {
                summary: "fixed login bug".to_owned(),
                instructions: None,
                tasks,
                shells: Vec::new(),
                automatic: true,
            }
        }

        fn fake_transcript() -> Vec<Message> {
            vec![
                Message::user("fix the bug"),
                Message::assistant("looking now"),
                Message::user("any progress?"),
                Message::assistant("found it"),
            ]
        }

        #[tokio::test]
        async fn clears_tracker_and_carries_the_task_list_over() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let session = handle::start(&store, "claude-sonnet-5-5", None);
            let session_id = session.session_id().to_owned();
            let tracker = FileTracker::default();
            tracker.record_read(
                std::path::Path::new("/tmp/example.rs"),
                b"old",
                LastView::Full,
                "toolu_test",
            );
            let mut messages = fake_transcript();
            let sink = CapturingSink::default();

            let compacted = replace_session_with_summary(
                &session,
                &tracker,
                &mut messages,
                &sink,
                boundary(plan()),
            )
            .await;

            assert!(compacted);
            assert_eq!(tracker.snapshot_all(), []);
            assert_eq!(
                messages,
                [synthesize_post_compact_message(
                    "fixed login bug",
                    &plan(),
                    &[]
                )]
            );
            session.shutdown().await;
            let saved = store.load_session_data(&session_id).unwrap();
            assert_eq!(saved.compact.map(|compact| compact.tasks), Some(plan()));
            assert_eq!(saved.messages, messages);
        }

        #[tokio::test]
        async fn preserves_state_when_persist_fails() {
            let session = handle::testing::dead("dead-compact-session");
            let tracker = FileTracker::default();
            let path = std::path::PathBuf::from("/tmp/example.rs");
            tracker.record_read(&path, b"old", LastView::Full, "toolu_test");
            let original = fake_transcript();
            let mut messages = original.clone();
            let sink = CapturingSink::default();

            let compacted = replace_session_with_summary(
                &session,
                &tracker,
                &mut messages,
                &sink,
                boundary(Vec::new()),
            )
            .await;

            assert!(!compacted);
            assert_eq!(messages.len(), original.len());
            assert!(matches!(
                &messages[0].content[0],
                ContentBlock::Text { text } if text == "fix the bug"
            ));
            assert!(
                matches!(&messages[3].content[0], ContentBlock::Text { text } if text == "found it")
            );
            assert_eq!(tracker.snapshot_all().len(), 1);
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::SummaryDiscarded, Event::Error(message)]
                        if message.contains("Session write failed")
                ),
                "the unsaved summary is discarded before the write error: {events:?}",
            );
        }

        #[tokio::test]
        async fn swaps_in_memory_once_its_write_stops_persistence() {
            let dir = tempfile::tempdir().unwrap();
            let session = handle::start(&test_store(dir.path()), "claude-sonnet-5-5", None);
            std::fs::remove_dir_all(test_project_dir(dir.path())).unwrap();
            let unsaved = session
                .record_message(Message::user("x".repeat(5 << 20)))
                .await;
            assert!(unsaved.failure.is_some());
            let mut messages = fake_transcript();
            let sink = CapturingSink::default();
            let summary = "s".repeat(2 << 20);
            let mut large = boundary(Vec::new());
            large.summary.clone_from(&summary);

            let compacted = replace_session_with_summary(
                &session,
                &FileTracker::default(),
                &mut messages,
                &sink,
                large,
            )
            .await;

            assert!(compacted);
            assert_eq!(
                messages,
                [synthesize_post_compact_message(&summary, &[], &[])]
            );
            let stop = session_write_failed(
                "session persistence stopped after more than 8 MiB of entries failed to write: \
                 failed to create ",
            );
            let events = sink.events();
            assert!(
                matches!(
                    events.as_slice(),
                    [Event::SessionCompacted { .. }, Event::Error(message)]
                        if message.starts_with(&stop)
                ),
                "expected the compaction, then the stop: {events:?}",
            );
            session.shutdown().await;
        }

        #[tokio::test]
        async fn still_replaces_messages_when_event_send_fails() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let session = handle::start(&store, "claude-sonnet-5-5", None);
            let tracker = FileTracker::default();
            tracker.record_read(
                std::path::Path::new("/tmp/example.rs"),
                b"old",
                LastView::Full,
                "toolu_test",
            );
            let mut messages = fake_transcript();

            let compacted = replace_session_with_summary(
                &session,
                &tracker,
                &mut messages,
                &FailingSink,
                boundary(Vec::new()),
            )
            .await;

            assert!(compacted);
            assert_eq!(tracker.snapshot_all(), []);
            assert_eq!(messages.len(), 1);
            let first = &messages[0].content[0];
            assert!(
                matches!(first, ContentBlock::Text { text } if text.contains("fixed login bug")),
                "{first:?}"
            );
        }
    }
}
