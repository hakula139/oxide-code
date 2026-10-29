//! Test-only [`SessionHandle`] fixtures.

use std::path::Path;

use super::{CompactBoundary, SessionHandle};
use crate::file_tracker::FileTracker;
use crate::file_tracker::testing::record_tracked_file;
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SessionContext;
use crate::session::store::testing::TEST_PROJECT;
use crate::session::store::{CompactRecord, SessionStore};

/// A `/compact` boundary for `summary` with no focus or task list.
pub(crate) fn boundary(summary: &str) -> CompactBoundary {
    CompactBoundary {
        record: CompactRecord {
            summary: summary.to_owned(),
            instructions: None,
            tasks: Vec::new(),
        },
        context: SessionContext::default(),
    }
}

/// Records a Write call of `tool_use_id` on `handle` that leaves `alpha` at `path`, as `tracker`
/// records it. An `answered` call also gets its tool result.
pub(crate) async fn record_write_call(
    handle: &SessionHandle,
    tracker: &FileTracker,
    tool_use_id: &str,
    path: &Path,
    answered: bool,
) {
    handle
        .record_message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                tool_use_id,
                "write",
                serde_json::Value::Null,
            )],
        })
        .await;
    record_tracked_file(tracker, path, b"alpha", tool_use_id);
    if answered {
        handle
            .record_message(Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result(tool_use_id, "wrote", false)],
            })
            .await;
    }
}

/// A fresh session on a store that was never set up, so its first write that saves a message
/// fails.
pub(crate) fn unsaved() -> SessionHandle {
    super::start(&SessionStore::unopened(TEST_PROJECT), "m", None)
}
