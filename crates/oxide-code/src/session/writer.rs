//! Thin I/O helpers on top of [`SessionManager`].
//!
//! Centralizes the "take the session lock, call `record_message`, route
//! any failure through [`log_session_err`]" boilerplate so the TUI, bare
//! REPL, and headless modes share one recording path.

use tokio::sync::Mutex;
use tracing::warn;

use crate::agent::event::{AgentEvent, AgentSink};
use crate::message::Message;
use crate::session::manager::SessionManager;

/// Record one message to the session, surfacing any write failure via
/// `sink`. Holds the session lock only for the duration of the write
/// so other tasks (and concurrent writes from the same task) see
/// fresh access instead of blocking behind a long-running agent turn.
pub(crate) async fn record_session_message(
    session: &Mutex<SessionManager>,
    msg: &Message,
    sink: Option<&dyn AgentSink>,
) {
    let mut s = session.lock().await;
    let r = s.record_message(msg).await;
    log_session_err(r, &mut s, sink);
}

/// Logs session I/O errors without aborting the agent loop.
///
/// The first failure within a session is also surfaced to the user via
/// `sink` (when available) so they know the conversation may not be
/// saved. Subsequent failures warn-log only to avoid spamming the UI
/// — the persistence problem has already been announced.
pub(crate) fn log_session_err(
    result: anyhow::Result<()>,
    session: &mut SessionManager,
    sink: Option<&dyn AgentSink>,
) {
    let Err(e) = result else {
        return;
    };
    warn!("session write failed: {e}");
    if session.record_write_failure()
        && let Some(sink) = sink
    {
        // `{e:#}` flattens the anyhow cause chain — the outer context
        // is usually "failed to write session file" while the
        // actionable root (permission denied, disk full, ...) lives
        // beneath. Plain `Display` would hide it.
        _ = sink.send(AgentEvent::Error(format!(
            "Session write failed: {e:#}. Conversation history may be incomplete; further write errors will be silent."
        )));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use anyhow::{Result, anyhow};

    use super::super::store::{test_session_file, test_store};
    use super::*;

    /// Recording sink: captures every event the helper emits so tests
    /// can assert both "sent exactly this" and "sent nothing".
    struct CapturingSink {
        events: StdMutex<Vec<AgentEvent>>,
    }

    impl CapturingSink {
        fn new() -> Self {
            Self {
                events: StdMutex::new(Vec::new()),
            }
        }

        fn errors(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::Error(msg) => Some(msg.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    impl AgentSink for CapturingSink {
        fn send(&self, event: AgentEvent) -> Result<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    // ── record_session_message ──

    #[tokio::test]
    async fn record_session_message_writes_through_to_manager() {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let manager = SessionManager::start(&store, "m");
        let sid = manager.session_id().to_owned();
        let session = Mutex::new(manager);

        record_session_message(&session, &Message::user("hello"), None).await;

        let content = std::fs::read_to_string(test_session_file(dir.path(), &sid)).unwrap();
        assert!(content.contains(r#""type":"message""#));
        assert!(content.contains("hello"));
    }

    // ── log_session_err ──

    #[tokio::test]
    async fn log_session_err_is_noop_on_ok() {
        // Ok short-circuits before `record_write_failure`, so the sticky
        // flag must stay clear; the next genuine failure should still
        // get surfaced to the sink.
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");
        let sink = CapturingSink::new();
        log_session_err(Ok(()), &mut manager, Some(&sink));
        assert!(sink.errors().is_empty());

        log_session_err(Err(anyhow!("real failure")), &mut manager, Some(&sink));
        assert_eq!(sink.errors().len(), 1);
    }

    #[tokio::test]
    async fn log_session_err_err_without_sink_only_warns() {
        // No sink means the caller already teared down the UI (finish()
        // after TUI restore). The failure flag still flips so a later
        // call with a sink won't duplicate the notification.
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");

        log_session_err(Err(anyhow!("write blew up")), &mut manager, None);

        // Flag is set — a subsequent failure on the same manager should
        // no longer reach a sink.
        let sink = CapturingSink::new();
        log_session_err(Err(anyhow!("second failure")), &mut manager, Some(&sink));
        assert!(
            sink.errors().is_empty(),
            "subsequent failure must not re-notify",
        );
    }

    #[tokio::test]
    async fn log_session_err_first_failure_notifies_via_sink() {
        // Feed a two-level cause chain so the `{e:#}` format actually
        // has something to flatten — regression for the bug where
        // `Display` alone would hide the actionable root cause
        // (permission denied, disk full, ...) under a generic outer
        // context ("failed to write session file").
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");
        let sink = CapturingSink::new();

        let err = anyhow!("permission denied").context("failed to write session file");
        log_session_err(Err(err), &mut manager, Some(&sink));

        let errors = sink.errors();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("failed to write session file"),
            "outer context missing: {errors:?}"
        );
        assert!(
            errors[0].contains("permission denied"),
            "root cause should be flattened into the message: {errors:?}"
        );
        assert!(
            errors[0].contains("silent"),
            "message should warn that future errors will be quiet: {errors:?}"
        );
    }

    #[tokio::test]
    async fn log_session_err_subsequent_failure_stays_silent() {
        // Two failures on the same manager, both with a sink: the sink
        // sees exactly one Error event. Sticky-suppression is what lets
        // mid-conversation disk-full errors not drown the user.
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::start(&test_store(dir.path()), "m");
        let sink = CapturingSink::new();

        log_session_err(Err(anyhow!("first")), &mut manager, Some(&sink));
        log_session_err(Err(anyhow!("second")), &mut manager, Some(&sink));
        log_session_err(Err(anyhow!("third")), &mut manager, Some(&sink));

        assert_eq!(sink.errors().len(), 1);
    }
}
