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

/// Log session I/O errors without aborting the agent loop.
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
        _ = sink.send(AgentEvent::Error(format!(
            "Session write failed: {e}. Conversation history may be incomplete; further write errors will be silent."
        )));
    }
}
