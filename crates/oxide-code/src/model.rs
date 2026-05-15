//! Ground-truth table of known Claude models. Substring-matched, most-specific entry first.

mod pricing;

use std::borrow::Cow;

use crate::config::Effort;

pub(crate) use pricing::TokenCostRates;
use pricing::{HAIKU_RATES, OPUS_4_1_RATES, OPUS_4_5_PLUS_RATES, SONNET_RATES};

// ── ModelInfo ──

/// One row in the [`MODELS`] catalogue. Pure data — no methods. Looked up by substring against a
/// caller-supplied model id (alias-resolved + `[1m]`-stripped); see [`lookup`].
pub(crate) struct ModelInfo {
    /// First substring match in [`MODELS`] wins; ordering matters.
    pub(crate) id_substr: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) cutoff: Option<&'static str>,
    pub(crate) capabilities: Capabilities,
    /// First-party Claude API token prices in USD per million tokens.
    pub(crate) cost_rates: Option<TokenCostRates>,
}

const STANDARD_CONTEXT_WINDOW: u32 = 200_000;
const CONTEXT_1M_WINDOW: u32 = 1_000_000;

// ── Capabilities ──

/// Per-model gate set consumed by the wire-builder (header + body fields), the slash commands
/// (`/effort` rejects unsupported tiers, `/model` rejects `[1m]` on non-1M models), and the
/// effort picker (renders only the supported ladder).
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent capability gate"
)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Capabilities {
    pub(crate) interleaved_thinking: bool,
    pub(crate) context_management: bool,
    /// `context-1m-2025-08-07` beta.
    pub(crate) context_1m: bool,
    /// `output_config.effort` levels accepted upstream. Empty when the model rejects `effort`.
    pub(crate) supported_efforts: &'static [Effort],
    /// `structured-outputs-2025-12-15` beta.
    pub(crate) structured_outputs: bool,
}

// ── MODELS ──

/// Most-specific substring first.
pub(crate) const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id_substr: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        cutoff: Some("January 2026"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            supported_efforts: &[
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
                Effort::Max,
            ],
            structured_outputs: true,
        },
        cost_rates: Some(OPUS_4_5_PLUS_RATES),
    },
    ModelInfo {
        id_substr: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        cutoff: Some("May 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            supported_efforts: &[Effort::Low, Effort::Medium, Effort::High, Effort::Max],
            structured_outputs: true,
        },
        cost_rates: Some(OPUS_4_5_PLUS_RATES),
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        cutoff: Some("August 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            supported_efforts: &[Effort::Low, Effort::Medium, Effort::High],
            structured_outputs: true,
        },
        cost_rates: Some(SONNET_RATES),
    },
    ModelInfo {
        id_substr: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        cutoff: Some("May 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: false,
            supported_efforts: &[],
            structured_outputs: true,
        },
        cost_rates: Some(OPUS_4_5_PLUS_RATES),
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            supported_efforts: &[],
            structured_outputs: true,
        },
        cost_rates: Some(SONNET_RATES),
    },
    ModelInfo {
        id_substr: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        cutoff: Some("February 2025"),
        capabilities: Capabilities {
            // 3P gateways 400 on `interleaved-thinking` for Haiku 4.5.
            interleaved_thinking: false,
            context_management: true,
            context_1m: false,
            supported_efforts: &[],
            structured_outputs: true,
        },
        cost_rates: Some(HAIKU_RATES),
    },
    ModelInfo {
        id_substr: "claude-opus-4-1",
        display_name: "Claude Opus 4.1",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: false,
            supported_efforts: &[],
            structured_outputs: true,
        },
        cost_rates: Some(OPUS_4_1_RATES),
    },
];

impl Capabilities {
    /// Whether `output_config.effort` is sent at all for this model.
    pub(crate) fn has_effort(self) -> bool {
        !self.supported_efforts.is_empty()
    }

    /// Whether the model accepts `level`. Anthropic 400s on unsupported tiers — no silent clamp.
    pub(crate) fn accepts_effort(self, level: Effort) -> bool {
        self.supported_efforts.contains(&level)
    }

    /// Highest accepted level ≤ `pick`. `None` when the model rejects effort entirely.
    pub(crate) fn clamp_effort(self, pick: Effort) -> Option<Effort> {
        self.supported_efforts
            .iter()
            .copied()
            .rev()
            .find(|&level| level <= pick)
    }

    /// Default tier when the user hasn't picked one. `Max` is opt-in, so the implicit ceiling is
    /// the highest non-`Max` supported level.
    pub(crate) fn default_effort(self) -> Option<Effort> {
        self.supported_efforts
            .iter()
            .copied()
            .rev()
            .find(|&level| level != Effort::Max)
    }

    /// Clamps `pick` when present, otherwise falls back to [`Self::default_effort`].
    pub(crate) fn resolve_effort(self, pick: Option<Effort>) -> Option<Effort> {
        match pick {
            Some(p) => self.clamp_effort(p),
            None => self.default_effort(),
        }
    }
}

// ── ResolvedModelId ──

/// A model id that has passed through the `/model` resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedModelId(String);

impl ResolvedModelId {
    pub(crate) fn new(id: String) -> Self {
        Self(id)
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }

    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

// ── Lookup ──

/// First-match substring lookup against [`MODELS`].
pub(crate) fn lookup(model: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|info| model.contains(info.id_substr))
}

/// Capabilities for `model`, falling back to [`Capabilities::default`] for unknown ids.
pub(crate) fn capabilities_for(model: &str) -> Capabilities {
    lookup(model)
        .map(|info| info.capabilities)
        .unwrap_or_default()
}

/// Effective context window for known Claude models. `[1m]` opts into the 1M beta only on
/// models that advertise that capability; unknown raw ids stay disabled for auto-compaction.
pub(crate) fn context_window_for(model: &str) -> Option<u32> {
    let info = lookup(model)?;
    if model.ends_with("[1m]") && info.capabilities.context_1m {
        Some(CONTEXT_1M_WINDOW)
    } else {
        Some(STANDARD_CONTEXT_WINDOW)
    }
}

pub(crate) fn token_cost_rates_for(model: &str) -> Option<TokenCostRates> {
    lookup(model).and_then(|info| info.cost_rates)
}

/// Human-facing label: the row's [`ModelInfo::display_name`] plus a ` (1M context)` suffix on
/// `[1m]` ids. Falls back to the raw id when the model is unknown.
pub(crate) fn display_name(model: &str) -> Cow<'_, str> {
    let base = lookup(model).map_or(Cow::Borrowed(model), |info| {
        Cow::Borrowed(info.display_name)
    });
    if model.ends_with("[1m]") {
        Cow::Owned(format!("{base} (1M context)"))
    } else {
        base
    }
}

/// Width-constrained variant for the status bar. Drops the `Claude ` family prefix and
/// abbreviates the 1M opt-in to ` [1M]`. Falls back to the raw id when the model is unknown.
pub(crate) fn short_display_name(model: &str) -> Cow<'_, str> {
    let Some(info) = lookup(model) else {
        return Cow::Borrowed(model);
    };
    let base = info
        .display_name
        .strip_prefix("Claude ")
        .unwrap_or(info.display_name);
    if model.ends_with("[1m]") {
        Cow::Owned(format!("{base} [1M]"))
    } else {
        Cow::Borrowed(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptCacheTtl;

    // ── capability rows ──

    #[test]
    fn capability_flags_match_upstream_substring_predicates() {
        // Locks substring-derived flags to upstream's `modelSupports*` predicates. Opus 4.7
        // postdates the predicate set and is skipped.
        for info in MODELS {
            if info.id_substr == "claude-opus-4-7" {
                continue;
            }
            let m = info.id_substr;
            let is_opus_or_sonnet_4 = m.contains("opus-4") || m.contains("sonnet-4");
            let expect_interleaved_thinking = is_opus_or_sonnet_4;
            let expect_context_management = is_opus_or_sonnet_4 || m.contains("haiku-4");
            let expect_context_1m = m.contains("claude-sonnet-4") || m.contains("opus-4-6");
            let expect_effort = m.contains("opus-4-6") || m.contains("sonnet-4-6");

            assert_eq!(
                info.capabilities.interleaved_thinking, expect_interleaved_thinking,
                "{m}"
            );
            assert_eq!(
                info.capabilities.context_management, expect_context_management,
                "{m}"
            );
            assert_eq!(info.capabilities.context_1m, expect_context_1m, "{m}");
            assert_eq!(info.capabilities.has_effort(), expect_effort, "{m}");
        }
    }

    #[test]
    fn opus_4_7_uniquely_supports_xhigh() {
        // Upstream predates 4.7; pin so a future "alignment" edit doesn't strip our caps.
        let caps = lookup("claude-opus-4-7").unwrap().capabilities;
        assert!(caps.interleaved_thinking);
        assert!(caps.context_management);
        assert!(caps.context_1m);
        assert!(caps.accepts_effort(Effort::Xhigh));
        assert!(caps.accepts_effort(Effort::Max));
        assert!(caps.structured_outputs);

        for other in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-1",
        ] {
            assert!(
                !lookup(other)
                    .unwrap()
                    .capabilities
                    .accepts_effort(Effort::Xhigh),
                "{other} must not accept Xhigh — it 400s on non-4.7",
            );
        }
    }

    #[test]
    fn effort_max_is_opus_only() {
        for supported in ["claude-opus-4-7", "claude-opus-4-6"] {
            assert!(
                lookup(supported)
                    .unwrap()
                    .capabilities
                    .accepts_effort(Effort::Max),
                "{supported}",
            );
        }
        for unsupported in [
            "claude-sonnet-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            assert!(
                !lookup(unsupported)
                    .unwrap()
                    .capabilities
                    .accepts_effort(Effort::Max),
                "{unsupported}",
            );
        }
    }

    #[test]
    fn supported_efforts_is_ascending_for_every_models_row() {
        // `clamp_effort` and `default_effort` walk the slice in reverse to find the highest
        // tier ≤ pick. They depend on the slice being authored in ascending order.
        for info in MODELS {
            let efforts = info.capabilities.supported_efforts;
            assert!(
                efforts.windows(2).all(|w| w[0] < w[1]),
                "{}: supported_efforts must be ascending: {efforts:?}",
                info.id_substr,
            );
        }
    }

    #[test]
    fn structured_outputs_flag_tracks_upstream_allowlist() {
        for supported in [
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            assert!(
                lookup(supported).unwrap().capabilities.structured_outputs,
                "{supported}"
            );
        }
    }

    // ── Capabilities::accepts_effort ──

    #[test]
    fn accepts_effort_matches_per_tier_capability_flag() {
        let opus_4_7 = lookup("claude-opus-4-7").unwrap().capabilities;
        let opus_4_6 = lookup("claude-opus-4-6").unwrap().capabilities;
        let sonnet_4_6 = lookup("claude-sonnet-4-6").unwrap().capabilities;
        let sonnet_4_5 = lookup("claude-sonnet-4-5").unwrap().capabilities;

        // Opus 4.7 — full ladder.
        assert!(opus_4_7.accepts_effort(Effort::Low));
        assert!(opus_4_7.accepts_effort(Effort::High));
        assert!(opus_4_7.accepts_effort(Effort::Xhigh));
        assert!(opus_4_7.accepts_effort(Effort::Max));

        // Opus 4.6 — base + max but no xhigh.
        assert!(opus_4_6.accepts_effort(Effort::High));
        assert!(opus_4_6.accepts_effort(Effort::Max));
        assert!(!opus_4_6.accepts_effort(Effort::Xhigh));

        // Sonnet 4.6 — base only, no max / xhigh.
        assert!(sonnet_4_6.accepts_effort(Effort::High));
        assert!(!sonnet_4_6.accepts_effort(Effort::Max));
        assert!(!sonnet_4_6.accepts_effort(Effort::Xhigh));

        // Sonnet 4.5 — no effort at all.
        assert!(!sonnet_4_5.accepts_effort(Effort::Low));
        assert!(!sonnet_4_5.accepts_effort(Effort::Max));
    }

    // ── Capabilities::clamp_effort ──

    #[test]
    fn clamp_effort_picks_highest_supported_at_or_below_user_pick() {
        let opus_4_7 = lookup("claude-opus-4-7").unwrap().capabilities;
        assert_eq!(opus_4_7.clamp_effort(Effort::Max), Some(Effort::Max));
        assert_eq!(opus_4_7.clamp_effort(Effort::Xhigh), Some(Effort::Xhigh));
        assert_eq!(opus_4_7.clamp_effort(Effort::Low), Some(Effort::Low));

        // Opus 4.6: Max ✓, Xhigh ✗ — `xhigh` clamps down to `high`, never up to `max`.
        let opus_4_6 = lookup("claude-opus-4-6").unwrap().capabilities;
        assert_eq!(opus_4_6.clamp_effort(Effort::Max), Some(Effort::Max));
        assert_eq!(opus_4_6.clamp_effort(Effort::Xhigh), Some(Effort::High));
        assert_eq!(opus_4_6.clamp_effort(Effort::High), Some(Effort::High));

        // Sonnet 4.6: Max ✗, Xhigh ✗ — both clamp to `high`.
        let sonnet_4_6 = lookup("claude-sonnet-4-6").unwrap().capabilities;
        assert_eq!(sonnet_4_6.clamp_effort(Effort::Max), Some(Effort::High));
        assert_eq!(sonnet_4_6.clamp_effort(Effort::Xhigh), Some(Effort::High));
        assert_eq!(
            sonnet_4_6.clamp_effort(Effort::Medium),
            Some(Effort::Medium)
        );

        // No `effort` at all → None regardless of pick.
        let haiku_4_5 = lookup("claude-haiku-4-5").unwrap().capabilities;
        assert_eq!(haiku_4_5.clamp_effort(Effort::Max), None);
        assert_eq!(haiku_4_5.clamp_effort(Effort::Low), None);
    }

    // ── Capabilities::default_effort ──

    #[test]
    fn default_effort_picks_highest_supported_tier_when_user_has_no_pick() {
        // Opus 4.7: full ladder → xhigh.
        let opus_4_7 = lookup("claude-opus-4-7").unwrap().capabilities;
        assert_eq!(opus_4_7.default_effort(), Some(Effort::Xhigh));

        // Opus 4.6 / Sonnet 4.6: effort but no xhigh → high.
        for id in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            let caps = lookup(id).unwrap().capabilities;
            assert_eq!(caps.default_effort(), Some(Effort::High), "{id}");
        }

        // No effort tier at all → None.
        let haiku_4_5 = lookup("claude-haiku-4-5").unwrap().capabilities;
        assert_eq!(haiku_4_5.default_effort(), None);
    }

    // ── Capabilities::resolve_effort ──

    #[test]
    fn resolve_effort_passes_pick_through_when_model_accepts_it() {
        let opus_4_7 = lookup("claude-opus-4-7").unwrap().capabilities;
        assert_eq!(
            opus_4_7.resolve_effort(Some(Effort::Xhigh)),
            Some(Effort::Xhigh)
        );
    }

    #[test]
    fn resolve_effort_clamps_pick_against_model_ceiling() {
        // Sonnet 4.6 caps at `high`.
        let sonnet_4_6 = lookup("claude-sonnet-4-6").unwrap().capabilities;
        assert_eq!(
            sonnet_4_6.resolve_effort(Some(Effort::Xhigh)),
            Some(Effort::High)
        );
    }

    #[test]
    fn resolve_effort_falls_back_to_model_default_when_pick_is_none() {
        let opus_4_7 = lookup("claude-opus-4-7").unwrap().capabilities;
        assert_eq!(opus_4_7.resolve_effort(None), Some(Effort::Xhigh));
        let sonnet_4_6 = lookup("claude-sonnet-4-6").unwrap().capabilities;
        assert_eq!(sonnet_4_6.resolve_effort(None), Some(Effort::High));
    }

    #[test]
    fn resolve_effort_is_none_on_no_tier_model() {
        // Haiku 4.5 rejects the effort field; both pick=None and pick=Some collapse to None.
        let haiku_4_5 = lookup("claude-haiku-4-5").unwrap().capabilities;
        assert_eq!(haiku_4_5.resolve_effort(None), None);
        assert_eq!(haiku_4_5.resolve_effort(Some(Effort::High)), None);
    }

    // ── ResolvedModelId ──

    #[test]
    fn resolved_model_id_into_inner_returns_wrapped_string() {
        let id = ResolvedModelId::new("claude-opus-4-7".to_owned());
        assert_eq!(id.into_inner(), "claude-opus-4-7");
    }

    // ── lookup ──

    #[test]
    fn lookup_picks_first_matching_substring_row() {
        let info = lookup("claude-opus-4-6").unwrap();
        assert_eq!(info.display_name, "Claude Opus 4.6");
        assert!(info.capabilities.has_effort());
    }

    #[test]
    fn lookup_ignores_1m_suffix_tag_for_matching() {
        // `[1m]` is a client-side opt-in marker; substring match still finds the base row.
        let info = lookup("claude-opus-4-6[1m]").unwrap();
        assert_eq!(info.display_name, "Claude Opus 4.6");
    }

    #[test]
    fn lookup_unknown_or_retired_model_family_is_absent() {
        for unknown in [
            "claude-opus-5-0",
            "claude-opus-4",
            "claude-sonnet-4",
            "claude-haiku-4",
            "claude-opus-4-20250514",
            "gpt-4",
        ] {
            assert!(lookup(unknown).is_none(), "{unknown} must not resolve");
        }
    }

    // ── context_window_for ──

    #[test]
    fn context_window_for_known_models_defaults_to_standard_window() {
        assert_eq!(context_window_for("claude-opus-4-7"), Some(200_000));
        assert_eq!(context_window_for("claude-haiku-4-5"), Some(200_000));
    }

    #[test]
    fn context_window_for_1m_suffix_requires_model_capability() {
        assert_eq!(context_window_for("claude-opus-4-7[1m]"), Some(1_000_000));
        assert_eq!(context_window_for("claude-haiku-4-5[1m]"), Some(200_000));
    }

    #[test]
    fn context_window_for_unknown_model_is_none() {
        assert_eq!(context_window_for("claude-future-9"), None);
    }

    // ── token_cost_rates_for ──

    #[test]
    fn token_cost_rates_for_known_model_estimates_prompt_cache_ttl() {
        let rates = token_cost_rates_for("claude-opus-4-7[1m]").unwrap();
        let five_min = rates.estimate_usd(
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
            PromptCacheTtl::FiveMin,
        );
        let one_hour = rates.estimate_usd(
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
            PromptCacheTtl::OneHour,
        );

        assert!((five_min - 36.75).abs() < 1e-9);
        assert!((one_hour - 40.5).abs() < 1e-9);
    }

    #[test]
    fn token_cost_rates_for_opus_4_1_uses_older_pricing() {
        let rates = token_cost_rates_for("claude-opus-4-1").unwrap();
        let cost = rates.estimate_usd(
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
            PromptCacheTtl::FiveMin,
        );

        assert!((cost - 110.25).abs() < 1e-9);
    }

    #[test]
    fn token_cost_rates_for_unknown_model_is_absent() {
        assert!(token_cost_rates_for("claude-future-9").is_none());
    }

    // ── display_name ──

    #[test]
    fn display_name_known_plain_id_renders_row_label() {
        for (id, expected) in [
            ("claude-opus-4-7", "Claude Opus 4.7"),
            ("claude-opus-4-6", "Claude Opus 4.6"),
            ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ("claude-opus-4-5", "Claude Opus 4.5"),
            ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
            ("claude-haiku-4-5", "Claude Haiku 4.5"),
            ("claude-opus-4-1", "Claude Opus 4.1"),
        ] {
            assert_eq!(display_name(id), expected, "{id}");
        }
    }

    #[test]
    fn display_name_dated_suffix_falls_through_to_substring_row() {
        assert_eq!(display_name("claude-opus-4-6-20260401"), "Claude Opus 4.6");
    }

    #[test]
    fn display_name_appends_1m_context_suffix_on_1m_id() {
        assert_eq!(
            display_name("claude-opus-4-7[1m]"),
            "Claude Opus 4.7 (1M context)"
        );
    }

    #[test]
    fn display_name_unknown_id_falls_through_to_raw() {
        assert_eq!(display_name("gpt-4"), "gpt-4");
        assert_eq!(display_name("custom-model"), "custom-model");
    }

    // ── short_display_name ──

    #[test]
    fn short_display_name_strips_claude_family_prefix() {
        for (id, expected) in [
            ("claude-opus-4-7", "Opus 4.7"),
            ("claude-sonnet-4-6", "Sonnet 4.6"),
            ("claude-haiku-4-5", "Haiku 4.5"),
            ("claude-opus-4-1", "Opus 4.1"),
        ] {
            assert_eq!(short_display_name(id), expected, "{id}");
        }
    }

    #[test]
    fn short_display_name_replaces_1m_context_with_compact_tag() {
        assert_eq!(short_display_name("claude-opus-4-7[1m]"), "Opus 4.7 [1M]");
    }

    #[test]
    fn short_display_name_unknown_id_falls_through_to_raw() {
        assert_eq!(short_display_name("gpt-4"), "gpt-4");
    }
}
