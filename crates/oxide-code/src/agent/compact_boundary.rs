//! Compact-boundary persistence and live transcript replacement.

use crate::agent::compaction::synthesize_post_compact_message;
use crate::agent::event::{AgentEvent, AgentSink};
use crate::file_tracker::FileTracker;
use crate::message::Message;
use crate::session::handle::SessionHandle;

/// Persists a compact boundary and swaps the live transcript to the synthetic summary root.
pub(crate) async fn replace_session_with_summary(
    session: &SessionHandle,
    file_tracker: &FileTracker,
    messages: &mut Vec<Message>,
    sink: &dyn AgentSink,
    summary: String,
    instructions: Option<String>,
    automatic: bool,
) -> bool {
    let synthetic = synthesize_post_compact_message(&summary);
    let outcome = session
        .compact(summary.clone(), instructions.clone(), synthetic.clone())
        .await;
    sink.session_write_error(outcome.failure.as_deref());
    if outcome.failure.is_some() {
        return false;
    }

    file_tracker.clear();
    *messages = vec![synthetic];
    if let Err(e) = sink.send(AgentEvent::SessionCompacted {
        summary,
        pre_count: outcome.pre_count,
        instructions,
        automatic,
    }) {
        tracing::error!("session-compacted event dropped: {e}");
    }
    true
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use anyhow::anyhow;

    use super::*;
    use crate::agent::event::{AgentSink, CapturingSink};
    use crate::file_tracker::LastView;
    use crate::message::{ContentBlock, Message};
    use crate::session::handle;
    use crate::session::store::test_store;

    struct FailingSink;

    impl AgentSink for FailingSink {
        fn send(&self, _event: AgentEvent) -> anyhow::Result<()> {
            Err(anyhow!("sink closed"))
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
    async fn replace_session_with_summary_clears_tracker_and_replaces_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let session = handle::start(&store, "claude-sonnet-4-6");
        let tracker = FileTracker::default();
        tracker.record_read(
            std::path::Path::new("/tmp/example.rs"),
            b"old",
            SystemTime::UNIX_EPOCH,
            3,
            LastView::Full,
        );
        let mut messages = fake_transcript();
        let sink = CapturingSink::new();

        let compacted = replace_session_with_summary(
            &session,
            &tracker,
            &mut messages,
            &sink,
            "fixed login bug".to_owned(),
            None,
            true,
        )
        .await;

        assert!(compacted);
        assert!(tracker.snapshot_all().is_empty());
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("fixed login bug"))
        );
    }

    #[tokio::test]
    async fn replace_session_with_summary_preserves_state_when_persist_fails() {
        let session = handle::testing::dead("dead-compact-session");
        let tracker = FileTracker::default();
        let path = std::path::PathBuf::from("/tmp/example.rs");
        tracker.record_read(&path, b"old", SystemTime::UNIX_EPOCH, 3, LastView::Full);
        let original = fake_transcript();
        let mut messages = original.clone();
        let sink = CapturingSink::new();

        let compacted = replace_session_with_summary(
            &session,
            &tracker,
            &mut messages,
            &sink,
            "fixed login bug".to_owned(),
            None,
            true,
        )
        .await;

        assert!(!compacted);
        assert_eq!(messages.len(), original.len());
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "fix the bug")
        );
        assert!(
            matches!(&messages[3].content[0], ContentBlock::Text { text } if text == "found it")
        );
        assert_eq!(tracker.snapshot_all().len(), 1);
        assert!(
            sink.events()
                .iter()
                .any(|event| matches!(event, AgentEvent::Error(message) if message.contains("Session write failed")))
        );
    }

    #[tokio::test]
    async fn replace_session_with_summary_still_replaces_messages_when_event_send_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let session = handle::start(&store, "claude-sonnet-4-6");
        let tracker = FileTracker::default();
        tracker.record_read(
            std::path::Path::new("/tmp/example.rs"),
            b"old",
            SystemTime::UNIX_EPOCH,
            3,
            LastView::Full,
        );
        let mut messages = fake_transcript();

        let compacted = replace_session_with_summary(
            &session,
            &tracker,
            &mut messages,
            &FailingSink,
            "fixed login bug".to_owned(),
            None,
            true,
        )
        .await;

        assert!(compacted);
        assert!(tracker.snapshot_all().is_empty());
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("fixed login bug"))
        );
    }
}
