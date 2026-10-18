//! Catalog of the supported Claude models, matched by exact id.
//!
//! Every row shares one request surface: adaptive thinking, the full `output_config.effort`
//! ladder, `clear_thinking` context management, a 1M-token context window, and 128K output.
//! Other ids (custom gateway names, retired models) pass through verbatim with no known
//! capabilities, so their requests carry none of those opt-ins and auto-compaction stays off.

mod pricing;
mod usage;

use std::fmt::Write;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::Effort;
pub(crate) use pricing::TokenCostRates;
use pricing::{FABLE_5_1_RATES, HAIKU_5_5_RATES, OPUS_5_5_RATES, SONNET_5_5_RATES};
pub(crate) use usage::TokenUsage;

// ── ModelInfo ──

/// One row in the [`MODELS`] catalog.
#[derive(Debug)]
pub(crate) struct ModelInfo {
    pub(crate) id: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) family: Family,
    pub(crate) cutoff: &'static str,
    /// The API's own default when a request omits `output_config.effort`. Sent explicitly so the
    /// UI shows the level the request actually runs at.
    pub(crate) default_effort: Effort,
    /// Whether the model accepts `thinking.display = "updates"`.
    pub(crate) progress_updates: bool,
    /// First-party Claude API token prices in USD per million tokens.
    pub(crate) cost_rates: TokenCostRates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

impl Family {
    /// The family's name as it appears in model ids.
    pub(crate) const fn id_token(self) -> &'static str {
        match self {
            Self::Fable => "fable",
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Haiku => "haiku",
        }
    }
}

const CONTEXT_WINDOW: u32 = 1_000_000;
/// Half the 128K output ceiling every catalog model shares. `effort` steers thinking depth, so
/// this only bounds runaway output.
const MAX_TOKENS: u32 = 64_000;
/// Fits the lower output ceilings of the older or smaller models an unknown id may name.
const UNKNOWN_MODEL_MAX_TOKENS: u32 = 16_000;

// ── MODELS ──

/// Listing order is the `/model` picker order.
pub(crate) const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-fable-5-1",
        display_name: "Claude Fable 5.1",
        family: Family::Fable,
        cutoff: "June 2026",
        default_effort: Effort::High,
        progress_updates: true,
        cost_rates: FABLE_5_1_RATES,
    },
    ModelInfo {
        id: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        family: Family::Opus,
        cutoff: "June 2026",
        default_effort: Effort::Medium,
        progress_updates: true,
        cost_rates: OPUS_5_5_RATES,
    },
    ModelInfo {
        id: "claude-sonnet-5-5",
        display_name: "Claude Sonnet 5.5",
        family: Family::Sonnet,
        cutoff: "June 2026",
        default_effort: Effort::High,
        progress_updates: true,
        cost_rates: SONNET_5_5_RATES,
    },
    ModelInfo {
        id: "claude-haiku-5-5",
        display_name: "Claude Haiku 5.5",
        family: Family::Haiku,
        cutoff: "June 2026",
        default_effort: Effort::Medium,
        progress_updates: false,
        cost_rates: HAIKU_5_5_RATES,
    },
];

fn lookup(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|info| info.id == id)
}

/// The catalog row to suggest for an unknown `id` that carries a `[1m]` suffix or starts with
/// `claude-`: the row the id names once the suffix is dropped, else the row of the family it names.
pub(crate) fn replacement_for(id: &str) -> Option<&'static ModelInfo> {
    let id = id.to_ascii_lowercase();
    let base = match id.strip_suffix("[1m]") {
        Some(base) => base,
        None if id.starts_with("claude-") => &id,
        None => return None,
    };
    lookup(base).or_else(|| {
        MODELS
            .iter()
            .find(|info| base.split('-').any(|token| token == info.family.id_token()))
    })
}

/// The catalog rates for `id`, or `None` for an id outside the catalog.
pub(crate) fn cost_rates(id: &str) -> Option<TokenCostRates> {
    lookup(id).map(|info| info.cost_rates)
}

// ── ModelSpec ──

/// A model id resolved against [`MODELS`] once. Every model-derived request field (effort,
/// thinking, betas, context management) and every label reads from here, so a swap re-derives
/// them all together.
#[derive(Debug, Clone)]
pub(crate) struct ModelSpec {
    id: String,
    info: Option<&'static ModelInfo>,
}

impl ModelSpec {
    pub(crate) fn resolve(id: String) -> Self {
        let info = lookup(&id);
        Self { id, info }
    }

    /// The id sent verbatim as the request's `model`.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// `None` for ids outside the catalog.
    pub(crate) fn info(&self) -> Option<&'static ModelInfo> {
        self.info
    }

    /// Effort to send: the user's `pick`, else the model's default. `None` for unknown ids, whose
    /// upstream may reject `output_config.effort`.
    pub(crate) fn resolve_effort(&self, pick: Option<Effort>) -> Option<Effort> {
        self.info.map(|info| pick.unwrap_or(info.default_effort))
    }

    /// Output cap to send: the user's `pick`, else 64K for catalog models and 16K for unknown
    /// ids, whose models may cap output lower.
    pub(crate) fn resolve_max_tokens(&self, pick: Option<u32>) -> u32 {
        pick.unwrap_or(self.info.map_or(UNKNOWN_MODEL_MAX_TOKENS, |_| MAX_TOKENS))
    }

    /// Adaptive thinking for known models, dropping replayed blocks whose conversation prefix
    /// changed. `show_thinking` opts into summarized thinking text, otherwise models with progress
    /// updates return only those. `None` for unknown ids, whose upstream may reject the field.
    pub(crate) fn thinking(&self, show_thinking: bool) -> Option<ThinkingConfig> {
        self.info.map(|info| ThinkingConfig::Adaptive {
            display: if show_thinking {
                Some(ThinkingDisplay::Summarized)
            } else {
                info.progress_updates.then_some(ThinkingDisplay::Updates)
            },
            block_binding: BlockBinding::DropBlock,
        })
    }

    /// `None` for unknown ids, which disables auto-compaction.
    pub(crate) fn context_window(&self) -> Option<u32> {
        self.info.map(|_| CONTEXT_WINDOW)
    }

    /// Human-facing label. Falls back to the raw id when the model is unknown.
    pub(crate) fn display_name(&self) -> &str {
        self.info.map_or(&self.id, |info| info.display_name)
    }

    /// Width-constrained variant for the status bar. Drops the `Claude ` family prefix. Falls
    /// back to the raw id when the model is unknown.
    pub(crate) fn short_display_name(&self) -> &str {
        self.info.map_or(&self.id, |info| {
            info.display_name
                .strip_prefix("Claude ")
                .unwrap_or(info.display_name)
        })
    }

    /// Warning for an id outside the catalog, naming what its requests go without and the
    /// [`replacement_for`] suggestion. `None` for catalog models.
    pub(crate) fn unsupported_notice(&self) -> Option<String> {
        if self.info.is_some() {
            return None;
        }
        let mut notice = format!(
            "`{}` is not a supported model, \
             so requests go without thinking, effort, and context management, \
             and auto-compaction and cost tracking stay off",
            self.id,
        );
        if let Some(info) = replacement_for(&self.id) {
            _ = write!(
                notice,
                ". Did you mean `{}` ({})?",
                info.id, info.display_name
            );
        }
        Some(notice)
    }
}

impl PartialEq for ModelSpec {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ModelSpec {}

impl Serialize for ModelSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.id)
    }
}

impl<'de> Deserialize<'de> for ModelSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::resolve)
    }
}

// ── ThinkingConfig ──

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ThinkingConfig {
    /// Model decides the thinking budget.
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
        block_binding: BlockBinding,
    },
}

/// `thinking.display` values. Omitting the field returns no thinking text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThinkingDisplay {
    Summarized,
    /// Only the progress notes written between tool calls come back as text. Requires the
    /// `thinking-display-updates-2026-08-18` beta.
    Updates,
}

/// `thinking.block_binding`. `DropBlock` makes the API drop a replayed thinking block whose
/// conversation prefix changed since it was produced, plus every later one, where it would
/// otherwise reject the request. Requires the `thinking-binding-controls-2026-08-01` beta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "prefix_mismatch_behavior", rename_all = "snake_case")]
pub(crate) enum BlockBinding {
    DropBlock,
}

// ── ResolvedModelId ──

/// A model id a frontend picked, such as the `/model` resolver's match. The engine accepts any id,
/// since [`ModelSpec::resolve`] keeps an unknown one without catalog info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ResolvedModelId(String);

impl ResolvedModelId {
    pub(crate) fn new(id: String) -> Self {
        Self(id)
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptCacheTtl;

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::resolve(id.to_owned())
    }

    // ── replacement_for ──

    #[test]
    fn replacement_for_maps_1m_suffix_and_family_claude_ids_to_catalog_rows() {
        for (id, expected) in [
            ("claude-opus-5-5[1m]", "claude-opus-5-5"),
            ("opus[1m]", "claude-opus-5-5"),
            ("CLAUDE-SONNET-5-5[1M]", "claude-sonnet-5-5"),
            ("claude-haiku-5-5-20261001", "claude-haiku-5-5"),
            ("claude-fable-latest", "claude-fable-5-1"),
        ] {
            assert_eq!(
                replacement_for(id).map(|info| info.id),
                Some(expected),
                "{id}"
            );
        }
    }

    #[test]
    fn replacement_for_leaves_gateway_and_familyless_ids_without_a_suggestion() {
        for id in [
            "gpt-5",
            "my-opus-gateway",
            "anthropic.claude-opus-5-5",
            "claude-future-9",
        ] {
            assert!(replacement_for(id).is_none(), "{id}");
        }
    }

    // ── cost_rates ──

    #[test]
    fn cost_rates_route_each_model_to_its_own_card() {
        // 50K input + 50K cache reads + 1M output stays within Haiku's base card: Fable
        // $0.50 + $0.0125 + $50, Opus $0.20 + $0.01 + $20, Sonnet $0.10 + $0.005 + $10, and Haiku
        // $0.005 + $0.0005 + $0.50.
        let usage = TokenUsage {
            cache_read_input_tokens: 50_000,
            ..TokenUsage::new(50_000, 1_000_000)
        };

        for (id, expected) in [
            ("claude-fable-5-1", 50.5125),
            ("claude-opus-5-5", 20.21),
            ("claude-sonnet-5-5", 10.105),
            ("claude-haiku-5-5", 0.5055),
        ] {
            let cost = cost_rates(id)
                .unwrap()
                .estimate_usd(usage, PromptCacheTtl::FiveMin);

            assert!((cost - expected).abs() < 1e-9, "{id}: {cost} vs {expected}");
        }
    }

    #[test]
    fn cost_rates_unknown_model_is_absent() {
        assert!(cost_rates("claude-future-9").is_none());
    }

    // ── ModelSpec::resolve ──

    #[test]
    fn resolve_finds_every_row_by_exact_id() {
        for info in MODELS {
            let resolved = spec(info.id).info().map(|row| row.id);
            assert_eq!(resolved, Some(info.id));
        }
    }

    #[test]
    fn resolve_keeps_near_miss_ids_unknown() {
        for unknown in [
            "claude-opus",
            "claude-opus-5-5[1m]",
            "claude-opus-5-5-20261001",
            "anthropic.claude-opus-5-5",
            "gpt-5",
        ] {
            let model = spec(unknown);
            assert!(model.info().is_none(), "{unknown} must not resolve");
            assert_eq!(model.id(), unknown);
        }
    }

    // ── ModelSpec::resolve_effort ──

    #[test]
    fn resolve_effort_falls_back_to_per_model_default() {
        for (id, expected) in [
            ("claude-fable-5-1", Effort::High),
            ("claude-opus-5-5", Effort::Medium),
            ("claude-sonnet-5-5", Effort::High),
            ("claude-haiku-5-5", Effort::Medium),
        ] {
            assert_eq!(spec(id).resolve_effort(None), Some(expected), "{id}");
        }
    }

    #[test]
    fn resolve_effort_passes_every_pick_through_on_supported_models() {
        for info in MODELS {
            for level in Effort::ALL {
                assert_eq!(
                    spec(info.id).resolve_effort(Some(level)),
                    Some(level),
                    "{} {level}",
                    info.id,
                );
            }
        }
    }

    #[test]
    fn resolve_effort_is_none_for_unknown_model() {
        let model = spec("custom-model");
        assert_eq!(model.resolve_effort(None), None);
        assert_eq!(model.resolve_effort(Some(Effort::High)), None);
    }

    // ── ModelSpec::resolve_max_tokens ──

    #[test]
    fn resolve_max_tokens_prefers_pick_then_catalog_default() {
        for (id, default) in [
            ("claude-fable-5-1", 64_000),
            ("claude-opus-5-5", 64_000),
            ("claude-sonnet-5-5", 64_000),
            ("claude-haiku-5-5", 64_000),
            ("custom-model", 16_000),
        ] {
            let model = spec(id);
            assert_eq!(model.resolve_max_tokens(None), default, "{id}");
            assert_eq!(model.resolve_max_tokens(Some(4_096)), 4_096, "{id}");
        }
    }

    // ── ModelSpec::thinking ──

    #[test]
    fn thinking_known_model_is_adaptive_with_display_following_show_thinking() {
        let drop_block = serde_json::json!({"prefix_mismatch_behavior": "drop_block"});
        for (id, hidden_display) in [
            ("claude-fable-5-1", Some("updates")),
            ("claude-opus-5-5", Some("updates")),
            ("claude-sonnet-5-5", Some("updates")),
            ("claude-haiku-5-5", None),
        ] {
            let model = spec(id);
            assert_eq!(
                serde_json::to_value(model.thinking(true)).unwrap(),
                serde_json::json!({
                    "type": "adaptive",
                    "display": "summarized",
                    "block_binding": drop_block,
                }),
                "{id}",
            );
            let mut hidden = serde_json::json!({"type": "adaptive", "block_binding": drop_block});
            if let Some(display) = hidden_display {
                hidden["display"] = display.into();
            }
            assert_eq!(
                serde_json::to_value(model.thinking(false)).unwrap(),
                hidden,
                "{id}"
            );
        }
    }

    #[test]
    fn thinking_unknown_model_is_omitted() {
        assert!(spec("custom-model").thinking(true).is_none());
    }

    // ── ModelSpec::context_window ──

    #[test]
    fn context_window_supported_models_is_1m() {
        for info in MODELS {
            assert_eq!(
                spec(info.id).context_window(),
                Some(1_000_000),
                "{}",
                info.id
            );
        }
    }

    #[test]
    fn context_window_unknown_model_is_none() {
        assert_eq!(spec("claude-future-9").context_window(), None);
    }

    // ── ModelSpec::display_name ──

    #[test]
    fn display_name_known_id_renders_row_label() {
        for (id, expected) in [
            ("claude-fable-5-1", "Claude Fable 5.1"),
            ("claude-opus-5-5", "Claude Opus 5.5"),
            ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
            ("claude-haiku-5-5", "Claude Haiku 5.5"),
        ] {
            assert_eq!(spec(id).display_name(), expected, "{id}");
        }
    }

    #[test]
    fn display_name_unknown_id_falls_through_to_raw() {
        assert_eq!(spec("custom-model").display_name(), "custom-model");
    }

    // ── ModelSpec::short_display_name ──

    #[test]
    fn short_display_name_strips_claude_family_prefix() {
        for (id, expected) in [
            ("claude-fable-5-1", "Fable 5.1"),
            ("claude-opus-5-5", "Opus 5.5"),
            ("claude-sonnet-5-5", "Sonnet 5.5"),
            ("claude-haiku-5-5", "Haiku 5.5"),
        ] {
            assert_eq!(spec(id).short_display_name(), expected, "{id}");
        }
    }

    #[test]
    fn short_display_name_unknown_id_falls_through_to_raw() {
        assert_eq!(spec("custom-model").short_display_name(), "custom-model");
    }

    // ── ModelSpec::unsupported_notice ──

    #[test]
    fn unsupported_notice_names_lost_features_and_suggests_replacement() {
        assert_eq!(
            spec("claude-opus-5-5-20261001")
                .unsupported_notice()
                .as_deref(),
            Some(
                "`claude-opus-5-5-20261001` is not a supported model, \
                 so requests go without thinking, effort, and context management, \
                 and auto-compaction and cost tracking stay off. \
                 Did you mean `claude-opus-5-5` (Claude Opus 5.5)?"
            ),
        );
        assert_eq!(
            spec("gateway-model").unsupported_notice().as_deref(),
            Some(
                "`gateway-model` is not a supported model, \
                 so requests go without thinking, effort, and context management, \
                 and auto-compaction and cost tracking stay off"
            ),
        );
    }

    #[test]
    fn unsupported_notice_is_absent_for_catalog_models() {
        for info in MODELS {
            assert_eq!(spec(info.id).unsupported_notice(), None, "{}", info.id);
        }
    }

    // ── ThinkingConfig ──

    #[test]
    fn thinking_config_adaptive_serializes_display_and_block_binding() {
        let binding = serde_json::json!({"prefix_mismatch_behavior": "drop_block"});
        for (display, wire) in [
            (None, None),
            (Some(ThinkingDisplay::Summarized), Some("summarized")),
            (Some(ThinkingDisplay::Updates), Some("updates")),
        ] {
            let json = serde_json::to_value(&ThinkingConfig::Adaptive {
                display,
                block_binding: BlockBinding::DropBlock,
            })
            .unwrap();
            let mut expected = serde_json::json!({"type": "adaptive", "block_binding": binding});
            if let Some(wire) = wire {
                expected["display"] = wire.into();
            }
            assert_eq!(json, expected);
        }
    }
}
