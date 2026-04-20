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
/// `modelSupportsEffort`, `context_1m` → `modelSupports1M`.
///
/// `context_1m` does not currently drive beta sending — that signal is
/// the user-opt-in `[1m]` tag on the model string. The flag is kept for
/// UI paths (a future `/model` picker) that want to hide the 1M variant
/// on models that can't honor it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent capability flags — each maps 1:1 to a \
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
}

/// All capabilities enabled. Shorthand for the top-tier rows.
const ALL_CAPS: Capabilities = Capabilities {
    interleaved_thinking: true,
    context_management: true,
    effort: true,
    context_1m: true,
};

/// Opus-4 family baseline: thinking + context management, no 1M / effort.
const OPUS_4_BASELINE: Capabilities = Capabilities {
    interleaved_thinking: true,
    context_management: true,
    effort: false,
    context_1m: false,
};

/// Sonnet-4 family baseline: like Opus-4, plus 1M (all Sonnet 4.x support
/// the 1M-context beta per the upstream reference).
const SONNET_4_BASELINE: Capabilities = Capabilities {
    interleaved_thinking: true,
    context_management: true,
    effort: false,
    context_1m: true,
};

/// Haiku-4 family baseline: context management only — Haiku rejects
/// interleaved thinking on 3P gateways, never supports 1M, and has no
/// effort control.
const HAIKU_4_BASELINE: Capabilities = Capabilities {
    interleaved_thinking: false,
    context_management: true,
    effort: false,
    context_1m: false,
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
        capabilities: OPUS_4_BASELINE,
    },
    ModelInfo {
        id_substr: "claude-sonnet-4",
        marketing: "Claude Sonnet 4",
        cutoff: Some("January 2025"),
        capabilities: SONNET_4_BASELINE,
    },
    ModelInfo {
        id_substr: "claude-haiku-4",
        marketing: "Claude Haiku 4",
        cutoff: Some("February 2025"),
        capabilities: HAIKU_4_BASELINE,
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
}
