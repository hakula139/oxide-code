//! Claude model catalog, capabilities, pricing, and request parameters.
//!
//! Catalog models support adaptive thinking, the full `output_config.effort` ladder,
//! `clear_thinking` context management, a 1M-token context window, and 128K output. Unknown IDs
//! pass through unchanged without these opt-ins or default and percentage-based auto-compaction.
//! Explicit token thresholds still apply.

mod pricing;
mod usage;

pub(crate) use self::usage::TokenUsage;

use std::fmt::Write as _;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};

use self::pricing::{
    FABLE_5_1_RATES, HAIKU_5_5_RATES, OPUS_5_5_RATES, SONNET_5_5_RATES, TokenCostRates,
};
use crate::util::variant;

// ── ModelInfo ──

/// One row in the [`MODELS`] catalog.
#[derive(Debug)]
pub struct ModelInfo {
    pub id: &'static str,
    pub display_name: &'static str,
    pub(crate) family: Family,
    pub(crate) cutoff: &'static str,
    /// The API's own default when a request omits `output_config.effort`. Sent explicitly so the UI
    /// shows the level the request actually runs at.
    pub default_effort: Effort,
    /// Whether the model accepts `thinking.display = "updates"`.
    pub(crate) progress_updates: bool,
    /// First-party Claude API token prices in USD per million tokens.
    cost_rates: TokenCostRates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

impl Family {
    /// The family's name as it appears in model IDs.
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
/// Half the 128K output ceiling every catalog model shares. `effort` steers thinking depth, so this
/// only bounds runaway output.
const MAX_TOKENS: u32 = 64_000;
/// Fits the lower output ceilings of the older or smaller models an unknown ID may name.
const UNKNOWN_MODEL_MAX_TOKENS: u32 = 16_000;

// ── MODELS ──

/// Listing order is the `/model` picker order.
pub const MODELS: &[ModelInfo] = &[
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
/// `claude-`: the row the ID names once the suffix is dropped, else the row of the family it names.
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

/// Returns whether `id` names a catalog model that writes no progress updates, so none of its
/// reasoning text is a progress note.
pub(crate) fn lacks_progress_updates(id: &str) -> bool {
    lookup(id).is_some_and(|info| !info.progress_updates)
}

/// The catalog rates for `id`, or `None` for an ID outside the catalog.
pub(crate) fn cost_rates(id: &str) -> Option<TokenCostRates> {
    lookup(id).map(|info| info.cost_rates)
}

// ── ModelSpec ──

/// A model ID resolved against [`MODELS`] once. Every model-derived request field (effort,
/// thinking, betas, context management) and every label reads from here, so a swap re-derives them
/// all together.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    id: String,
    info: Option<&'static ModelInfo>,
}

impl ModelSpec {
    pub fn resolve(id: impl Into<String>) -> Self {
        let id = id.into();
        let info = lookup(&id);
        Self { id, info }
    }

    /// The ID sent verbatim as the request's `model`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The catalog row, `None` for IDs outside the catalog.
    pub fn info(&self) -> Option<&'static ModelInfo> {
        self.info
    }

    /// Effort to send: the user's `pick`, else the model's default. `None` for unknown IDs, whose
    /// upstream may reject `output_config.effort`.
    pub fn resolve_effort(&self, pick: Option<Effort>) -> Option<Effort> {
        self.info.map(|info| pick.unwrap_or(info.default_effort))
    }

    /// Output cap to send: the user's `pick`, else 64K for catalog models and 16K for unknown IDs,
    /// whose models may cap output lower.
    pub fn resolve_max_tokens(&self, pick: Option<u32>) -> u32 {
        pick.unwrap_or(self.info.map_or(UNKNOWN_MODEL_MAX_TOKENS, |_| MAX_TOKENS))
    }

    /// The context window in tokens. `None` for unknown IDs disables default and percentage-based
    /// auto-compaction and the status-line percentage. Explicit token thresholds still apply.
    pub(crate) fn context_window(&self) -> Option<u32> {
        self.info.map(|_| CONTEXT_WINDOW)
    }

    /// The catalog name, or the raw ID for an unknown model.
    pub fn display_name(&self) -> &str {
        self.info.map_or(&self.id, |info| info.display_name)
    }

    /// The status bar label: the catalog name without its `Claude ` prefix, or the raw ID for an
    /// unknown model.
    pub fn short_display_name(&self) -> &str {
        self.info.map_or(&self.id, |info| {
            info.display_name
                .strip_prefix("Claude ")
                .unwrap_or(info.display_name)
        })
    }

    /// Warning for an ID outside the catalog, naming what its requests go without and the
    /// [`replacement_for`] suggestion. `None` for catalog models.
    pub fn unsupported_notice(&self) -> Option<String> {
        if self.info.is_some() {
            return None;
        }
        let mut notice = format!(
            "`{}` is not a supported model, \
             so requests go without thinking, effort, and context management, \
             and cost tracking stays off. \
             Auto-compaction requires an explicit token threshold.",
            self.id,
        );
        if let Some(info) = replacement_for(&self.id) {
            _ = write!(
                notice,
                " Did you mean `{}` ({})?",
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

// ── ResolvedModelId ──

/// A model ID a frontend picked, such as the `/model` resolver's match. The engine accepts any ID,
/// since [`ModelSpec::resolve`] keeps an unknown one without catalog info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResolvedModelId(String);

impl ResolvedModelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }
}

// ── Effort ──

/// Effort level sent as `output_config.effort`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Display,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "lowercase")]
#[strum(
    serialize_all = "lowercase",
    parse_err_ty = anyhow::Error,
    parse_err_fn = variant::unknown::<Self>
)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// One-line UX hint for the typed-arg popup.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Low => "Fastest, shallow reasoning",
            Self::Medium => "Balanced",
            Self::High => "Deep reasoning",
            Self::Xhigh => "Extended thinking",
            Self::Max => "Maximum thinking",
        }
    }
}

// ── PromptCacheTtl ──

/// Sent as `cache_control.ttl`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Display,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[strum(parse_err_ty = anyhow::Error, parse_err_fn = variant::unknown::<Self>)]
pub enum PromptCacheTtl {
    #[serde(rename = "5m")]
    #[strum(serialize = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    #[strum(serialize = "1h")]
    OneHour,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod replacement_for {
        use super::*;

        #[test]
        fn maps_1m_suffix_and_family_claude_ids_to_catalog_rows() {
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
        fn leaves_gateway_and_familyless_ids_without_a_suggestion() {
            for id in [
                "gpt-5",
                "my-opus-gateway",
                "anthropic.claude-opus-5-5",
                "claude-future-9",
            ] {
                assert!(replacement_for(id).is_none(), "{id}");
            }
        }
    }

    mod cost_rates {
        use super::*;

        #[test]
        fn route_each_model_to_its_own_card() {
            // 50K input + 50K cache reads + 1M output stays within Haiku's base card: Fable $0.50 +
            // $0.0125 + $50, Opus $0.20 + $0.01 + $20, Sonnet $0.10 + $0.005 + $10, and Haiku
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
        fn unknown_model_has_no_rates() {
            assert!(cost_rates("claude-future-9").is_none());
        }
    }

    mod resolve {
        use super::*;

        #[test]
        fn finds_every_row_by_exact_id() {
            for info in MODELS {
                let resolved = ModelSpec::resolve(info.id).info().map(|row| row.id);
                assert_eq!(resolved, Some(info.id));
            }
        }

        #[test]
        fn keeps_near_miss_ids_unknown() {
            for unknown in [
                "claude-opus",
                "claude-opus-5-5[1m]",
                "claude-opus-5-5-20261001",
                "anthropic.claude-opus-5-5",
                "gpt-5",
            ] {
                let model = ModelSpec::resolve(unknown);
                assert!(model.info().is_none(), "{unknown} must not resolve");
                assert_eq!(model.id(), unknown);
            }
        }
    }

    mod resolve_effort {
        use super::*;

        #[test]
        fn falls_back_to_per_model_default() {
            for (id, expected) in [
                ("claude-fable-5-1", Effort::High),
                ("claude-opus-5-5", Effort::Medium),
                ("claude-sonnet-5-5", Effort::High),
                ("claude-haiku-5-5", Effort::Medium),
            ] {
                assert_eq!(
                    ModelSpec::resolve(id).resolve_effort(None),
                    Some(expected),
                    "{id}"
                );
            }
        }

        #[test]
        fn passes_every_pick_through_on_supported_models() {
            for info in MODELS {
                for &level in Effort::VARIANTS {
                    assert_eq!(
                        ModelSpec::resolve(info.id).resolve_effort(Some(level)),
                        Some(level),
                        "{} {level}",
                        info.id,
                    );
                }
            }
        }

        #[test]
        fn omits_effort_for_an_unknown_model() {
            let model = ModelSpec::resolve("custom-model");
            assert_eq!(model.resolve_effort(None), None);
            assert_eq!(model.resolve_effort(Some(Effort::High)), None);
        }
    }

    mod resolve_max_tokens {
        use super::*;

        #[test]
        fn prefers_pick_then_catalog_default() {
            for (id, default) in [
                ("claude-fable-5-1", 64_000),
                ("claude-opus-5-5", 64_000),
                ("claude-sonnet-5-5", 64_000),
                ("claude-haiku-5-5", 64_000),
                ("custom-model", 16_000),
            ] {
                let model = ModelSpec::resolve(id);
                assert_eq!(model.resolve_max_tokens(None), default, "{id}");
                assert_eq!(model.resolve_max_tokens(Some(4_096)), 4_096, "{id}");
            }
        }
    }

    mod context_window {
        use super::*;

        #[test]
        fn supported_models_is_1m() {
            for info in MODELS {
                assert_eq!(
                    ModelSpec::resolve(info.id).context_window(),
                    Some(1_000_000),
                    "{}",
                    info.id
                );
            }
        }

        #[test]
        fn unknown_model_has_no_known_window() {
            assert_eq!(ModelSpec::resolve("claude-future-9").context_window(), None);
        }
    }

    mod display_name {
        use super::*;

        #[test]
        fn known_id_renders_row_label() {
            for (id, expected) in [
                ("claude-fable-5-1", "Claude Fable 5.1"),
                ("claude-opus-5-5", "Claude Opus 5.5"),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                ("claude-haiku-5-5", "Claude Haiku 5.5"),
            ] {
                assert_eq!(ModelSpec::resolve(id).display_name(), expected, "{id}");
            }
        }

        #[test]
        fn unknown_id_falls_through_to_raw() {
            let custom = ModelSpec::resolve("custom-model");

            assert_eq!(custom.display_name(), "custom-model");
        }
    }

    mod short_display_name {
        use super::*;

        #[test]
        fn strips_claude_family_prefix() {
            for (id, expected) in [
                ("claude-fable-5-1", "Fable 5.1"),
                ("claude-opus-5-5", "Opus 5.5"),
                ("claude-sonnet-5-5", "Sonnet 5.5"),
                ("claude-haiku-5-5", "Haiku 5.5"),
            ] {
                assert_eq!(
                    ModelSpec::resolve(id).short_display_name(),
                    expected,
                    "{id}"
                );
            }
        }

        #[test]
        fn unknown_id_falls_through_to_raw() {
            let custom = ModelSpec::resolve("custom-model");

            assert_eq!(custom.short_display_name(), "custom-model");
        }
    }

    mod unsupported_notice {
        use super::*;

        #[test]
        fn names_lost_features_and_suggests_replacement() {
            assert_eq!(
                ModelSpec::resolve("claude-opus-5-5-20261001")
                    .unsupported_notice()
                    .as_deref(),
                Some(
                    "`claude-opus-5-5-20261001` is not a supported model, \
                     so requests go without thinking, effort, and context management, \
                     and cost tracking stays off. \
                     Auto-compaction requires an explicit token threshold. \
                     Did you mean `claude-opus-5-5` (Claude Opus 5.5)?"
                ),
            );
            assert_eq!(
                ModelSpec::resolve("gateway-model")
                    .unsupported_notice()
                    .as_deref(),
                Some(
                    "`gateway-model` is not a supported model, \
                     so requests go without thinking, effort, and context management, \
                     and cost tracking stays off. \
                     Auto-compaction requires an explicit token threshold."
                ),
            );
        }

        #[test]
        fn is_absent_for_catalog_models() {
            for info in MODELS {
                assert_eq!(
                    ModelSpec::resolve(info.id).unsupported_notice(),
                    None,
                    "{}",
                    info.id
                );
            }
        }
    }

    mod effort {
        use super::*;

        #[test]
        fn round_trips_through_serde_and_fromstr() {
            for &variant in Effort::VARIANTS {
                let token = variant.to_string();
                assert_eq!(serde_json::to_value(variant).unwrap(), token);
                assert_eq!(token.parse::<Effort>().unwrap(), variant);
            }
        }

        #[test]
        fn round_trips_through_toml_deserialize() {
            #[derive(Deserialize)]
            struct Wrap {
                effort: Effort,
            }
            let wrap: Wrap = toml::from_str(r#"effort = "xhigh""#).unwrap();
            assert_eq!(wrap.effort, Effort::Xhigh);
        }

        mod from_str {
            use super::*;

            #[test]
            fn rejects_unknown_tokens_listing_the_accepted_ones() {
                let err = "extra-high".parse::<Effort>().expect_err("unknown token");
                assert_eq!(
                    format!("{err:#}"),
                    "expected one of low, medium, high, xhigh, max"
                );
            }
        }
    }

    mod prompt_cache_ttl {
        use super::*;

        #[test]
        fn round_trips_through_serde_and_fromstr() {
            for &variant in PromptCacheTtl::VARIANTS {
                let token = variant.to_string();
                assert_eq!(serde_json::to_value(variant).unwrap(), token);
                assert_eq!(token.parse::<PromptCacheTtl>().unwrap(), variant);
            }
        }

        mod from_str {
            use super::*;

            #[test]
            fn rejects_unknown_tokens_listing_the_accepted_ones() {
                let err = "30m".parse::<PromptCacheTtl>().expect_err("unknown token");
                assert_eq!(format!("{err:#}"), "expected one of 5m, 1h");
            }
        }
    }
}
