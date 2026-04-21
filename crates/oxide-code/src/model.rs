//! Ground-truth table of known Claude models.
//!
//! Centralizes everything that needs to branch on the target model:
//! marketing name, knowledge cutoff, and API capabilities (interleaved
//! thinking, context management, effort control, 1M context). One table,
//! one lookup, every caller reads the same booleans — so the one place we
//! edit when a new model ships is here.
//!
//! Matching is substring-based on `id_substr` with more-specific entries
//! first, so `claude-opus-4-6` wins over `claude-opus-4`. An unknown model
//! string falls through to the family base row (e.g. `claude-opus-4`),
//! which carries conservative capability flags — we'd rather under-send
//! an experimental beta than 400 a request.

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
/// headers. Each flag corresponds to a `modelSupports*` check in the
/// upstream reference: `interleaved_thinking` → `modelSupportsISP`,
/// `context_management` → `modelSupportsContextManagement`, `effort` →
/// `modelSupportsEffort`, `context_1m` → `modelSupports1M`,
/// `structured_outputs` → `modelSupportsStructuredOutputs`.
///
/// `context_1m` does not currently drive beta sending — that signal is
/// the user-opt-in `[1m]` tag on the model string. The flag is kept for
/// UI paths (a future `/model` picker) that want to hide the 1M variant
/// on models that can't honor it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "five independent capability flags — each maps 1:1 to a \
              separate upstream `modelSupports*` predicate; a bitflag or \
              state-machine refactor would add indirection without any \
              expressiveness gain"
)]
#[derive(Copy, Clone, Default)]
pub(crate) struct Capabilities {
    pub(crate) interleaved_thinking: bool,
    pub(crate) context_management: bool,
    pub(crate) effort: bool,
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

/// All capabilities enabled. Shorthand for the top-tier rows.
const ALL_CAPS: Capabilities = Capabilities {
    interleaved_thinking: true,
    context_management: true,
    effort: true,
    context_1m: true,
    structured_outputs: true,
};

/// Opus-4 family baseline (4.1 / 4.5): thinking + context management +
/// structured outputs; no effort, no 1M. Distinct from the Opus 4 base
/// row because upstream's structured-outputs allowlist is per-version
/// and a future `claude-opus-4-0` variant landing after the snapshot
/// should stay in the safer, narrower fallback below.
const OPUS_4_BASELINE: Capabilities = Capabilities {
    effort: false,
    context_1m: false,
    ..ALL_CAPS
};

/// Opus 4 unqualified base: like [`OPUS_4_BASELINE`] minus structured
/// outputs. Used only by the substring fallback row for model IDs that
/// don't match a specific Opus 4.x entry.
const OPUS_4_BASE_CAPS: Capabilities = Capabilities {
    structured_outputs: false,
    ..OPUS_4_BASELINE
};

/// Sonnet-4 family baseline (4.5): Opus 4 baseline + 1M (Sonnet 4.x
/// all carry 1M per upstream, opt-in via `[1m]`).
const SONNET_4_BASELINE: Capabilities = Capabilities {
    context_1m: true,
    ..OPUS_4_BASELINE
};

/// Sonnet 4 unqualified base: [`SONNET_4_BASELINE`] minus structured.
const SONNET_4_BASE_CAPS: Capabilities = Capabilities {
    structured_outputs: false,
    ..SONNET_4_BASELINE
};

/// Haiku-4 family baseline (4.5): context management + structured
/// outputs only — no thinking (3P gateways 400), no 1M, no effort.
const HAIKU_4_BASELINE: Capabilities = Capabilities {
    interleaved_thinking: false,
    effort: false,
    context_1m: false,
    ..ALL_CAPS
};

/// Haiku 4 unqualified base: [`HAIKU_4_BASELINE`] minus structured.
const HAIKU_4_BASE_CAPS: Capabilities = Capabilities {
    structured_outputs: false,
    ..HAIKU_4_BASELINE
};

/// Ordered table of known Claude models. More-specific prefixes come
/// before their family stems so lookup's first-match rule routes
/// `claude-opus-4-6` to the 4.6 row, not the `claude-opus-4` base.
///
/// Capability flags follow the upstream `modelSupports*` predicates. The
/// one intentional extension: Opus 4.7 is treated as 4.6-equivalent
/// rather than falling through to the Opus-4 base, reflecting the
/// released-as-a-minor-bump shape of the model (monotonic capabilities).
pub(crate) const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id_substr: "claude-opus-4-7",
        marketing: "Claude Opus 4.7",
        cutoff: Some("January 2026"),
        capabilities: ALL_CAPS,
    },
    ModelInfo {
        id_substr: "claude-opus-4-6",
        marketing: "Claude Opus 4.6",
        cutoff: Some("May 2025"),
        capabilities: ALL_CAPS,
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-6",
        marketing: "Claude Sonnet 4.6",
        cutoff: Some("August 2025"),
        capabilities: ALL_CAPS,
    },
    ModelInfo {
        id_substr: "claude-opus-4-5",
        marketing: "Claude Opus 4.5",
        cutoff: Some("May 2025"),
        capabilities: OPUS_4_BASELINE,
    },
    ModelInfo {
        id_substr: "claude-sonnet-4-5",
        marketing: "Claude Sonnet 4.5",
        cutoff: Some("January 2025"),
        capabilities: SONNET_4_BASELINE,
    },
    ModelInfo {
        id_substr: "claude-haiku-4-5",
        marketing: "Claude Haiku 4.5",
        cutoff: Some("February 2025"),
        capabilities: HAIKU_4_BASELINE,
    },
    ModelInfo {
        id_substr: "claude-opus-4-1",
        marketing: "Claude Opus 4.1",
        cutoff: Some("January 2025"),
        capabilities: OPUS_4_BASELINE,
    },
    ModelInfo {
        id_substr: "claude-opus-4",
        marketing: "Claude Opus 4",
        cutoff: Some("January 2025"),
        capabilities: OPUS_4_BASE_CAPS,
    },
    ModelInfo {
        id_substr: "claude-sonnet-4",
        marketing: "Claude Sonnet 4",
        cutoff: Some("January 2025"),
        capabilities: SONNET_4_BASE_CAPS,
    },
    ModelInfo {
        id_substr: "claude-haiku-4",
        marketing: "Claude Haiku 4",
        cutoff: Some("February 2025"),
        capabilities: HAIKU_4_BASE_CAPS,
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
    fn haiku_4_family_omits_interleaved_thinking() {
        // Haiku never accepts interleaved_thinking on 3P gateways;
        // sending it produces HTTP 400. Pin both haiku-4-5 and the
        // haiku-4 base row.
        assert!(
            !lookup("claude-haiku-4-5")
                .unwrap()
                .capabilities
                .interleaved_thinking
        );
        assert!(
            !lookup("claude-haiku-4-999")
                .unwrap()
                .capabilities
                .interleaved_thinking
        );
    }

    #[test]
    fn opus_4_5_has_thinking_and_cm_but_not_effort() {
        // Opus 4.5 predates effort support — regression guard for the
        // earlier `is_opus_ge_46` heuristic which wrongly included 4.5.
        let caps = lookup("claude-opus-4-5").unwrap().capabilities;
        assert!(caps.interleaved_thinking);
        assert!(caps.context_management);
        assert!(!caps.effort);
        assert!(!caps.context_1m);
    }

    #[test]
    fn opus_4_7_inherits_4_6_caps() {
        // Opus 4.7 is a monotonic bump of 4.6 — same capabilities
        // including effort and 1M.
        let caps = lookup("claude-opus-4-7").unwrap().capabilities;
        assert!(caps.effort);
        assert!(caps.context_1m);
    }

    #[test]
    fn sonnet_4_base_supports_1m_per_upstream_pattern() {
        // The upstream `modelSupports1M` uses `includes('claude-sonnet-4')`
        // — so even the Sonnet 4 base row supports 1M (via [1m] tag).
        let caps = lookup("claude-sonnet-4").unwrap().capabilities;
        assert!(caps.context_1m);
    }

    #[test]
    fn opus_4_base_does_not_support_1m() {
        // Opus's 1M support is only in 4.6 (and our forward-looking
        // 4.7); the base row must not claim it.
        let caps = lookup("claude-opus-4").unwrap().capabilities;
        assert!(!caps.context_1m);
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
