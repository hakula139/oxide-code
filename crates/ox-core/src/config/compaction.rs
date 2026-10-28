//! Compaction settings: whether auto-compaction runs and the token count that triggers it, derived
//! from the configured threshold and the model's context window.

use std::fmt;
use std::ops::RangeInclusive;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::merge::Origins;
use super::provenance::{Source, Sourced};
use super::schema::CompactionTable;
use super::{env_number, resolve_flag};
use crate::model::ModelSpec;
use crate::util::path::tildify;
use crate::util::text::compact_count;

const AUTO_COMPACTION_OUTPUT_RESERVE_CAP: u32 = 20_000;
const AUTO_COMPACTION_BUFFER_TOKENS: u32 = 13_000;
const MIN_AUTO_COMPACTION_THRESHOLD_TOKENS: u32 = 50_000;

// ── CompactionConfig ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub auto: AutoCompactionConfig,
    policy: AutoCompactionPolicy,
}

impl CompactionConfig {
    pub(crate) fn for_model(self, model: &ModelSpec, max_tokens: u32) -> Self {
        resolve_policy(self.policy, model, max_tokens)
    }

    /// The default policy resolved for `model` with `max_tokens` of output.
    #[cfg(test)]
    pub(crate) fn with_default_policy(model: &ModelSpec, max_tokens: u32) -> Self {
        resolve_policy(AutoCompactionPolicy::Default, model, max_tokens)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoCompactionConfig {
    pub enabled: bool,
    pub threshold_tokens: Option<u32>,
}

impl AutoCompactionConfig {
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            threshold_tokens: None,
        }
    }

    pub(crate) const fn should_trigger(self, context_tokens: u32) -> bool {
        match (self.enabled, self.threshold_tokens) {
            (true, Some(threshold)) => context_tokens >= threshold,
            _ => false,
        }
    }

    /// When compaction triggers, with an abbreviated count, as `/status` and the config-change
    /// notice show it.
    pub fn abbreviated(self) -> String {
        match (self.enabled, self.threshold_tokens) {
            (true, Some(threshold)) => format!("at {} tokens", compact_count(threshold)),
            _ => self.to_string(),
        }
    }
}

/// When compaction triggers, or why it never does, as `/config` shows it, with the exact count.
impl fmt::Display for AutoCompactionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.enabled, self.threshold_tokens) {
            (true, Some(threshold)) => write!(f, "at {threshold} tokens"),
            (true, None) => f.write_str("off (no threshold)"),
            _ => f.write_str("off"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCompactionPolicy {
    Disabled,
    Default,
    Tokens(u32),
    Percent(ThresholdPercent),
}

/// A share of the context window, validated to [`Self::RANGE`] when the config loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ThresholdPercent(u8);

impl ThresholdPercent {
    pub(super) const RANGE: RangeInclusive<u8> = 1..=100;
}

// ── Resolution ──

/// Resolves the compaction config, sourced from the threshold if one applies, else `auto_enabled`.
pub(super) fn resolve(
    file: &CompactionTable,
    origins: &Origins,
    model: &ModelSpec,
    max_tokens: u32,
) -> Result<Sourced<CompactionConfig>> {
    let auto_requested = resolve_flag(
        "OX_COMPACTION_AUTO_ENABLED",
        origins.sourced("client.compaction.auto_enabled", file.enabled),
        true,
    )?;
    // Parsed even when disabled, so a bad value fails the load as it does in a config file.
    let env_tokens = env_number("OX_COMPACTION_AUTO_THRESHOLD_TOKENS", 0..=u32::MAX)?;
    let env_percent = env_number(
        "OX_COMPACTION_AUTO_THRESHOLD_PERCENT",
        ThresholdPercent::RANGE,
    )?
    .map(|percent| percent.map(ThresholdPercent));

    let policy = if auto_requested.value {
        resolve_auto_threshold(env_tokens, env_percent, file, origins)?
            .unwrap_or_else(|| auto_requested.map(|_| AutoCompactionPolicy::Default))
    } else {
        auto_requested.map(|_| AutoCompactionPolicy::Disabled)
    };
    Ok(policy.map(|policy| resolve_policy(policy, model, max_tokens)))
}

fn resolve_policy(
    policy: AutoCompactionPolicy,
    model: &ModelSpec,
    max_tokens: u32,
) -> CompactionConfig {
    let auto = resolve_auto_compaction(policy, model, max_tokens);
    CompactionConfig { auto, policy }
}

fn resolve_auto_compaction(
    policy: AutoCompactionPolicy,
    model: &ModelSpec,
    max_tokens: u32,
) -> AutoCompactionConfig {
    let threshold = match policy {
        AutoCompactionPolicy::Disabled => return AutoCompactionConfig::disabled(),
        AutoCompactionPolicy::Default => default_auto_threshold(model, max_tokens),
        AutoCompactionPolicy::Tokens(tokens) => {
            Some(threshold_from_tokens(tokens, model, max_tokens))
        }
        AutoCompactionPolicy::Percent(percent) => {
            threshold_from_percent(percent, model, max_tokens)
        }
    };
    AutoCompactionConfig {
        enabled: true,
        threshold_tokens: threshold,
    }
}

/// The configured threshold policy, `None` when neither threshold is set. Env thresholds replace
/// the file thresholds as a pair.
fn resolve_auto_threshold(
    env_tokens: Option<Sourced<u32>>,
    env_percent: Option<Sourced<ThresholdPercent>>,
    file: &CompactionTable,
    origins: &Origins,
) -> Result<Option<Sourced<AutoCompactionPolicy>>> {
    let (tokens, percent) = if env_tokens.is_some() || env_percent.is_some() {
        (env_tokens, env_percent)
    } else {
        (
            origins.sourced(
                "client.compaction.auto_threshold_tokens",
                file.threshold_tokens,
            ),
            origins.sourced(
                "client.compaction.auto_threshold_percent",
                file.threshold_percent.map(ThresholdPercent),
            ),
        )
    };

    match (tokens, percent) {
        (Some(tokens), Some(_)) => {
            // Both come from one place: the env pair, or one file since layers clear the pair.
            let both = match &tokens.source {
                Source::File(layer) => format!(
                    "{} sets both auto_threshold_tokens and auto_threshold_percent",
                    tildify(layer.path())
                ),
                _ => "both OX_COMPACTION_AUTO_THRESHOLD_TOKENS \
                      and OX_COMPACTION_AUTO_THRESHOLD_PERCENT are set"
                    .to_owned(),
            };
            bail!("{both}. Set only one compaction threshold.")
        }
        (Some(tokens), None) => Ok(Some(tokens.map(AutoCompactionPolicy::Tokens))),
        (None, Some(percent)) => Ok(Some(percent.map(AutoCompactionPolicy::Percent))),
        (None, None) => Ok(None),
    }
}

// ── Threshold Math ──

fn threshold_from_tokens(tokens: u32, model: &ModelSpec, max_tokens: u32) -> u32 {
    clamp_threshold_floor(clamp_threshold_ceil(tokens, model, max_tokens))
}

fn threshold_from_percent(
    ThresholdPercent(percent): ThresholdPercent,
    model: &ModelSpec,
    max_tokens: u32,
) -> Option<u32> {
    let context_window = model.context_window()?;
    let raw = context_window.saturating_mul(u32::from(percent)) / 100;
    Some(clamp_threshold_floor(clamp_threshold_ceil(
        raw, model, max_tokens,
    )))
}

/// Snaps `tokens` down to the model's safe trigger when one is known.
fn clamp_threshold_ceil(tokens: u32, model: &ModelSpec, max_tokens: u32) -> u32 {
    default_auto_threshold(model, max_tokens).map_or(tokens, |max| tokens.min(max))
}

/// Snaps `tokens` up to [`MIN_AUTO_COMPACTION_THRESHOLD_TOKENS`].
fn clamp_threshold_floor(tokens: u32) -> u32 {
    tokens.max(MIN_AUTO_COMPACTION_THRESHOLD_TOKENS)
}

fn default_auto_threshold(model: &ModelSpec, max_tokens: u32) -> Option<u32> {
    model
        .context_window()
        .and_then(|window| default_auto_threshold_for_window(window, max_tokens))
}

fn default_auto_threshold_for_window(context_window: u32, max_tokens: u32) -> Option<u32> {
    let reserve = max_tokens.min(AUTO_COMPACTION_OUTPUT_RESERVE_CAP);
    context_window
        .checked_sub(reserve)?
        .checked_sub(AUTO_COMPACTION_BUFFER_TOKENS)
}

// ── Testing ──

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::{AutoCompactionConfig, AutoCompactionPolicy, CompactionConfig};

    /// Output of [`default_auto_threshold`](super::default_auto_threshold) for a 1M-token window
    /// when `max_tokens` saturates the reserve cap.
    pub const WINDOW_1M_THRESHOLD: u32 = 967_000;

    impl CompactionConfig {
        pub(crate) const fn disabled() -> Self {
            Self {
                auto: AutoCompactionConfig::disabled(),
                policy: AutoCompactionPolicy::Disabled,
            }
        }

        pub const fn with_threshold(tokens: u32) -> Self {
            Self {
                auto: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(tokens),
                },
                policy: AutoCompactionPolicy::Tokens(tokens),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::WINDOW_1M_THRESHOLD;
    use super::*;
    use crate::model::MODELS;

    mod for_model {
        use super::*;

        #[test]
        fn re_resolves_a_percent_threshold_against_the_new_window() {
            let policy = AutoCompactionPolicy::Percent(ThresholdPercent(40));
            let unknown = ModelSpec::resolve("custom-model");
            let compaction = resolve_policy(policy, &unknown, 16_000);
            assert_eq!(
                compaction.auto.threshold_tokens, None,
                "precondition: no window"
            );

            let known = ModelSpec::resolve("claude-opus-5-5");
            let swapped = compaction.for_model(&known, 64_000);

            assert_eq!(
                swapped.auto,
                AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(400_000),
                }
            );
            assert_eq!(swapped.policy, policy);
        }
    }

    mod auto_compaction_config {
        use super::*;

        #[test]
        fn display_names_the_threshold_or_why_it_is_off() {
            let windowless = AutoCompactionConfig {
                enabled: true,
                threshold_tokens: None,
            };
            let windowed = AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(400_000),
            };

            assert_eq!(windowed.to_string(), "at 400000 tokens");
            assert_eq!(windowless.to_string(), "off (no threshold)");
            assert_eq!(AutoCompactionConfig::disabled().to_string(), "off");
            assert_eq!(windowed.abbreviated(), "at 400k tokens");
            assert_eq!(windowless.abbreviated(), "off (no threshold)");
            assert_eq!(AutoCompactionConfig::disabled().abbreviated(), "off");
        }

        mod should_trigger {
            use super::*;

            #[test]
            fn requires_enabled_threshold_and_enough_tokens() {
                let enabled = AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(10),
                };
                assert!(enabled.should_trigger(10));
                assert!(!enabled.should_trigger(9));
                assert!(!AutoCompactionConfig::disabled().should_trigger(100));
                assert!(
                    !AutoCompactionConfig {
                        enabled: true,
                        threshold_tokens: None,
                    }
                    .should_trigger(100)
                );
            }
        }
    }

    mod resolve_policy {
        use super::*;

        #[test]
        fn bounds_the_threshold_by_the_floor_and_the_model_window() {
            let (opus, unknown) = ("claude-opus-5-5", "custom-model");
            for (id, policy, threshold) in [
                (opus, AutoCompactionPolicy::Tokens(400_000), Some(400_000)),
                (opus, AutoCompactionPolicy::Tokens(0), Some(50_000)),
                (opus, AutoCompactionPolicy::Tokens(49_999), Some(50_000)),
                (
                    opus,
                    AutoCompactionPolicy::Tokens(990_000),
                    Some(WINDOW_1M_THRESHOLD),
                ),
                (
                    opus,
                    AutoCompactionPolicy::Percent(ThresholdPercent(40)),
                    Some(400_000),
                ),
                (
                    opus,
                    AutoCompactionPolicy::Percent(ThresholdPercent(4)),
                    Some(50_000),
                ),
                (
                    unknown,
                    AutoCompactionPolicy::Percent(ThresholdPercent(40)),
                    None,
                ),
            ] {
                let model = ModelSpec::resolve(id);
                assert_eq!(
                    resolve_policy(policy, &model, 64_000).auto,
                    AutoCompactionConfig {
                        enabled: true,
                        threshold_tokens: threshold,
                    },
                    "{id} {policy:?}",
                );
            }
        }
    }

    mod window_1m_threshold {
        use super::*;

        #[test]
        fn is_the_default_for_every_known_model() {
            for info in MODELS {
                let model = ModelSpec::resolve(info.id);
                assert_eq!(
                    default_auto_threshold(&model, 64_000),
                    Some(WINDOW_1M_THRESHOLD),
                    "{}",
                    info.id,
                );
            }
            let unknown = ModelSpec::resolve("custom-model");
            assert_eq!(default_auto_threshold(&unknown, 64_000), None);
        }
    }
}
