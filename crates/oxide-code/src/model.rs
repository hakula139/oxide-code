//! Ground-truth table of known Claude models. Substring-matched, most-specific entry first.

use std::borrow::Cow;

use crate::config::Effort;

// ── ModelInfo ──

/// One row in the [`MODELS`] catalogue. Pure data — no methods. Looked up by substring against a
/// caller-supplied model id (alias-resolved + `[1m]`-stripped); see [`lookup`].
pub(crate) struct ModelInfo {
    /// First substring match in [`MODELS`] wins; ordering matters.
    pub(crate) id_substr: &'static str,
    /// Unqualified fallback row that resolves dated / legacy ids; user-facing listings hide it.
    pub(crate) is_family_base: bool,
    pub(crate) marketing: &'static str,
    pub(crate) cutoff: Option<&'static str>,
    pub(crate) capabilities: Capabilities,
}

// ── Capabilities ──

/// Per-model gate set consumed by the wire-builder (header + body fields), the slash commands
/// (`/effort` rejects unsupported tiers, `/model` rejects `[1m]` on non-1M models), and the
/// effort picker (renders only the supported ladder). Every field is a Boolean because each one
/// maps 1:1 to a distinct upstream switch — combining them would obscure the source of truth.
#[expect(
    clippy::struct_excessive_bools,
    reason = "seven independent capability flags — each maps 1:1 to a separate upstream `modelSupports*` predicate or a per-version allowlist; a bitflag or state-machine refactor would add indirection without any expressiveness gain"
)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Capabilities {
    pub(crate) interleaved_thinking: bool,
    pub(crate) context_management: bool,
    /// `context-1m-2025-08-07` beta.
    pub(crate) context_1m: bool,
    /// `output_config.effort` at low / medium / high.
    pub(crate) effort: bool,
    /// `effort = "max"`. Opus-only.
    pub(crate) effort_max: bool,
    /// `effort = "xhigh"`. Opus 4.7 only.
    pub(crate) effort_xhigh: bool,
    /// `structured-outputs-2025-12-15` beta.
    pub(crate) structured_outputs: bool,
}

// ── MODELS ──

/// Most-specific substring first. No inheritance between rows.
pub(crate) const MODELS: &[ModelInfo] = &[
    // Inherits 4.6 as a monotonic projection; `effort_xhigh` is the 4.7-only addition.
    ModelInfo {
        id_substr: "claude-opus-4-7",
        is_family_base: false,
        marketing: "Claude Opus 4.7",
        cutoff: Some("January 2026"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            effort: true,
            effort_max: true,
            effort_xhigh: true,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-opus-4-6",
        is_family_base: false,
        marketing: "Claude Opus 4.6",
        cutoff: Some("May 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            effort: true,
            effort_max: true,
            effort_xhigh: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-6",
        is_family_base: false,
        marketing: "Claude Sonnet 4.6",
        cutoff: Some("August 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            effort: true,
            // `max` is Opus-only; Sonnet 4.6 400s on it.
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-opus-4-5",
        is_family_base: false,
        marketing: "Claude Opus 4.5",
        cutoff: Some("May 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: false,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-5",
        is_family_base: false,
        marketing: "Claude Sonnet 4.5",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-haiku-4-5",
        is_family_base: false,
        marketing: "Claude Haiku 4.5",
        cutoff: Some("February 2025"),
        capabilities: Capabilities {
            // 3P gateways 400 on `interleaved-thinking` for Haiku 4.5.
            interleaved_thinking: false,
            context_management: true,
            context_1m: false,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-opus-4-1",
        is_family_base: false,
        marketing: "Claude Opus 4.1",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: false,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: true,
        },
    },
    // Unqualified base (`claude-opus-4`, `-4-0`, `-4-20250514`); structured outputs arrived in 4.1.
    ModelInfo {
        id_substr: "claude-opus-4",
        is_family_base: true,
        marketing: "Claude Opus 4",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: false,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: false,
        },
    },
    // Sonnet 4 base: all Sonnet 4.x carry 1M per upstream's `sonnet-4` substring rule.
    ModelInfo {
        id_substr: "claude-sonnet-4",
        is_family_base: true,
        marketing: "Claude Sonnet 4",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            context_1m: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: false,
        },
    },
    ModelInfo {
        id_substr: "claude-haiku-4",
        is_family_base: true,
        marketing: "Claude Haiku 4",
        cutoff: Some("February 2025"),
        capabilities: Capabilities {
            interleaved_thinking: false,
            context_management: true,
            context_1m: false,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            structured_outputs: false,
        },
    },
];

impl Capabilities {
    /// Whether the model accepts a given `output_config.effort` value. Splits on tier because the
    /// Anthropic API rejects unsupported levels with a 400, not a silent clamp — `xhigh` is 4.7
    /// only, `max` is Opus-only.
    pub(crate) fn accepts_effort(self, level: Effort) -> bool {
        match level {
            Effort::Low | Effort::Medium | Effort::High => self.effort,
            Effort::Xhigh => self.effort_xhigh,
            Effort::Max => self.effort_max,
        }
    }

    /// Highest accepted level ≤ `pick`. `None` when the model doesn't support effort.
    pub(crate) fn clamp_effort(self, pick: Effort) -> Option<Effort> {
        if !self.effort {
            return None;
        }
        [
            Effort::Max,
            Effort::Xhigh,
            Effort::High,
            Effort::Medium,
            Effort::Low,
        ]
        .into_iter()
        .find(|&level| level <= pick && self.accepts_effort(level))
    }

    /// Highest tier the model accepts when the user hasn't picked one — `xhigh` for 4.7, `high`
    /// for other effort-capable models, `None` otherwise.
    pub(crate) fn default_effort(self) -> Option<Effort> {
        if self.effort_xhigh {
            Some(Effort::Xhigh)
        } else if self.effort {
            Some(Effort::High)
        } else {
            None
        }
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

/// True when `id` matches a family-base row exactly. User-facing listings filter against this.
pub(crate) fn is_family_base(id: &str) -> bool {
    MODELS
        .iter()
        .any(|info| info.id_substr == id && info.is_family_base)
}

/// Capabilities for `model`, falling back to [`Capabilities::default`] for unknown ids.
pub(crate) fn capabilities_for(model: &str) -> Capabilities {
    lookup(model)
        .map(|info| info.capabilities)
        .unwrap_or_default()
}

pub(crate) fn marketing_name(model: &str) -> Option<&'static str> {
    lookup(model).map(|info| info.marketing)
}

/// Marketing name when known, raw id otherwise.
pub(crate) fn marketing_or_id(model: &str) -> Cow<'_, str> {
    marketing_name(model).map_or_else(|| Cow::Borrowed(model), Cow::Borrowed)
}

/// Human-facing label: marketing name + ` (1M context)` suffix on `[1m]` ids.
pub(crate) fn display_name(model: &str) -> Cow<'_, str> {
    let base = marketing_or_id(model);
    if model.ends_with("[1m]") {
        Cow::Owned(format!("{base} (1M context)"))
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── capability rows ──

    #[test]
    fn capability_flags_match_upstream_substring_predicates() {
        // Locks substring-derived flags to upstream's `modelSupports*` predicates. Allowlist
        // flags have their own tests. Opus 4.7 postdates the predicate set and is skipped.
        for info in MODELS {
            if info.id_substr == "claude-opus-4-7" {
                continue;
            }
            let m = info.id_substr;
            let is_opus_or_sonnet_4 = m.contains("opus-4") || m.contains("sonnet-4");
            // haiku-4 is not in modelSupportsISP
            let expect_interleaved_thinking = is_opus_or_sonnet_4;
            let expect_context_management = is_opus_or_sonnet_4 || m.contains("haiku-4");
            let expect_context_1m = m.contains("claude-sonnet-4") || m.contains("opus-4-6");
            let expect_effort = m.contains("opus-4-6") || m.contains("sonnet-4-6");

            assert_eq!(
                info.capabilities.interleaved_thinking, expect_interleaved_thinking,
                "{m}: interleaved_thinking should match modelSupportsISP",
            );
            assert_eq!(
                info.capabilities.context_management, expect_context_management,
                "{m}: context_management should match modelSupportsContextManagement",
            );
            assert_eq!(
                info.capabilities.context_1m, expect_context_1m,
                "{m}: context_1m should match modelSupports1M",
            );
            assert_eq!(
                info.capabilities.effort, expect_effort,
                "{m}: effort should match modelSupportsEffort",
            );
        }
    }

    #[test]
    fn opus_4_7_uniquely_supports_xhigh() {
        // Upstream predates 4.7; pin so a future "alignment" edit doesn't strip our caps.
        let caps = lookup("claude-opus-4-7").unwrap().capabilities;
        assert!(caps.interleaved_thinking);
        assert!(caps.context_management);
        assert!(caps.context_1m);
        assert!(caps.effort);
        assert!(caps.effort_max);
        assert!(caps.effort_xhigh);
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
                !lookup(other).unwrap().capabilities.effort_xhigh,
                "{other} must not claim effort_xhigh — it 400s on non-4.7",
            );
        }
    }

    #[test]
    fn effort_max_is_opus_only() {
        // `max` is Opus-only; Sonnet 4.6 supports base `effort` but 400s on `max`.
        for supported in ["claude-opus-4-7", "claude-opus-4-6"] {
            assert!(
                lookup(supported).unwrap().capabilities.effort_max,
                "{supported} should claim effort_max",
            );
        }
        for unsupported in [
            "claude-sonnet-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-1",
            "claude-opus-4",
            "claude-sonnet-4",
            "claude-haiku-4",
        ] {
            assert!(
                !lookup(unsupported).unwrap().capabilities.effort_max,
                "{unsupported} must not claim effort_max",
            );
        }
    }

    #[test]
    fn structured_outputs_flag_tracks_upstream_allowlist() {
        // Per-version allowlist: Opus 4.1/4.5/4.6 (+ our 4.7 bump), Sonnet 4.5/4.6, Haiku 4.5.
        for supported in [
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-opus-4-1",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            assert!(
                lookup(supported).unwrap().capabilities.structured_outputs,
                "{supported} should claim structured outputs per upstream",
            );
        }
        for unsupported in ["claude-opus-4", "claude-sonnet-4", "claude-haiku-4"] {
            assert!(
                !lookup(unsupported).unwrap().capabilities.structured_outputs,
                "{unsupported} fallback row must not claim structured outputs",
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
    fn lookup_matches_most_specific_row_before_family_base() {
        let info = lookup("claude-opus-4-6").unwrap();
        assert_eq!(info.marketing, "Claude Opus 4.6");
        assert!(info.capabilities.effort);
    }

    #[test]
    fn lookup_ignores_1m_suffix_tag_for_matching() {
        // `[1m]` is a client-side opt-in marker; substring match still finds the base row.
        let info = lookup("claude-opus-4-6[1m]").unwrap();
        assert_eq!(info.marketing, "Claude Opus 4.6");
    }

    #[test]
    fn lookup_unknown_model_family_is_absent() {
        assert!(lookup("claude-opus-5-0").is_none());
        assert!(lookup("gpt-4").is_none());
    }

    // ── is_family_base ──

    #[test]
    fn is_family_base_recognizes_each_unqualified_base_row() {
        // Pinning per-id flips a mutation that flipped the bool only on `claude-opus-4` while
        // leaving sonnet / haiku correct — the slash-resolver filter would still reject opus
        // but silently accept sonnet/haiku family-bases.
        for base in ["claude-opus-4", "claude-sonnet-4", "claude-haiku-4"] {
            assert!(is_family_base(base), "{base} must flag as family-base");
        }
    }

    #[test]
    fn is_family_base_rejects_dated_versioned_and_unknown_ids() {
        for non_base in [
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "claude-opus-4-20250514",
            "gpt-4",
            "",
        ] {
            assert!(
                !is_family_base(non_base),
                "{non_base} must not flag as family-base",
            );
        }
    }

    // ── marketing_name ──

    #[test]
    fn marketing_name_known_models() {
        assert_eq!(marketing_name("claude-opus-4-7"), Some("Claude Opus 4.7"));
        assert_eq!(marketing_name("claude-opus-4-6"), Some("Claude Opus 4.6"));
        assert_eq!(
            marketing_name("claude-sonnet-4-6"),
            Some("Claude Sonnet 4.6")
        );
        assert_eq!(marketing_name("claude-opus-4-5"), Some("Claude Opus 4.5"));
        assert_eq!(
            marketing_name("claude-sonnet-4-5"),
            Some("Claude Sonnet 4.5")
        );
        assert_eq!(marketing_name("claude-haiku-4-5"), Some("Claude Haiku 4.5"));
        assert_eq!(marketing_name("claude-opus-4-1"), Some("Claude Opus 4.1"));
        assert_eq!(marketing_name("claude-opus-4"), Some("Claude Opus 4"));
        assert_eq!(marketing_name("claude-sonnet-4"), Some("Claude Sonnet 4"));
        assert_eq!(marketing_name("claude-haiku-4"), Some("Claude Haiku 4"));
    }

    #[test]
    fn marketing_name_with_dated_suffix_falls_through_to_family_row() {
        assert_eq!(
            marketing_name("claude-opus-4-6-20260401"),
            Some("Claude Opus 4.6")
        );
    }

    #[test]
    fn marketing_name_unknown_model_is_absent() {
        assert_eq!(marketing_name("gpt-4o"), None);
        assert_eq!(marketing_name("custom-model"), None);
    }

    // ── marketing_or_id ──

    #[test]
    fn marketing_or_id_produces_marketing_for_known_id() {
        assert_eq!(marketing_or_id("claude-opus-4-7"), "Claude Opus 4.7");
    }

    #[test]
    fn marketing_or_id_falls_back_to_raw_id_for_unknown() {
        // Single seam for unknown-id fallback — every UI surface goes through this.
        assert_eq!(marketing_or_id("gpt-4"), "gpt-4");
    }

    // ── display_name ──

    #[test]
    fn display_name_appends_1m_context_suffix_on_1m_id() {
        assert_eq!(
            display_name("claude-opus-4-7[1m]"),
            "Claude Opus 4.7 (1M context)"
        );
    }

    #[test]
    fn display_name_omits_suffix_on_plain_id() {
        assert_eq!(display_name("claude-opus-4-7"), "Claude Opus 4.7");
    }

    #[test]
    fn display_name_unknown_plain_id_falls_through_to_raw() {
        assert_eq!(display_name("gpt-4"), "gpt-4");
    }
}
