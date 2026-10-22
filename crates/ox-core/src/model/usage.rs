//! Per-request token counts, which pricing, auto-compaction, and the status line read, and session
//! files record.

use serde::{Deserialize, Serialize};

/// Token counts one request reported. A count the payload omits reads as zero.
#[expect(
    clippy::struct_field_names,
    reason = "field names mirror Anthropic's usage payload, which session `usage` entries reuse"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u32,
    pub(crate) cache_creation_input_tokens: u32,
    pub(crate) cache_read_input_tokens: u32,
    pub(crate) output_tokens: u32,
}

impl TokenUsage {
    #[cfg(test)]
    pub(crate) const fn new(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            input_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens,
        }
    }

    /// Returns the prompt size: input tokens plus cache writes and reads.
    pub(crate) const fn context_tokens(self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_includes_cache_tokens() {
        let usage = TokenUsage {
            input_tokens: 10,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 30,
            output_tokens: 5,
        };

        assert_eq!(usage.context_tokens(), 60);
    }
}
