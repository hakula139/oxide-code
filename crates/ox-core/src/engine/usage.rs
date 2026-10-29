//! Usage accounting: what each request bills, the context usage and session cost the engine
//! shows, and the auto-compaction failures that pause it.

use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;

use super::Engine;
use super::compaction::AutoCompactFailures;
use super::turn::TurnReport;
use crate::model::{self, TokenUsage};
use crate::protocol::{Event, EventSink, UsageSnapshot};
use crate::session::store::UsageRecord;

/// The live session's billing, its context usage, and the failures that pause auto compaction. The
/// engine replaces it whole when it adopts another session's transcript.
pub(super) struct UsageLedger {
    /// [`TurnReport::context_estimate`] of the latest turn the transcript keeps, or an estimate
    /// from the parts of the next request when that turn reported no usage.
    context_estimate: u32,
    /// The latest usage any turn the transcript keeps reported, which the status line shows.
    displayed_usage: Option<TokenUsage>,
    total_estimated_cost_usd: f64,
    /// Delivers the usage of the live session's title request, which joins the session total once
    /// it lands.
    title_usage: Option<oneshot::Receiver<UsageRecord>>,
    /// Automatic compaction failures in a row, which pause it once they reach the limit.
    pub(super) auto_compaction_failures: AutoCompactFailures,
}

impl UsageLedger {
    /// The ledger of a session whose transcript recorded `usage`, seeded with the next request's
    /// context `estimate` and no title request in flight.
    pub(super) fn for_session(usage: &[UsageRecord], estimate: u32) -> Self {
        Self {
            context_estimate: estimate,
            displayed_usage: None,
            total_estimated_cost_usd: estimate_cost_usd(usage),
            title_usage: None,
            auto_compaction_failures: AutoCompactFailures::default(),
        }
    }

    pub(super) const fn context_estimate(&self) -> u32 {
        self.context_estimate
    }

    /// Takes a turn's context `estimate` and, when the turn reported any, its `usage`.
    fn observe_turn(&mut self, estimate: u32, usage: Option<TokenUsage>) {
        self.context_estimate = estimate;
        if usage.is_some() {
            self.displayed_usage = usage;
        }
    }

    /// Adds the estimated cost of `records` to the session total.
    fn bill(&mut self, records: &[UsageRecord]) {
        self.total_estimated_cost_usd += estimate_cost_usd(records);
    }

    /// Forgets the usage and auto-compaction failures of a transcript the session no longer sends,
    /// taking the context `estimate` of the next request's parts until a turn reports usage.
    fn reset_context(&mut self, sink: &dyn EventSink, estimate: u32) {
        self.auto_compaction_failures.reset(sink);
        self.context_estimate = estimate;
        self.displayed_usage = None;
    }

    /// Shows the context usage against `context_window` and the session cost, first adding the
    /// title request's cost once its usage lands.
    fn emit_update(&mut self, sink: &dyn EventSink, context_window: Option<u32>) {
        self.absorb_title_usage();
        let estimated_cost_usd =
            (self.total_estimated_cost_usd > 0.0).then_some(self.total_estimated_cost_usd);
        if self.displayed_usage.is_none() && estimated_cost_usd.is_none() {
            return;
        }
        sink.emit(Event::UsageUpdated(UsageSnapshot {
            context_tokens: self.displayed_usage.map(TokenUsage::context_tokens),
            context_window,
            estimated_cost_usd,
        }));
    }

    /// Holds `usage`, the receiver for the title request just started, until its usage joins the
    /// session total.
    pub(super) fn track_title_usage(&mut self, usage: oneshot::Receiver<UsageRecord>) {
        self.title_usage = Some(usage);
    }

    /// Accounts the title request once it ends, returning whether it billed any usage. Pending
    /// until then, and forever when no title request is in flight.
    pub(super) async fn title_usage_landed(&mut self) -> bool {
        let record = match &mut self.title_usage {
            Some(pending) => pending.await.ok(),
            None => std::future::pending().await,
        };
        self.account_title_usage(record)
    }

    /// Adds the title request's cost to the session total once its usage lands, and stops waiting
    /// once the request has ended, with or without usage.
    fn absorb_title_usage(&mut self) {
        let Some(pending) = &mut self.title_usage else {
            return;
        };
        let record = match pending.try_recv() {
            Ok(record) => Some(record),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Closed) => None,
        };
        self.account_title_usage(record);
    }

    /// Consumes the title result and returns whether it reported usage.
    fn account_title_usage(&mut self, record: Option<UsageRecord>) -> bool {
        self.title_usage = None;
        let Some(record) = record else {
            return false;
        };
        self.bill(&[record]);
        true
    }
}

#[cfg(test)]
impl UsageLedger {
    pub(super) const fn tracks_title_usage(&self) -> bool {
        self.title_usage.is_some()
    }

    pub(super) const fn displayed_usage(&self) -> Option<TokenUsage> {
        self.displayed_usage
    }

    pub(super) const fn total_estimated_cost_usd(&self) -> f64 {
        self.total_estimated_cost_usd
    }

    pub(super) const fn set_context_estimate(&mut self, estimate: u32) {
        self.context_estimate = estimate;
    }

    pub(super) const fn set_displayed_usage(&mut self, usage: TokenUsage) {
        self.displayed_usage = Some(usage);
    }

    pub(super) const fn set_total_estimated_cost_usd(&mut self, cost: f64) {
        self.total_estimated_cost_usd = cost;
    }
}

impl Engine {
    pub(super) async fn account_usage(&mut self, sink: &dyn EventSink, report: &TurnReport) {
        let estimate = report.context_estimate(
            self.transcript.prompt(),
            &self.tools.definitions(),
            self.transcript.messages(),
        );
        let usage = report.usage();
        self.usage.observe_turn(estimate, usage);
        if usage.is_none() {
            return;
        }
        self.bill(sink, &report.billable_rounds).await;
        self.emit_usage_update(sink);
    }

    /// Records each request in `requests`, priced by the session's model and cache TTL, and adds
    /// its estimated cost to the session total.
    pub(super) async fn bill(&mut self, sink: &dyn EventSink, requests: &[TokenUsage]) {
        if requests.is_empty() {
            return;
        }
        let records: Vec<_> = requests
            .iter()
            .map(|&tokens| UsageRecord {
                model: self.settings.model().id().to_owned(),
                cache_ttl: self.settings.prompt_cache_ttl(),
                tokens,
            })
            .collect();
        self.usage.bill(&records);
        let report = self.transcript.record_usage(records).await;
        sink.session_write(&report);
    }

    /// [`UsageLedger::reset_context`] with the estimate of the next request's parts.
    pub(super) fn reset_context_usage(&mut self, sink: &dyn EventSink) {
        let estimate = self.transcript.request_estimate(&self.tools.definitions());
        self.usage.reset_context(sink, estimate);
    }

    /// [`UsageLedger::emit_update`] against the session model's context window.
    pub(super) fn emit_usage_update(&mut self, sink: &dyn EventSink) {
        self.usage
            .emit_update(sink, self.settings.model().context_window());
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
