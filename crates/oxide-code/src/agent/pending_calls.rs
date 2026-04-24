//! Pending tool-call bookkeeping shared between live streaming and
//! transcript resume.
//!
//! Both the live event path
//! ([`crate::tui::app::App::handle_agent_event`]) and the resumed-history
//! walk ([`crate::tui::components::chat::ChatView::load_history`]) need
//! to bridge a tool-call observation to its later matching result so
//! they can render the result with:
//!
//! - The call's label as the status-line fallback when the tool
//!   emits `title: None`.
//! - The call's `name` + `input` to drive
//!   [`ToolRegistry::result_view`](crate::tool::ToolRegistry::result_view)
//!   for the structured result shape (Edit diff, future Read/Grep
//!   excerpts, etc.).
//!
//! This module factors that bookkeeping into one shared type so both
//! call sites evolve together — adding a new field to [`PendingCall`]
//! (say, a tool-side timestamp for latency display) lands for both
//! resumed and live sessions at once.

use std::collections::HashMap;

use crate::tool::ToolMetadata;

/// Generic fallback label used when a tool result arrives with
/// `title: None` AND no matching pending entry exists (orphan result
/// from a replayed transcript with missing start, or an agent-loop
/// bug). Both the live event path and the resumed-history walk must
/// render *something* so the user sees the output — this is the
/// something.
///
/// Not used when a pending entry exists: in that case the call's
/// computed label is the fallback, which carries more information
/// (tool name + input) than this sentinel.
pub(crate) const FALLBACK_RESULT_HEADER: &str = "(result)";

/// Resolves the status-line header for a completed tool result.
///
/// Tool-provided metadata wins because it is the most specific result
/// summary (`Edited f.rs`). The pending call label is next because it
/// still identifies the tool and input (`Edit(/tmp/f.rs)`). The generic
/// fallback is only for orphaned results whose start event is missing.
pub(crate) fn result_header(metadata: &ToolMetadata, pending_label: Option<&str>) -> String {
    metadata
        .title
        .clone()
        .or_else(|| pending_label.map(str::to_owned))
        .unwrap_or_else(|| FALLBACK_RESULT_HEADER.to_owned())
}

/// Per-call metadata observed at tool-call emission, consumed at the
/// matching result.
#[derive(Debug, Clone)]
pub(crate) struct PendingCall {
    /// TUI-display label — computed via
    /// [`ToolRegistry::label`](crate::tool::ToolRegistry::label).
    pub(crate) label: String,
    /// Registered tool name — the same string sent to the model.
    pub(crate) name: String,
    /// Original tool-call arguments. Retained (not re-parsed from the
    /// transcript at result time) so structured result views have
    /// direct access to `old_string` / `new_string` / etc. without a
    /// second walk.
    pub(crate) input: serde_json::Value,
}

/// Map from `tool_use_id` to its pending metadata. Thin wrapper around
/// [`HashMap`] so the two call sites share an API: `insert` at the
/// start of a call, `remove` to consume at the end.
#[derive(Debug, Default)]
pub(crate) struct PendingCalls {
    map: HashMap<String, PendingCall>,
}

impl PendingCalls {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records `call` under `id`. Overwrites any existing entry for
    /// the same id — the agent loop guarantees unique tool-use ids
    /// per session, but live restreams can re-emit an id if an earlier
    /// attempt was aborted.
    pub(crate) fn insert(&mut self, id: String, call: PendingCall) {
        self.map.insert(id, call);
    }

    /// Consumes and returns the pending entry for `id`. Returns
    /// `None` for orphan results (result arrived without a matching
    /// call — a transcript-repair artifact).
    pub(crate) fn remove(&mut self, id: &str) -> Option<PendingCall> {
        self.map.remove(id)
    }

    /// Discards every pending entry. Called at turn boundaries so a
    /// call whose result never arrived (agent-loop bug, crashed tool
    /// subprocess, mid-turn abort) doesn't linger in the map across
    /// turns — otherwise a long session accumulates orphaned entries
    /// indefinitely.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }

    /// Number of outstanding calls. Test-only observable so turn-end
    /// eviction can be asserted without reaching into the private map.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_call() -> PendingCall {
        PendingCall {
            label: "Edit(/tmp/f.rs)".to_owned(),
            name: "edit".to_owned(),
            input: serde_json::json!({"file_path": "/tmp/f.rs"}),
        }
    }

    // ── result_header ──

    #[test]
    fn result_header_prefers_metadata_title() {
        let metadata = ToolMetadata {
            title: Some("Edited f.rs".to_owned()),
            ..ToolMetadata::default()
        };

        let header = result_header(&metadata, Some("Edit(/tmp/f.rs)"));

        assert_eq!(header, "Edited f.rs");
    }

    #[test]
    fn result_header_falls_back_to_pending_label() {
        let header = result_header(&ToolMetadata::default(), Some("Edit(/tmp/f.rs)"));

        assert_eq!(header, "Edit(/tmp/f.rs)");
    }

    #[test]
    fn result_header_falls_back_to_generic_header_for_orphans() {
        let header = result_header(&ToolMetadata::default(), None);

        assert_eq!(header, FALLBACK_RESULT_HEADER);
    }

    // ── PendingCalls ──

    #[test]
    fn insert_and_remove_round_trip() {
        let mut calls = PendingCalls::new();
        calls.insert("t1".to_owned(), sample_call());
        let got = calls.remove("t1").expect("entry must be returned");
        assert_eq!(got.label, "Edit(/tmp/f.rs)");
        assert_eq!(got.name, "edit");
    }

    #[test]
    fn remove_drains_the_entry() {
        // Second `remove` must see `None` — otherwise a re-emitted
        // `ToolCallEnd` for the same id would render twice.
        let mut calls = PendingCalls::new();
        calls.insert("t1".to_owned(), sample_call());
        assert!(calls.remove("t1").is_some());
        assert!(calls.remove("t1").is_none());
    }

    #[test]
    fn remove_unknown_id_is_absent() {
        let mut calls = PendingCalls::new();
        assert!(calls.remove("orphan").is_none());
    }

    #[test]
    fn insert_overwrites_existing_entry() {
        // Aborted-and-retried tool runs land on the same id; the
        // later metadata must win so the rendered label matches the
        // call that actually completed.
        let mut calls = PendingCalls::new();
        calls.insert("t1".to_owned(), sample_call());
        let retry = PendingCall {
            label: "Edit(retry)".to_owned(),
            name: "edit".to_owned(),
            input: serde_json::json!({}),
        };
        calls.insert("t1".to_owned(), retry);
        let got = calls.remove("t1").unwrap();
        assert_eq!(got.label, "Edit(retry)");
    }
}
