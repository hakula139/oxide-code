//! Per-model token cost rates, which [`MODELS`](super::MODELS) attaches to each catalog row.
//!
//! Rates are quoted in USD per million tokens for the first-party Anthropic API. Values exclude
//! account discounts, marketplace billing, data-residency multipliers, fast-mode adjustments, and
//! server-side tool surcharges.

use super::TokenUsage;
use crate::config::PromptCacheTtl;

/// A model's price list. Some models switch to a higher card once a single request's prompt, its
/// [`TokenUsage::context_tokens`], crosses a size threshold, so callers price each request alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TokenCostRates {
    base: RateCard,
    long_prompt: Option<LongPromptRates>,
}

/// Card that replaces the base card when a request's prompt exceeds `above_tokens`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LongPromptRates {
    above_tokens: u32,
    rates: RateCard,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct RateCard {
    input: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
    cache_read: f64,
    output: f64,
}

pub(super) const FABLE_5_1_RATES: TokenCostRates = TokenCostRates {
    base: RateCard {
        input: 10.0,
        cache_write_5m: 12.50,
        cache_write_1h: 20.0,
        cache_read: 0.25,
        output: 50.0,
    },
    long_prompt: None,
};

pub(super) const OPUS_5_5_RATES: TokenCostRates = TokenCostRates {
    base: RateCard {
        input: 4.0,
        cache_write_5m: 5.0,
        cache_write_1h: 8.0,
        cache_read: 0.20,
        output: 20.0,
    },
    long_prompt: None,
};

pub(super) const SONNET_5_5_RATES: TokenCostRates = TokenCostRates {
    base: RateCard {
        input: 2.0,
        cache_write_5m: 2.50,
        cache_write_1h: 4.0,
        cache_read: 0.10,
        output: 10.0,
    },
    long_prompt: None,
};

pub(super) const HAIKU_5_5_RATES: TokenCostRates = TokenCostRates {
    base: RateCard {
        input: 0.10,
        cache_write_5m: 0.125,
        cache_write_1h: 0.20,
        cache_read: 0.01,
        output: 0.50,
    },
    long_prompt: Some(LongPromptRates {
        above_tokens: 100_000,
        rates: RateCard {
            input: 0.50,
            cache_write_5m: 0.625,
            cache_write_1h: 1.0,
            cache_read: 0.05,
            output: 2.50,
        },
    }),
};

impl TokenCostRates {
    /// Returns the USD cost of one request's `usage`.
    pub(crate) fn estimate_usd(self, usage: TokenUsage, cache_ttl: PromptCacheTtl) -> f64 {
        let rates = match self.long_prompt {
            Some(long) if usage.context_tokens() > long.above_tokens => long.rates,
            _ => self.base,
        };
        let cache_write = match cache_ttl {
            PromptCacheTtl::FiveMin => rates.cache_write_5m,
            PromptCacheTtl::OneHour => rates.cache_write_1h,
        };
        million_tokens(usage.input_tokens) * rates.input
            + million_tokens(usage.cache_creation_input_tokens) * cache_write
            + million_tokens(usage.cache_read_input_tokens) * rates.cache_read
            + million_tokens(usage.output_tokens) * rates.output
    }
}

fn million_tokens(tokens: u32) -> f64 {
    f64::from(tokens) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_usd(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected ${expected}, got ${actual}"
        );
    }

    // ── TokenCostRates::estimate_usd ──

    #[test]
    fn estimate_usd_flat_rates_price_every_token_class_and_cache_ttl() {
        // 1M of each class isolates the per-class rate: 4 input + 5 / 8 write + 0.20 read + 20.
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            output_tokens: 1_000_000,
        };

        let five_min = OPUS_5_5_RATES.estimate_usd(usage, PromptCacheTtl::FiveMin);
        let one_hour = OPUS_5_5_RATES.estimate_usd(usage, PromptCacheTtl::OneHour);

        assert_usd(five_min, 29.2);
        assert_usd(one_hour, 32.2);
    }

    #[test]
    fn estimate_usd_tiered_rates_switch_card_only_above_prompt_threshold() {
        // 100K prompt tokens bill on the base card at $0.10 / MTok input. One more token moves the
        // whole request, output included, onto the long-prompt card at $0.50 / $2.50.
        let at_threshold = HAIKU_5_5_RATES
            .estimate_usd(TokenUsage::new(100_000, 1_000_000), PromptCacheTtl::FiveMin);
        let above_threshold = HAIKU_5_5_RATES
            .estimate_usd(TokenUsage::new(100_001, 1_000_000), PromptCacheTtl::FiveMin);

        assert_usd(at_threshold, 0.01 + 0.50);
        assert_usd(above_threshold, 0.100_001 * 0.50 + 2.50);
    }

    #[test]
    fn estimate_usd_tiered_prompt_size_counts_cache_tokens() {
        // The 151K prompt includes cache writes and reads, so every class bills on the long-prompt
        // card ($0.50 input, $1.00 1h write, $0.05 read).
        let usage = TokenUsage {
            cache_creation_input_tokens: 50_000,
            cache_read_input_tokens: 100_000,
            ..TokenUsage::new(1_000, 0)
        };

        let cost = HAIKU_5_5_RATES.estimate_usd(usage, PromptCacheTtl::OneHour);

        assert_usd(cost, 0.001 * 0.50 + 0.05 * 1.0 + 0.1 * 0.05);
    }
}
