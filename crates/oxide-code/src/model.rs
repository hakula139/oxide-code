//! Ground-truth table of known Claude models.
//!
//! Centralizes everything that needs to branch on the target model:
//! marketing name, knowledge cutoff, and API capabilities (interleaved
//! thinking, context management, effort control, 1M context, structured
//! outputs). One table, one lookup, every caller reads the same booleans —
//! so the one place we edit when a new model ships is here.
//!
//! Matching is substring-based on `id_substr` with more-specific entries
//! first, so `claude-opus-4-6` wins over `claude-opus-4`. An unknown model
//! string falls through to the family base row (e.g. `claude-opus-4`),
//! which carries conservative capability flags — we'd rather under-send
//! an experimental beta than 400 a request.
//!
//! Capability flags mirror the third-party-gateway branch of the upstream
//! `modelSupports*` predicates (substring rules) and a few client-side
//! additions that come from the migration guide + live packet captures
//! (per-version allowlists):
//!
//! - `interleaved_thinking` ← `modelSupportsISP` — substring `opus-4` or
//!   `sonnet-4`.
//! - `context_management` ← `modelSupportsContextManagement` — substring
//!   `opus-4`, `sonnet-4`, or `haiku-4`.
//! - `effort` ← `modelSupportsEffort` — substring `opus-4-6` or
//!   `sonnet-4-6`. Gate for the `output_config.effort` body field at
//!   `low` / `medium` / `high` levels.
//! - `effort_max` — explicit allowlist: Opus 4.6, Opus 4.7. The `max`
//!   effort level is Opus-only; Sonnet 4.6 rejects it.
//! - `effort_xhigh` — explicit allowlist: Opus 4.7. Added by the 4.7
//!   release; older models 400 on `xhigh`. Callers should clamp an
//!   out-of-range pick down to the nearest supported level rather than
//!   ship an unsupported one.
//! - `context_1m` ← `modelSupports1M` — substring `claude-sonnet-4` or
//!   `opus-4-6`.
//! - `structured_outputs` ← `modelSupportsStructuredOutputs` — explicit
//!   allowlist: opus-4-1 / 4-5 / 4-6, sonnet-4-5 / 4-6, haiku-4-5.
//!
//! `capability_flags_match_upstream_predicates` in the test module locks
//! every row to the substring predicates above so a mis-bump fails CI
//! loudly. Flags that are allowlist-shaped (`effort_max`, `effort_xhigh`,
//! `structured_outputs`) are exercised by per-flag enumeration tests
//! because they don't reduce to a substring rule.

/// Metadata and capability flags for a single Claude model.
pub(crate) struct ModelInfo {
    /// Substring that identifies this model. The first substring match in
    /// [`MODELS`] wins, so entries are ordered most-specific first.
    pub(crate) id_substr: &'static str,
    /// User-visible product name for the TUI / prompt / session list.
    pub(crate) marketing: &'static str,
    /// Knowledge cutoff date for the `<env>` block. `None` when not known.
    pub(crate) cutoff: Option<&'static str>,
    pub(crate) capabilities: Capabilities,
}

/// Per-model feature flags consulted by the API client to gate beta
/// headers and body fields. `interleaved_thinking`, `context_management`,
/// `effort`, `context_1m`, and `structured_outputs` mirror upstream
/// `modelSupports*` predicates; `effort_max` and `effort_xhigh` are
/// client-side allowlists derived from the migration guide and live
/// packet captures.
///
/// `context_1m` does not currently drive beta sending — that signal is
/// the user-opt-in `[1m]` tag on the model string. The flag is kept for
/// UI paths (a future `/model` picker) that want to hide the 1M variant
/// on models that can't honor it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "seven independent capability flags — each maps 1:1 to a \
              separate upstream `modelSupports*` predicate or a \
              per-version allowlist; a bitflag or state-machine refactor \
              would add indirection without any expressiveness gain"
)]
#[derive(Copy, Clone, Default)]
pub(crate) struct Capabilities {
    pub(crate) interleaved_thinking: bool,
    pub(crate) context_management: bool,
    /// Whether `output_config.effort` is accepted at all. Gates the
    /// `low` / `medium` / `high` levels. The model-specific upper
    /// bound is further refined by `effort_max` / `effort_xhigh`.
    pub(crate) effort: bool,
    /// Whether `output_config.effort = "max"` is accepted. Opus-only
    /// per the migration guide; Sonnet 4.6 400s on it. Implies
    /// [`Self::effort`].
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed by Config::load once the Effort clamp lands"
        )
    )]
    pub(crate) effort_max: bool,
    /// Whether `output_config.effort = "xhigh"` is accepted. Introduced
    /// by Opus 4.7; older models 400 on it. Implies [`Self::effort`].
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed by Config::load once the Effort clamp lands"
        )
    )]
    pub(crate) effort_xhigh: bool,
    /// Whether the model accepts the `context-1m-2025-08-07` beta.
    /// `compute_betas` gates the beta on `has_1m_tag(model) AND
    /// context_1m` so a user who tags `claude-haiku-4[1m]` doesn't
    /// silently send an unsupported beta and 400.
    pub(crate) context_1m: bool,
    /// Whether the model accepts the `structured-outputs-2025-12-15`
    /// beta (JSON-schema-constrained text output). The upstream
    /// allowlist is Opus 4.1/4.5/4.6, Sonnet 4.5/4.6, Haiku 4.5;
    /// everything else silently falls back to free-form text, which
    /// [`Client::complete`][crate::client::anthropic::Client::complete]
    /// mirrors by dropping the `output_config` body field together with
    /// the beta header rather than 400ing on the gateway.
    pub(crate) structured_outputs: bool,
}

/// Ordered table of known Claude models. More-specific prefixes come
/// before their family stems so lookup's first-match rule routes
/// `claude-opus-4-6` to the 4.6 row, not the `claude-opus-4` base.
///
/// Capability flags are spelled out per row with no inheritance —
/// upstream's `modelSupports*` predicates are independent per flag and
/// the allowlist / substring shape varies by predicate (see the
/// module-level doc), so every row is the canonical reference for its
/// own model. Bumping for a new model is a single-row edit: copy the
/// nearest sibling and flip the flags that the upstream predicate(s)
/// change.
///
/// The one intentional divergence from the substring-predicate rules:
/// Opus 4.7 postdates the upstream snapshot we have on hand, so it
/// inherits 4.6's monotonic-capability projection for `effort`,
/// `context_management`, and `1M`. 4.7 uniquely adds `effort_xhigh`;
/// `effort_max` is Opus-only per the migration guide (4.6 + 4.7).
pub(crate) const MODELS: &[ModelInfo] = &[
    // Upstream predates 4.7; substring-derived flags inherit 4.6 as a
    // monotonic projection, and `effort_xhigh` is the one 4.7-only
    // addition (rejected as 400 by every other model).
    ModelInfo {
        id_substr: "claude-opus-4-7",
        marketing: "Claude Opus 4.7",
        cutoff: Some("January 2026"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: true,
            effort_max: true,
            effort_xhigh: true,
            context_1m: true,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-opus-4-6",
        marketing: "Claude Opus 4.6",
        cutoff: Some("May 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: true,
            effort_max: true,
            effort_xhigh: false,
            context_1m: true,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-6",
        marketing: "Claude Sonnet 4.6",
        cutoff: Some("August 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: true,
            // `max` is Opus-only per the migration guide; Sonnet 4.6
            // 400s on it.
            effort_max: false,
            effort_xhigh: false,
            context_1m: true,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-opus-4-5",
        marketing: "Claude Opus 4.5",
        cutoff: Some("May 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-5",
        marketing: "Claude Sonnet 4.5",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: true,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-haiku-4-5",
        marketing: "Claude Haiku 4.5",
        cutoff: Some("February 2025"),
        capabilities: Capabilities {
            // Haiku 4.5 doesn't match the `opus-4 || sonnet-4`
            // substring rule that gates `interleaved-thinking`, and 3P
            // gateways 400 on it. First-party would accept, but we
            // target 3P throughout.
            interleaved_thinking: false,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: false,
            structured_outputs: true,
        },
    },
    ModelInfo {
        id_substr: "claude-opus-4-1",
        marketing: "Claude Opus 4.1",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: false,
            structured_outputs: true,
        },
    },
    // Unqualified base (`claude-opus-4`, `claude-opus-4-0`,
    // `claude-opus-4-20250514`). Structured outputs arrived with 4.1
    // per upstream's explicit allowlist, so the base row must not
    // claim them.
    ModelInfo {
        id_substr: "claude-opus-4",
        marketing: "Claude Opus 4",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: false,
            structured_outputs: false,
        },
    },
    // Sonnet 4 base: all Sonnet 4.x carry 1M per upstream's
    // `sonnet-4` substring rule, so `context_1m` stays on here even
    // though structured outputs don't.
    ModelInfo {
        id_substr: "claude-sonnet-4",
        marketing: "Claude Sonnet 4",
        cutoff: Some("January 2025"),
        capabilities: Capabilities {
            interleaved_thinking: true,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: true,
            structured_outputs: false,
        },
    },
    ModelInfo {
        id_substr: "claude-haiku-4",
        marketing: "Claude Haiku 4",
        cutoff: Some("February 2025"),
        capabilities: Capabilities {
            interleaved_thinking: false,
            context_management: true,
            effort: false,
            effort_max: false,
            effort_xhigh: false,
            context_1m: false,
            structured_outputs: false,
        },
    },
];

/// First-match substring lookup against [`MODELS`]. Returns `None` for
/// model strings that don't contain any known family stem (e.g. a future
/// `claude-opus-5` before the table is bumped); callers decide whether
/// to fall back to empty capabilities or reject the request.
pub(crate) fn lookup(model: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|info| model.contains(info.id_substr))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── lookup ──

    #[test]
    fn lookup_matches_most_specific_row_before_family_base() {
        // `claude-opus-4-6` must hit the 4.6 row, not fall through to
        // the `claude-opus-4` base.
        let info = lookup("claude-opus-4-6").unwrap();
        assert_eq!(info.marketing, "Claude Opus 4.6");
        assert!(info.capabilities.effort);
    }

    #[test]
    fn lookup_returns_none_for_unknown_model_family() {
        // A hypothetical future family with no entry should miss entirely
        // so callers can opt into conservative defaults.
        assert!(lookup("claude-opus-5-0").is_none());
        assert!(lookup("gpt-4").is_none());
    }

    #[test]
    fn lookup_ignores_1m_suffix_tag_for_matching() {
        // `[1m]` is a client-side opt-in marker; the substring match
        // still finds the base model row.
        let info = lookup("claude-opus-4-6[1m]").unwrap();
        assert_eq!(info.marketing, "Claude Opus 4.6");
    }

    // ── capability rows ──

    #[test]
    fn capability_flags_match_upstream_substring_predicates() {
        // Lock every row's substring-derived capability flags to the
        // `modelSupports*`-style rules the third-party gateway expects.
        // A mis-bump or typo that lets the predicates below drift from
        // the `MODELS` table will fail here instead of silently
        // 400-ing one model family on a release day.
        //
        // Allowlist-shaped flags (`effort_max`, `effort_xhigh`,
        // `structured_outputs`) don't reduce to a substring rule, so
        // they're covered by per-flag enumeration tests below.
        //
        // Opus 4.7 postdates the predicate set we mirror, so we skip
        // it here — there is no substring rule to check against.
        for info in MODELS {
            if info.id_substr == "claude-opus-4-7" {
                continue;
            }
            let m = info.id_substr;
            let is_opus_or_sonnet_4 = m.contains("opus-4") || m.contains("sonnet-4");
            let expect_interleaved_thinking = is_opus_or_sonnet_4; // haiku-4 is not in modelSupportsISP
            let expect_context_management = is_opus_or_sonnet_4 || m.contains("haiku-4");
            let expect_effort = m.contains("opus-4-6") || m.contains("sonnet-4-6");
            let expect_context_1m = m.contains("claude-sonnet-4") || m.contains("opus-4-6");

            assert_eq!(
                info.capabilities.interleaved_thinking, expect_interleaved_thinking,
                "{m}: interleaved_thinking should match modelSupportsISP",
            );
            assert_eq!(
                info.capabilities.context_management, expect_context_management,
                "{m}: context_management should match modelSupportsContextManagement",
            );
            assert_eq!(
                info.capabilities.effort, expect_effort,
                "{m}: effort should match modelSupportsEffort",
            );
            assert_eq!(
                info.capabilities.context_1m, expect_context_1m,
                "{m}: context_1m should match modelSupports1M",
            );
        }
    }

    #[test]
    fn opus_4_7_uniquely_supports_xhigh() {
        // Upstream predates 4.7 so its predicates wouldn't claim
        // `effort` or `1M` on this id_substr. We override to the
        // monotonic-bump projection. Pin it so a well-meaning future
        // edit that "aligns 4.7 with the predicates" doesn't
        // accidentally strip the caps we rely on. `effort_xhigh` is
        // the one 4.7-only addition — every other row must reject it.
        let caps = lookup("claude-opus-4-7").unwrap().capabilities;
        assert!(caps.interleaved_thinking);
        assert!(caps.context_management);
        assert!(caps.effort);
        assert!(caps.effort_max);
        assert!(caps.effort_xhigh);
        assert!(caps.context_1m);
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
        // `max` effort is Opus-only per the migration guide. Sonnet
        // 4.6 supports base `effort` but 400s on `max`; Haiku doesn't
        // support `effort` at all.
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
        // Upstream `modelSupportsStructuredOutputs` is a per-version
        // allowlist: Opus 4.1/4.5/4.6 (+ our 4.7 monotonic bump),
        // Sonnet 4.5/4.6, Haiku 4.5. Everything else is out.
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
}
