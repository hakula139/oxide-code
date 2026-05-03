//! Tool-call correlation state shared by live streaming and transcript
//! resume, so both paths evolve together.

use std::collections::HashMap;

use crate::tool::ToolMetadata;

/// Sentinel for orphan results whose start event is missing.
pub(crate) const FALLBACK_RESULT_HEADER: &str = "(result)";

/// Resolves the header: metadata title > pending-call label > fallback.
pub(crate) fn result_header(metadata: &ToolMetadata, pending_label: Option<&str>) -> String {
    metadata
        .title
        .clone()
        .or_else(|| pending_label.map(str::to_owned))
        .unwrap_or_else(|| FALLBACK_RESULT_HEADER.to_owned())
}

#[derive(Debug, Clone)]
pub(crate) struct PendingCall {
    pub(crate) label: String,
    pub(crate) name: String,
    /// Retained so structured result views can access original arguments
    /// without re-parsing the transcript.
    pub(crate) input: serde_json::Value,
}

#[derive(Debug, Default)]
pub(crate) struct PendingCalls {
    map: HashMap<String, PendingCall>,
}

impl PendingCalls {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, id: String, call: PendingCall) {
        self.map.insert(id, call);
    }

    pub(crate) fn remove(&mut self, id: &str) -> Option<PendingCall> {
        self.map.remove(id)
    }

    /// Evicts stale entries at turn boundaries.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }

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
