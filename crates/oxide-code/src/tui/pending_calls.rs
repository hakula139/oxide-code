//! Pending tool-call bookkeeping shared between live streaming and
//! transcript resume.
//!
//! Both the live event path ([`super::app::App::handle_agent_event`])
//! and the resumed-history walk
//! ([`super::components::chat::ChatView::load_history`]) need to
//! bridge a tool-call observation to its later matching result so they
//! can render the result with:
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
    fn remove_unknown_id_returns_none() {
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
