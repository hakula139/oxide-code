//! Per-request `anthropic-beta` header computation.
//!
//! Every beta ships independent of the base URL — the
//! `prompt-caching-scope-2026-01-05` header in particular goes out
//! unconditionally because 3P re-distribution proxies fingerprint its
//! absence as "not from claude-code". The body-side
//! `cache_control.scope: "global"` field is the only 1P-only knob, and
//! it lives on [`is_first_party_base_url`] / [`static_prefix_cache_control`].
//! The header without the field is a server-side no-op but keeps the
//! canonical wire fingerprint intact for the verifier.

use crate::config::{Auth, PromptCacheTtl};

use super::wire::CacheControl;

pub(super) const CLAUDE_CODE_BETA_HEADER: &str = "claude-code-20250219";
const CONTEXT_1M_BETA_HEADER: &str = "context-1m-2025-08-07";
const CONTEXT_MANAGEMENT_BETA_HEADER: &str = "context-management-2025-06-27";
const EFFORT_BETA_HEADER: &str = "effort-2025-11-24";
const INTERLEAVED_THINKING_BETA_HEADER: &str = "interleaved-thinking-2025-05-14";
pub(super) const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const PROMPT_CACHING_SCOPE_BETA_HEADER: &str = "prompt-caching-scope-2026-01-05";
pub(super) const STRUCTURED_OUTPUTS_BETA_HEADER: &str = "structured-outputs-2025-12-15";

/// Computes the `anthropic-beta` header value for a request. Each beta
/// is gated on a [`Capabilities`][crate::model::Capabilities] flag, so
/// adding or bumping a model only means editing the lookup table.
///
/// `is_agentic` gates agent-only betas on the streaming chat path
/// (keeps one-shot calls like title generation minimal).
/// `want_structured` is cross-checked against the model's capability
/// flag so an unsupported [`crate::client::anthropic::wire::OutputFormat`]
/// silently drops back to free-form text instead of 400ing the
/// gateway.
pub(super) fn compute_betas(
    model: &str,
    auth: &Auth,
    is_agentic: bool,
    want_structured: bool,
) -> Vec<&'static str> {
    let caps = crate::model::capabilities_for(model);
    let is_haiku = model
        .split('-')
        .any(|tok| tok.eq_ignore_ascii_case("haiku"));

    // Order mirrors `docs/research/anthropic-api.md` → Per-model beta
    // sets: identity / auth → universal agentic → capability-gated.
    let mut out = Vec::with_capacity(8);

    // Gateway tag: required for non-Haiku OAuth on 1P (429 without it).
    // Non-agentic Haiku one-shots skip it; agentic Haiku re-adds it.
    if !is_haiku || is_agentic {
        out.push(CLAUDE_CODE_BETA_HEADER);
    }
    if matches!(auth, Auth::OAuth(_)) {
        out.push(OAUTH_BETA_HEADER);
    }

    // Order matches claude-code 2.1.121 wire captures: interleaved-thinking
    // → context-management → prompt-caching-scope → effort. 3P proxies
    // fingerprint this exact ordering, so even commutative reordering
    // can flip the verifier from accept to reject.
    if is_agentic {
        if caps.interleaved_thinking {
            out.push(INTERLEAVED_THINKING_BETA_HEADER);
        }
        if caps.context_management {
            out.push(CONTEXT_MANAGEMENT_BETA_HEADER);
        }
        // Prompt-caching scope is the beta that enables `scope: "global"`
        // on `cache_control`. claude-code emits the header
        // unconditionally; the body-side `scope: "global"` field is
        // separately gated on `is_first_party_base_url` because 3P
        // gateways reject it (tools taint the cache prefix). Sending
        // the header without the field is a server-side no-op but
        // matches the canonical wire fingerprint.
        out.push(PROMPT_CACHING_SCOPE_BETA_HEADER);
        if caps.effort {
            out.push(EFFORT_BETA_HEADER);
        }
    }

    // 1M context is explicit user opt-in via the `[1m]` model suffix.
    // Family-based auto-enable would break subscriptions without 1M
    // access, so we require the tag and cross-check the capability.
    if has_1m_tag(model) && caps.context_1m {
        out.push(CONTEXT_1M_BETA_HEADER);
    }
    // Structured outputs is one-shot only — streaming turns are free-form.
    if want_structured && caps.structured_outputs {
        out.push(STRUCTURED_OUTPUTS_BETA_HEADER);
    }

    out
}

/// Whether the target model accepts the `structured-outputs-2025-12-15`
/// beta. Thin wrapper over the capability table for pre-checks.
pub(super) fn supports_structured_outputs(model: &str) -> bool {
    crate::model::capabilities_for(model).structured_outputs
}

/// Whether `base_url` points at the first-party Anthropic API, gating
/// features that strict 3P proxies reject (currently: global-scope
/// prompt caching + its beta header).
///
/// An unparsable URL, a URL with no host, or any host other than the
/// 1P list is treated as third-party; the safe fallback (drop scope +
/// beta) preserves org-level ephemeral caching, which every gateway
/// accepts.
pub(super) fn is_first_party_base_url(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| {
            matches!(
                h.as_str(),
                "api.anthropic.com" | "api-staging.anthropic.com"
            )
        })
}

/// Cache-control for the static system-prompt prefix. On 1P, emit the
/// global scope so the prefix is shared across sessions; on 3P, fall
/// back to the default (org-scoped) ephemeral cache — 3P gateways
/// reject `scope: "global"` because tool definitions render first and
/// taint the cache prefix. `ttl` overrides the server default (5 m)
/// when set via `config.prompt_cache_ttl`.
pub(super) fn static_prefix_cache_control(
    is_first_party: bool,
    ttl: PromptCacheTtl,
) -> CacheControl {
    CacheControl {
        r#type: "ephemeral",
        scope: is_first_party.then_some("global"),
        ttl: ttl.wire(),
    }
}

/// Strips the `[1m]` tag from a caller-supplied model string. The tag
/// is a client-side convention; the API rejects it on the wire.
pub(super) fn api_model_id(model: &str) -> &str {
    tag_offset(model).map_or(model, |i| model[..i].trim_end())
}

/// Whether `model` carries the `[1m]` tag — an explicit user opt-in
/// to the 1M-context window (auto-gating on family would 400 on
/// subscriptions without 1M access).
fn has_1m_tag(model: &str) -> bool {
    tag_offset(model).is_some()
}

/// Byte offset of the `[1m]` tag, case-insensitive. Shared by
/// [`has_1m_tag`] and [`api_model_id`] so the two agree on every
/// accepted spelling. Model IDs are ASCII, so byte-window scanning
/// lines up with character boundaries.
fn tag_offset(model: &str) -> Option<usize> {
    model
        .as_bytes()
        .windows(4)
        .position(|w| w.eq_ignore_ascii_case(b"[1m]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_key() -> Auth {
        Auth::ApiKey("k".to_owned())
    }

    fn oauth() -> Auth {
        Auth::OAuth("t".to_owned())
    }

    // ── compute_betas ──

    #[test]
    fn compute_betas_agentic_opus_4_6_plain_carries_full_set_except_1m() {
        // Plain model (no `[1m]` tag) must not auto-enable 1M context —
        // a gateway without 1M access would 400. The exact-equality
        // assertion locks beta order to claude-code 2.1.121's wire
        // capture: identity / auth → interleaved-thinking → context-
        // management → prompt-caching-scope → effort. 3P proxies
        // fingerprint this ordering.
        let betas = compute_betas("claude-opus-4-6", &api_key(), true, false);
        assert_eq!(
            betas,
            vec![
                CLAUDE_CODE_BETA_HEADER,
                INTERLEAVED_THINKING_BETA_HEADER,
                CONTEXT_MANAGEMENT_BETA_HEADER,
                PROMPT_CACHING_SCOPE_BETA_HEADER,
                EFFORT_BETA_HEADER,
            ],
        );
    }

    #[test]
    fn compute_betas_opus_4_6_with_1m_tag_adds_context_1m() {
        let betas = compute_betas("claude-opus-4-6[1m]", &api_key(), true, false);
        assert!(betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(betas.contains(&EFFORT_BETA_HEADER));
    }

    #[test]
    fn compute_betas_oauth_adds_oauth_header() {
        let betas = compute_betas("claude-opus-4-6", &oauth(), true, false);
        assert!(betas.contains(&OAUTH_BETA_HEADER));
    }

    #[test]
    fn compute_betas_sonnet_4_5_has_thinking_but_not_effort() {
        // Sonnet 4.5 supports interleaved thinking but not effort;
        // plain (no `[1m]` tag) means no 1M beta either.
        let betas = compute_betas("claude-sonnet-4-5", &api_key(), true, false);
        assert!(betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(betas.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(!betas.contains(&EFFORT_BETA_HEADER));
    }

    #[test]
    fn compute_betas_haiku_4_5_agentic_omits_1m_effort_and_thinking() {
        // Haiku has a 200K window and no interleaved-thinking / effort
        // support on 3P gateways; all three must be absent.
        let betas = compute_betas("claude-haiku-4-5", &api_key(), true, false);
        assert!(betas.contains(&CLAUDE_CODE_BETA_HEADER));
        assert!(betas.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
        assert!(!betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(!betas.contains(&EFFORT_BETA_HEADER));
    }

    #[test]
    fn compute_betas_haiku_4_5_with_1m_tag_silently_drops_1m() {
        let betas = compute_betas("claude-haiku-4-5[1m]", &api_key(), true, false);
        assert!(!betas.contains(&CONTEXT_1M_BETA_HEADER));
    }

    #[test]
    fn compute_betas_haiku_non_agentic_minimal() {
        // Title-generator one-shot on API key → no agent tags, no gateway
        // tag. OAuth one-shot → only the OAuth tag.
        assert_eq!(
            compute_betas("claude-haiku-4-5", &api_key(), false, false),
            Vec::<&str>::new(),
        );
        assert_eq!(
            compute_betas("claude-haiku-4-5", &oauth(), false, false),
            vec![OAUTH_BETA_HEADER],
        );
    }

    #[test]
    fn compute_betas_non_haiku_non_agentic_keeps_claude_code_tag() {
        // OAuth on non-Haiku requires the gateway tag even for one-shots.
        let betas = compute_betas("claude-sonnet-4-6", &oauth(), false, false);
        assert!(betas.contains(&CLAUDE_CODE_BETA_HEADER));
        assert!(betas.contains(&OAUTH_BETA_HEADER));
        assert!(!betas.contains(&PROMPT_CACHING_SCOPE_BETA_HEADER));
        assert!(!betas.contains(&INTERLEAVED_THINKING_BETA_HEADER));
    }

    #[test]
    fn compute_betas_opus_4_7_matches_opus_4_6_family() {
        let plain = compute_betas("claude-opus-4-7", &api_key(), true, false);
        assert!(plain.contains(&INTERLEAVED_THINKING_BETA_HEADER));
        assert!(plain.contains(&CONTEXT_MANAGEMENT_BETA_HEADER));
        assert!(plain.contains(&EFFORT_BETA_HEADER));
        assert!(!plain.contains(&CONTEXT_1M_BETA_HEADER));

        let with_1m = compute_betas("claude-opus-4-7[1m]", &api_key(), true, false);
        assert!(with_1m.contains(&CONTEXT_1M_BETA_HEADER));
    }

    #[test]
    fn compute_betas_structured_outputs_gated_by_model_capability() {
        // Haiku 4.5 supports it → emitted alone on non-agentic API key.
        // Haiku 4 base predates the beta → silently dropped.
        assert_eq!(
            compute_betas("claude-haiku-4-5", &api_key(), false, true),
            vec![STRUCTURED_OUTPUTS_BETA_HEADER],
        );
        assert!(
            !compute_betas("claude-haiku-4", &api_key(), false, true)
                .contains(&STRUCTURED_OUTPUTS_BETA_HEADER),
        );
    }

    // ── supports_structured_outputs ──

    #[test]
    fn supports_structured_outputs_reflects_capability_table() {
        assert!(supports_structured_outputs("claude-haiku-4-5"));
        assert!(supports_structured_outputs("claude-opus-4-7"));
        assert!(!supports_structured_outputs("claude-haiku-4"));
        assert!(!supports_structured_outputs("claude-opus-5-0"));
    }

    // ── is_first_party_base_url ──

    #[test]
    fn is_first_party_base_url_accepts_official_hosts() {
        assert!(is_first_party_base_url("https://api.anthropic.com"));
        assert!(is_first_party_base_url("https://api.anthropic.com/"));
        assert!(is_first_party_base_url("https://api-staging.anthropic.com"));
        // Case-insensitive on the host (URL spec lowercases it).
        assert!(is_first_party_base_url("https://API.ANTHROPIC.COM"));
    }

    #[test]
    fn is_first_party_base_url_rejects_proxies_and_malformed_urls() {
        // Proxies and self-hosted gateways → 3P. Also anything that
        // doesn't parse as a URL falls through to the safe default.
        assert!(!is_first_party_base_url("https://api.openai.com"));
        assert!(!is_first_party_base_url("https://proxy.example.com"));
        assert!(!is_first_party_base_url("https://anthropic.com.evil.io"));
        assert!(!is_first_party_base_url("http://127.0.0.1:8080"));
        assert!(!is_first_party_base_url(""));
        assert!(!is_first_party_base_url("not-a-url"));
    }

    // ── static_prefix_cache_control ──

    #[test]
    fn static_prefix_cache_control_emits_global_scope_on_first_party_only() {
        let first = static_prefix_cache_control(true, PromptCacheTtl::OneHour);
        assert_eq!(first.r#type, "ephemeral");
        assert_eq!(first.scope, Some("global"));

        let third = static_prefix_cache_control(false, PromptCacheTtl::OneHour);
        assert_eq!(third.r#type, "ephemeral");
        assert_eq!(third.scope, None);
    }

    #[test]
    fn static_prefix_cache_control_ttl_matches_config() {
        // 1h → `ttl: "1h"` in the wire. 5m → field absent entirely
        // (matches server default; keeps the pre-2026-03 wire shape).
        let one_hour = static_prefix_cache_control(false, PromptCacheTtl::OneHour);
        assert_eq!(
            serde_json::to_string(&one_hour).unwrap(),
            r#"{"type":"ephemeral","ttl":"1h"}"#,
        );

        let five_min = static_prefix_cache_control(false, PromptCacheTtl::FiveMin);
        assert_eq!(
            serde_json::to_string(&five_min).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
    }

    // ── api_model_id ──

    #[test]
    fn api_model_id_strips_1m_tag_case_insensitively() {
        // Case-insensitive matching keeps `api_model_id` and `has_1m_tag`
        // in sync — a leaked `[1M]` in the API model field would 400.
        assert_eq!(api_model_id("claude-opus-4-7[1m]"), "claude-opus-4-7");
        assert_eq!(api_model_id("claude-opus-4-7[1M]"), "claude-opus-4-7");
        assert_eq!(api_model_id("claude-opus-4-7 [1m]"), "claude-opus-4-7");
        assert_eq!(api_model_id("claude-opus-4-7"), "claude-opus-4-7");
    }

    // ── has_1m_tag ──

    #[test]
    fn has_1m_tag_is_case_insensitive() {
        assert!(has_1m_tag("claude-opus-4-7[1m]"));
        assert!(has_1m_tag("claude-opus-4-7[1M]"));
        assert!(!has_1m_tag("claude-opus-4-7"));
    }
}
