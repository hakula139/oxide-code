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

/// In-flight tool calls keyed by their id, each with its label and the index of its chat block, so
/// its result can fall back to the label as its header and sit right under the call. Cleared at
/// turn boundaries so orphan starts from a cancelled or errored turn don't leak across turns.
#[derive(Debug, Default)]
pub(crate) struct PendingCalls {
    calls: HashMap<String, (String, usize)>,
}

impl PendingCalls {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, id: String, label: String, block: usize) {
        self.calls.insert(id, (label, block));
    }

    /// Removes the call `id`, returning its label and block index.
    pub(crate) fn remove(&mut self, id: &str) -> Option<(String, usize)> {
        self.calls.remove(id)
    }

    /// Moves each call whose block sits at or after `at` one block down, after a block is inserted
    /// at `at`.
    pub(crate) fn shift_from(&mut self, at: usize) {
        for (_, block) in self.calls.values_mut() {
            if *block >= at {
                *block += 1;
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.calls.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.calls.len()
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
        calls.insert("t1".to_owned(), "Edit(/tmp/f.rs)".to_owned(), 0);
        calls.insert("t1".to_owned(), "Edit(retry)".to_owned(), 3);
        assert_eq!(calls.remove("t1"), Some(("Edit(retry)".to_owned(), 3)));
    }

    // ── PendingCalls::remove ──

    #[test]
    fn remove_drains_the_entry() {
        let mut calls = PendingCalls::new();
        calls.insert("t1".to_owned(), "Edit(/tmp/f.rs)".to_owned(), 0);
        assert_eq!(calls.remove("t1"), Some(("Edit(/tmp/f.rs)".to_owned(), 0)));
        assert_eq!(calls.remove("t1"), None);
    }

    // ── PendingCalls::shift_from ──

    #[test]
    fn shift_from_moves_only_the_calls_at_or_after_the_insertion() {
        let mut calls = PendingCalls::new();
        for (id, block) in [("before", 1), ("at", 2), ("after", 4)] {
            calls.insert(id.to_owned(), id.to_owned(), block);
        }

        calls.shift_from(2);

        let blocks = ["before", "at", "after"].map(|id| calls.remove(id).map(|(_, block)| block));
        assert_eq!(blocks, [Some(1), Some(3), Some(5)]);
    }
}
