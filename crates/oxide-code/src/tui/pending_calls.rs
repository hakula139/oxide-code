//! Tool-call correlation state shared by live streaming and transcript resume.

use std::collections::HashMap;

/// Header for a result whose tool set no title and whose call is unknown, as when a `ToolCallEnd`
/// arrives without its `ToolCallStart` or a resumed transcript lost the call.
const FALLBACK_RESULT_HEADER: &str = "(result)";

/// Picks the display header for a tool result, preferring the tool's own post-execution title,
/// then the label its call showed, then the generic fallback.
pub(crate) fn result_header(title: Option<String>, call_label: Option<String>) -> String {
    title
        .or(call_label)
        .unwrap_or_else(|| FALLBACK_RESULT_HEADER.to_owned())
}

/// Labels of in-flight tool calls keyed by their id, kept so each result's header can fall back to
/// its call's label. Cleared at turn boundaries so orphan starts from a cancelled or errored turn
/// don't leak across turns.
#[derive(Debug, Default)]
pub(crate) struct PendingCalls {
    labels: HashMap<String, String>,
}

impl PendingCalls {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, id: String, label: String) {
        self.labels.insert(id, label);
    }

    pub(crate) fn remove(&mut self, id: &str) -> Option<String> {
        self.labels.remove(id)
    }

    pub(crate) fn clear(&mut self) {
        self.labels.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.labels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── result_header ──

    #[test]
    fn result_header_prefers_the_tool_title() {
        let header = result_header(
            Some("Edited f.rs".to_owned()),
            Some("Edit(/tmp/f.rs)".to_owned()),
        );

        assert_eq!(header, "Edited f.rs");
    }

    #[test]
    fn result_header_falls_back_to_the_call_label() {
        let header = result_header(None, Some("Edit(/tmp/f.rs)".to_owned()));

        assert_eq!(header, "Edit(/tmp/f.rs)");
    }

    #[test]
    fn result_header_falls_back_to_generic_header_for_orphans() {
        let header = result_header(None, None);

        assert_eq!(header, FALLBACK_RESULT_HEADER);
    }

    // ── PendingCalls::insert ──

    #[test]
    fn insert_overwrites_existing_entry() {
        let mut calls = PendingCalls::new();
        calls.insert("t1".to_owned(), "Edit(/tmp/f.rs)".to_owned());
        calls.insert("t1".to_owned(), "Edit(retry)".to_owned());
        assert_eq!(calls.remove("t1").as_deref(), Some("Edit(retry)"));
    }

    // ── PendingCalls::remove ──

    #[test]
    fn remove_drains_the_entry() {
        let mut calls = PendingCalls::new();
        calls.insert("t1".to_owned(), "Edit(/tmp/f.rs)".to_owned());
        assert_eq!(calls.remove("t1").as_deref(), Some("Edit(/tmp/f.rs)"));
        assert_eq!(calls.remove("t1"), None);
    }
}
