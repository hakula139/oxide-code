//! Usage accounting: what each request bills, and the context usage and session cost the
//! engine shows.

use tokio::sync::oneshot::error::TryRecvError;

use super::Engine;
use super::turn::TurnReport;
use crate::model::{self, TokenUsage};
use crate::protocol::{Event, EventSink, UsageSnapshot};
use crate::session::entry::UsageRecord;

impl Engine {
    pub(super) async fn account_usage(&mut self, sink: &dyn EventSink, report: &TurnReport) {
        self.last_usage = report.usage();
        if self.last_usage.is_none() {
            return;
        }
        self.displayed_usage = self.last_usage;
        self.bill(sink, &report.billable_rounds).await;
        self.emit_usage_update(sink);
    }

    /// Records each request in `requests` and adds its estimated cost to the session total.
    pub(super) async fn bill(&mut self, sink: &dyn EventSink, requests: &[TokenUsage]) {
        if requests.is_empty() {
            return;
        }
        let records: Vec<_> = requests
            .iter()
            .map(|&tokens| UsageRecord {
                model: self.client.model().id().to_owned(),
                cache_ttl: self.client.prompt_cache_ttl(),
                tokens,
            })
            .collect();
        self.total_estimated_cost_usd += estimate_cost_usd(&records);
        let outcome = self.session.record_usage(records).await;
        sink.session_write_error(outcome.failure.as_deref());
    }

    /// Forgets the usage and auto-compaction failures of a transcript the session no longer sends.
    pub(super) fn reset_context_usage(&mut self, sink: &dyn EventSink) {
        self.auto_compaction_failures.reset(sink);
        self.last_usage = None;
        self.displayed_usage = None;
    }

    /// Shows the context usage and session cost, first adding the title request's cost once its
    /// usage lands.
    pub(super) fn emit_usage_update(&mut self, sink: &dyn EventSink) {
        self.absorb_title_usage();
        let estimated_cost_usd =
            (self.total_estimated_cost_usd > 0.0).then_some(self.total_estimated_cost_usd);
        if self.displayed_usage.is_none() && estimated_cost_usd.is_none() {
            return;
        }
        sink.emit(
            Event::UsageUpdated(UsageSnapshot {
                context_tokens: self.displayed_usage.map(TokenUsage::context_tokens),
                context_window: self.client.model().context_window(),
                estimated_cost_usd,
            }),
            "usage-updated",
        );
    }

    /// Adds the title request's cost to the session total once its usage lands, and stops waiting
    /// once the request has ended, with or without usage.
    fn absorb_title_usage(&mut self) {
        let Some(pending) = &mut self.title_usage else {
            return;
        };
        match pending.try_recv() {
            Ok(record) => self.total_estimated_cost_usd += estimate_cost_usd(&[record]),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Closed) => {}
        }
        self.title_usage = None;
    }
}

/// Estimated USD cost of `requests`, each priced by the model and cache TTL it ran with.
pub(super) fn estimate_cost_usd(requests: &[UsageRecord]) -> f64 {
    requests
        .iter()
        .filter_map(|request| {
            let rates = model::cost_rates(&request.model)?;
            Some(rates.estimate_usd(request.tokens, request.cache_ttl))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod estimate_cost_usd {
        use super::*;

        #[test]
        fn prices_each_request_by_its_own_model_and_cache_ttl() {
            let request = |model: &str, cache_ttl: &str| -> UsageRecord {
                serde_json::from_value(serde_json::json!({
                    "model": model,
                    "cache_ttl": cache_ttl,
                    "input_tokens": 0,
                    "cache_creation_input_tokens": 1_000_000,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 0,
                }))
                .unwrap()
            };

            let cost = estimate_cost_usd(&[
                request("claude-opus-5-5", "1h"),
                request("claude-sonnet-5-5", "5m"),
                request("unpriced-model", "5m"),
            ]);

            assert!(
                (cost - (8.0 + 2.5)).abs() < 1e-9,
                "a 1h write on Opus, a 5m write on Sonnet, and nothing for an unknown model: {cost}"
            );
        }
    }
}
