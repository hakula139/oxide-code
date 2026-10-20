//! TUI component modules.

pub(crate) mod chat;
pub(crate) mod input;
pub(crate) mod status;
pub(crate) mod tasks;
pub(crate) mod welcome;

use ox_core::util::text::plural;

/// Left margin that lines the status line and the task panel up with the chat blocks.
const LEFT_MARGIN: &str = "  ";

/// Counts the items an overflow leaves out, as in `... +3 lines`.
pub(super) fn overflow_count(hidden: usize, singular: &str, plural_form: &str) -> String {
    format!("... +{hidden} {}", plural(hidden, singular, plural_form))
}
