//! Model client contract: the [`Wire`] streaming trait and the wire-neutral events, reply, and
//! rate limits it reports. [`anthropic`] implements it for the Anthropic Messages API.

pub mod anthropic;

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use strum::{IntoStaticStr, VariantArray};
use tokio::sync::mpsc;

use crate::message::{ContentBlock, Message};
use crate::model::TokenUsage;
use crate::prompt::SystemPrompt;
use crate::tool::ToolDefinition;

// ── Wire ──

/// Streams model responses over one provider's API. A wire retries a failed attempt only before it
/// streams any text, thinking, or progress, since a consumer cannot retract what it showed.
pub(crate) trait Wire: Send + Sync {
    /// Streams a conversation request. The receiver yields [`ResponseEvent`]s ending in one
    /// [`ResponseEvent::Completed`], or a final `Err` when the request fails.
    fn stream_message(
        &self,
        messages: &[Message],
        system: &SystemPrompt,
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>>;

    /// Streams a request no later request extends, so caching its transcript tail would only add a
    /// write premium. Defaults to [`Self::stream_message`] for wires that do no caching.
    fn stream_one_shot(
        &self,
        messages: &[Message],
        system: &SystemPrompt,
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<ResponseEvent>>> {
        self.stream_message(messages, system, user_context, tools)
    }
}

// ── Response ──

/// One step of a streamed response, in the order the model produced it.
#[derive(Debug, PartialEq)]
pub(crate) enum ResponseEvent {
    /// Reply text as it streams.
    Text(String),
    /// Reasoning text as it streams.
    Thinking(String),
    /// What the model says it is about to do, sent as the tool call it introduces starts.
    Progress(String),
    /// The request's usage so far. Each snapshot replaces the last, including one from an attempt
    /// that was retried. Usage streams apart from the reply so a request that fails or is cancelled
    /// midway still bills.
    Usage(TokenUsage),
    /// The account's usage limits as of this request, from a wire that meters subscription use.
    RateLimits(RateLimits),
    /// A failed attempt about to be retried, as a user-facing notice.
    Retrying(String),
    /// The finished reply. No event follows it.
    Completed(Response),
}

/// A finished reply, its content blocks in the order the model produced them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Response {
    pub(crate) content: Vec<ContentBlock>,
    /// Why a tool call's streamed input was not valid JSON, by call id. Its `input` is `{}`.
    pub(crate) parse_errors: HashMap<String, String>,
    /// `None` when the response omitted it, in which case the content alone decides the next step.
    pub(crate) stop_reason: Option<StopReason>,
    pub(crate) stop_details: Option<StopDetails>,
}

/// Why the model stopped. Variants follow Anthropic's `stop_reason` values, which its wire
/// deserializes directly. `Unknown` keeps a future value from failing the whole `message_delta`
/// frame, which would also drop its usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, IntoStaticStr, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    /// A long server-side turn paused. Re-send the transcript with this reply appended to resume.
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
    #[serde(other)]
    Unknown,
}

impl StopReason {
    /// The wire's own name for this reason.
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Structured refusal detail. Both fields are informational and may be absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct StopDetails {
    pub(crate) category: Option<String>,
    pub(crate) explanation: Option<String>,
}

// ── Rate Limits ──

/// An account's subscription usage limits. A window the provider did not report is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<RateLimitWindow>,
    pub seven_day: Option<RateLimitWindow>,
}

/// One rolling usage window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    /// Share of the window's allowance spent, where 1 is all of it.
    pub utilization: f64,
    /// When the window resets, in Unix seconds.
    pub resets_at: i64,
}

// ── Errors ──

/// A request rejected for a prompt over the context window. Displays as the wire's diagnostic.
#[derive(Debug, thiserror::Error)]
#[error("{diagnostic}")]
pub(crate) struct PromptTooLong {
    /// Tokens over the limit, when the rejection names both counts.
    pub(crate) overflow: Option<u32>,
    diagnostic: String,
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use super::{RateLimitWindow, RateLimits};

    /// Limits with the five-hour window 42% spent and no seven-day window. The window resets at
    /// 2100-01-01, so it stays in the future while tests run.
    pub(crate) fn five_hour_limit() -> RateLimits {
        RateLimits {
            five_hour: Some(RateLimitWindow {
                utilization: 0.42,
                resets_at: 4_102_444_800,
            }),
            seven_day: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod stop_reason {
        use super::*;

        mod as_str {
            use super::*;

            #[test]
            fn names_each_variant_the_way_its_wire_value_does() {
                for &reason in StopReason::VARIANTS {
                    if reason == StopReason::Unknown {
                        continue;
                    }
                    let wire = format!("\"{}\"", reason.as_str());
                    assert_eq!(
                        serde_json::from_str::<StopReason>(&wire).unwrap(),
                        reason,
                        "{reason:?}",
                    );
                }
            }
        }
    }
}
