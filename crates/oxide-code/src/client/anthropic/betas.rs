//! Per-request `anthropic-beta` header computation.

use crate::config::{Auth, PromptCacheTtl, ThinkingConfig, ThinkingDisplay};
use crate::model::{Family, ModelSpec};

use super::wire::CacheControl;

const ADVISOR_TOOL_BETA_HEADER: &str = "advisor-tool-2026-03-01";
pub(super) const CLAUDE_CODE_BETA_HEADER: &str = "claude-code-20250219";
const CONTEXT_MANAGEMENT_BETA_HEADER: &str = "context-management-2025-06-27";
const EFFORT_BETA_HEADER: &str = "effort-2025-11-24";
const INTERLEAVED_THINKING_BETA_HEADER: &str = "interleaved-thinking-2025-05-14";
const MID_CONVERSATION_SYSTEM_BETA_HEADER: &str = "mid-conversation-system-2026-04-07";
pub(super) const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const PER_TURN_CONTROL_BETA_HEADER: &str = "per-turn-control-2026-07-01";
const PROMPT_CACHING_SCOPE_BETA_HEADER: &str = "prompt-caching-scope-2026-01-05";
const THINKING_BINDING_CONTROLS_BETA_HEADER: &str = "thinking-binding-controls-2026-08-01";
const THINKING_DISPLAY_UPDATES_BETA_HEADER: &str = "thinking-display-updates-2026-08-18";
const THINKING_TOKEN_COUNT_BETA_HEADER: &str = "thinking-token-count-2026-05-13";

/// Builds the comma-joined `anthropic-beta` header value.
///
/// Order is load-bearing: 3P proxies fingerprint the exact sequence and reject mismatches. Agentic
/// turns on catalogue models send the sequence Claude Code 2.1.296 sends for them, captured from
/// `claude -p` with an API key, so it includes betas for features this client leaves unused.
/// `oauth` takes the slot Claude Code's `getAllModelBetas` gives it. One-shots carry at most the
/// claude-code tag and the auth extension. The thinking betas derive from the request body's
/// `thinking` field, so they always match it.
pub(super) fn compute_betas(
    model: &ModelSpec,
    thinking: Option<&ThinkingConfig>,
    auth: &Auth,
    is_agentic: bool,
) -> Vec<&'static str> {
    let is_haiku = model
        .info()
        .is_some_and(|info| info.family == Family::Haiku);

    let mut out = Vec::with_capacity(12);

    if !is_haiku {
        out.push(CLAUDE_CODE_BETA_HEADER);
    }
    if matches!(auth, Auth::OAuth(_)) {
        out.push(OAUTH_BETA_HEADER);
    }
    if is_agentic {
        if model.info().is_some() {
            out.extend([
                INTERLEAVED_THINKING_BETA_HEADER,
                THINKING_TOKEN_COUNT_BETA_HEADER,
                CONTEXT_MANAGEMENT_BETA_HEADER,
                PROMPT_CACHING_SCOPE_BETA_HEADER,
                MID_CONVERSATION_SYSTEM_BETA_HEADER,
            ]);
            if is_haiku {
                out.push(CLAUDE_CODE_BETA_HEADER);
            }
            out.extend([
                PER_TURN_CONTROL_BETA_HEADER,
                ADVISOR_TOOL_BETA_HEADER,
                EFFORT_BETA_HEADER,
            ]);
        } else {
            out.push(PROMPT_CACHING_SCOPE_BETA_HEADER);
        }
    }
    if let Some(ThinkingConfig::Adaptive { display, .. }) = thinking {
        out.push(THINKING_BINDING_CONTROLS_BETA_HEADER);
        if *display == Some(ThinkingDisplay::Updates) {
            out.push(THINKING_DISPLAY_UPDATES_BETA_HEADER);
        }
    }

    out
}

/// Whether `base_url` points at Anthropic's production or staging host. Gates body-side
/// `cache_control.scope = "global"`, which 3P gateways reject downstream of tools.
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

/// `cache_control` for the static system prefix. `scope: "global"` shares the cache across
/// sessions and is 1P-only (see [`is_first_party_base_url`]).
pub(super) fn static_prefix_cache_control(
    is_first_party: bool,
    ttl: PromptCacheTtl,
) -> CacheControl {
    CacheControl {
        scope: is_first_party.then_some("global"),
        ..conversation_cache_control(ttl)
    }
}

/// `cache_control` for the breakpoint on the conversation's final block. The transcript is
/// session-specific, so it never carries `scope`.
pub(super) fn conversation_cache_control(ttl: PromptCacheTtl) -> CacheControl {
    CacheControl {
        r#type: "ephemeral",
        scope: None,
        ttl: ttl.wire(),
    }
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

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::resolve(id.to_owned())
    }

    // ── compute_betas ──

    #[test]
    fn compute_betas_agentic_known_model_mirrors_claude_code_then_appends_binding() {
        let claude_code = "claude-code-20250219,interleaved-thinking-2025-05-14,\
            thinking-token-count-2026-05-13,context-management-2025-06-27,\
            prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
            per-turn-control-2026-07-01,advisor-tool-2026-03-01,effort-2025-11-24";
        let haiku = "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
            context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
            mid-conversation-system-2026-04-07,claude-code-20250219,per-turn-control-2026-07-01,\
            advisor-tool-2026-03-01,effort-2025-11-24";
        for (id, captured) in [
            ("claude-opus-5-5", claude_code),
            ("claude-sonnet-5-5", claude_code),
            ("claude-haiku-5-5", haiku),
        ] {
            let model = spec(id);
            assert_eq!(
                compute_betas(&model, model.thinking(true).as_ref(), &api_key(), true).join(","),
                format!("{captured},thinking-binding-controls-2026-08-01"),
                "{id}",
            );
        }
    }

    #[test]
    fn compute_betas_oauth_slots_auth_header_after_claude_code_tag() {
        assert_eq!(
            compute_betas(&spec("claude-opus-5-5"), None, &oauth(), true)[..3],
            [
                CLAUDE_CODE_BETA_HEADER,
                OAUTH_BETA_HEADER,
                INTERLEAVED_THINKING_BETA_HEADER,
            ],
        );
        // Haiku's agentic claude-code tag comes later, so the auth header leads.
        assert_eq!(
            compute_betas(&spec("claude-haiku-5-5"), None, &oauth(), true)[..2],
            [OAUTH_BETA_HEADER, INTERLEAVED_THINKING_BETA_HEADER],
        );
    }

    #[test]
    fn compute_betas_updates_display_appends_its_beta_last() {
        let opus = spec("claude-opus-5-5");
        let betas = compute_betas(&opus, opus.thinking(false).as_ref(), &api_key(), true);
        assert_eq!(
            betas[betas.len() - 3..],
            [
                EFFORT_BETA_HEADER,
                THINKING_BINDING_CONTROLS_BETA_HEADER,
                THINKING_DISPLAY_UPDATES_BETA_HEADER,
            ],
        );
        // Haiku has no progress updates, so hidden thinking sends no display and no beta.
        let haiku = spec("claude-haiku-5-5");
        assert!(
            !compute_betas(&haiku, haiku.thinking(false).as_ref(), &api_key(), true)
                .contains(&THINKING_DISPLAY_UPDATES_BETA_HEADER),
        );
    }

    #[test]
    fn compute_betas_agentic_unknown_model_sends_only_tag_and_cache_scope() {
        assert_eq!(
            compute_betas(&spec("custom-model"), None, &api_key(), true),
            vec![CLAUDE_CODE_BETA_HEADER, PROMPT_CACHING_SCOPE_BETA_HEADER],
        );
    }

    #[test]
    fn compute_betas_haiku_non_agentic_minimal() {
        assert_eq!(
            compute_betas(&spec("claude-haiku-5-5"), None, &api_key(), false),
            Vec::<&str>::new(),
        );
        assert_eq!(
            compute_betas(&spec("claude-haiku-5-5"), None, &oauth(), false),
            vec![OAUTH_BETA_HEADER],
        );
    }

    #[test]
    fn compute_betas_non_haiku_non_agentic_keeps_claude_code_tag() {
        assert_eq!(
            compute_betas(&spec("claude-sonnet-5-5"), None, &oauth(), false),
            vec![CLAUDE_CODE_BETA_HEADER, OAUTH_BETA_HEADER],
        );
        // Family comes from the catalogue row, so a retired Haiku id is treated as unknown.
        assert_eq!(
            compute_betas(&spec("claude-haiku-4-5"), None, &api_key(), false),
            vec![CLAUDE_CODE_BETA_HEADER],
        );
    }

    // ── is_first_party_base_url ──

    #[test]
    fn is_first_party_base_url_accepts_official_hosts() {
        assert!(is_first_party_base_url("https://api.anthropic.com"));
        assert!(is_first_party_base_url("https://api.anthropic.com/"));
        assert!(is_first_party_base_url("https://api-staging.anthropic.com"));
        assert!(is_first_party_base_url("https://API.ANTHROPIC.COM"));
    }

    #[test]
    fn is_first_party_base_url_rejects_proxies_and_malformed_urls() {
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

    // ── conversation_cache_control ──

    #[test]
    fn conversation_cache_control_carries_ttl_without_scope() {
        assert_eq!(
            serde_json::to_string(&conversation_cache_control(PromptCacheTtl::OneHour)).unwrap(),
            r#"{"type":"ephemeral","ttl":"1h"}"#,
        );
        assert_eq!(
            serde_json::to_string(&conversation_cache_control(PromptCacheTtl::FiveMin)).unwrap(),
            r#"{"type":"ephemeral"}"#,
        );
    }
}
