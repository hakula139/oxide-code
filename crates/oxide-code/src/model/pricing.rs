//! Token cost rates and per-million USD pricing.
//!
//! Rates are quoted in USD per million tokens for first-party Anthropic API. Values exclude
//! account discounts, marketplace billing, data-residency multipliers, fast-mode adjustments,
//! and server-side tool surcharges. Update both this table and `MODELS` in lockstep when a row
//! changes pricing.

use crate::config::PromptCacheTtl;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TokenCostRates {
    input: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
    cache_read: f64,
    output: f64,
}

pub(super) const OPUS_4_5_PLUS_RATES: TokenCostRates = TokenCostRates {
    input: 5.0,
    cache_write_5m: 6.25,
    cache_write_1h: 10.0,
    cache_read: 0.50,
    output: 25.0,
};

pub(super) const SONNET_RATES: TokenCostRates = TokenCostRates {
    input: 3.0,
    cache_write_5m: 3.75,
    cache_write_1h: 6.0,
    cache_read: 0.30,
    output: 15.0,
};

pub(super) const HAIKU_RATES: TokenCostRates = TokenCostRates {
    input: 1.0,
    cache_write_5m: 1.25,
    cache_write_1h: 2.0,
    cache_read: 0.10,
    output: 5.0,
};

impl TokenCostRates {
    pub(crate) fn estimate_usd(
        self,
        input_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        output_tokens: u32,
        cache_ttl: PromptCacheTtl,
    ) -> f64 {
        let cache_write = match cache_ttl {
            PromptCacheTtl::FiveMin => self.cache_write_5m,
            PromptCacheTtl::OneHour => self.cache_write_1h,
        };
        million_tokens(input_tokens) * self.input
            + million_tokens(cache_creation_input_tokens) * cache_write
            + million_tokens(cache_read_input_tokens) * self.cache_read
            + million_tokens(output_tokens) * self.output
    }
}

fn million_tokens(tokens: u32) -> f64 {
    f64::from(tokens) / 1_000_000.0
}
