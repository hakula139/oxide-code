//! Per-request `anthropic-beta` header computation.

use crate::config::PromptCacheTtl;
use crate::config::auth::AuthKind;
use crate::model::{Family, ModelSpec, ThinkingConfig, ThinkingDisplay};

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
/// Agentic turns on catalog models send the sequence Claude Code [`super::CLAUDE_CLI_VERSION`]
/// sends for them, captured from `claude -p` with an API key, so it includes betas for features
/// this client leaves unused. Fable 5.1, which that capture lacks, sends the Opus 5.5 sequence.
/// `oauth` takes the slot Claude Code's `getAllModelBetas` gives it. One-shots carry at most the
/// claude-code tag and the auth extension. The thinking betas derive from the request body's
/// `thinking` field, so they always match it.
pub(super) fn compute_betas(
    model: &ModelSpec,
    thinking: Option<&ThinkingConfig>,
    auth: AuthKind,
    is_agentic: bool,
) -> Vec<&'static str> {
    let is_haiku = model
        .info()
        .is_some_and(|info| info.family == Family::Haiku);

    let mut out = Vec::with_capacity(12);

    if !is_haiku {
        out.push(CLAUDE_CODE_BETA_HEADER);
    }
    if auth == AuthKind::OAuth {
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

/// `cache_control` for the static system prefix. `scope: "global"` shares the cache across
/// sessions, and Claude Code sends it only to the Anthropic API.
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
    use super::super::CLAUDE_CLI_VERSION;
    use super::*;

    mod compute_betas {
        use super::*;

        fn spec(id: &str) -> ModelSpec {
            ModelSpec::resolve(id.to_owned())
        }

        #[test]
        fn agentic_known_model_mirrors_claude_code_then_appends_binding() {
            assert_eq!(
                CLAUDE_CLI_VERSION, "2.1.296",
                "the sequences below were captured from this release, \
                 so a bump needs a fresh capture",
            );
            let claude_code = "claude-code-20250219,\
                               interleaved-thinking-2025-05-14,\
                               thinking-token-count-2026-05-13,\
                               context-management-2025-06-27,\
                               prompt-caching-scope-2026-01-05,\
                               mid-conversation-system-2026-04-07,\
                               per-turn-control-2026-07-01,\
                               advisor-tool-2026-03-01,\
                               effort-2025-11-24";
            let haiku = "interleaved-thinking-2025-05-14,\
                         thinking-token-count-2026-05-13,\
                         context-management-2025-06-27,\
                         prompt-caching-scope-2026-01-05,\
                         mid-conversation-system-2026-04-07,\
                         claude-code-20250219,\
                         per-turn-control-2026-07-01,\
                         advisor-tool-2026-03-01,\
                         effort-2025-11-24";
            for (id, captured) in [
                // Uncaptured on this release, so it reuses the Opus 5.5 sequence
                // `compute_betas` cites.
                ("claude-fable-5-1", claude_code),
                ("claude-opus-5-5", claude_code),
                ("claude-sonnet-5-5", claude_code),
                ("claude-haiku-5-5", haiku),
            ] {
                let model = spec(id);
                assert_eq!(
                    compute_betas(
                        &model,
                        model.thinking(true).as_ref(),
                        AuthKind::ApiKey,
                        true
                    )
                    .join(","),
                    format!("{captured},thinking-binding-controls-2026-08-01"),
                    "{id}",
                );
            }
        }

        #[test]
        fn oauth_slots_auth_header_after_claude_code_tag() {
            assert_eq!(
                compute_betas(&spec("claude-opus-5-5"), None, AuthKind::OAuth, true)[..3],
                [
                    CLAUDE_CODE_BETA_HEADER,
                    OAUTH_BETA_HEADER,
                    INTERLEAVED_THINKING_BETA_HEADER,
                ],
            );
            // Haiku's agentic claude-code tag comes later, so the auth header leads.
            assert_eq!(
                compute_betas(&spec("claude-haiku-5-5"), None, AuthKind::OAuth, true)[..2],
                [OAUTH_BETA_HEADER, INTERLEAVED_THINKING_BETA_HEADER],
            );
            // A gateway auth token keeps API-key betas, since the OAuth beta is subscription-only.
            let opus = spec("claude-opus-5-5");
            assert_eq!(
                compute_betas(&opus, None, AuthKind::AuthToken, true),
                compute_betas(&opus, None, AuthKind::ApiKey, true),
            );
            assert!(
                !compute_betas(&opus, None, AuthKind::ApiKey, true).contains(&OAUTH_BETA_HEADER)
            );
        }

        #[test]
        fn updates_display_appends_its_beta_last() {
            let opus = spec("claude-opus-5-5");
            let betas = compute_betas(&opus, opus.thinking(false).as_ref(), AuthKind::ApiKey, true);
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
                !compute_betas(
                    &haiku,
                    haiku.thinking(false).as_ref(),
                    AuthKind::ApiKey,
                    true
                )
                .contains(&THINKING_DISPLAY_UPDATES_BETA_HEADER),
            );
        }

        #[test]
        fn agentic_unknown_model_sends_only_tag_and_cache_scope() {
            assert_eq!(
                compute_betas(&spec("custom-model"), None, AuthKind::ApiKey, true),
                vec![CLAUDE_CODE_BETA_HEADER, PROMPT_CACHING_SCOPE_BETA_HEADER],
            );
        }

        #[test]
        fn haiku_non_agentic_minimal() {
            assert_eq!(
                compute_betas(&spec("claude-haiku-5-5"), None, AuthKind::ApiKey, false),
                Vec::<&str>::new(),
            );
            assert_eq!(
                compute_betas(&spec("claude-haiku-5-5"), None, AuthKind::OAuth, false),
                vec![OAUTH_BETA_HEADER],
            );
        }

        #[test]
        fn non_haiku_non_agentic_keeps_claude_code_tag() {
            assert_eq!(
                compute_betas(&spec("claude-sonnet-5-5"), None, AuthKind::OAuth, false),
                vec![CLAUDE_CODE_BETA_HEADER, OAUTH_BETA_HEADER],
            );
            // Family comes from the catalog row, so an unknown id naming Haiku keeps the tag.
            assert_eq!(
                compute_betas(&spec("my-haiku-gateway"), None, AuthKind::ApiKey, false),
                vec![CLAUDE_CODE_BETA_HEADER],
            );
        }
    }

    mod static_prefix_cache_control {
        use super::*;

        #[test]
        fn emits_global_scope_on_first_party_only() {
            let first = static_prefix_cache_control(true, PromptCacheTtl::OneHour);
            assert_eq!(first.r#type, "ephemeral");
            assert_eq!(first.scope, Some("global"));

            let third = static_prefix_cache_control(false, PromptCacheTtl::OneHour);
            assert_eq!(third.r#type, "ephemeral");
            assert_eq!(third.scope, None);
        }

        #[test]
        fn ttl_matches_config() {
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
    }

    mod conversation_cache_control {
        use super::*;

        #[test]
        fn carries_ttl_without_scope() {
            assert_eq!(
                serde_json::to_string(&conversation_cache_control(PromptCacheTtl::OneHour))
                    .unwrap(),
                r#"{"type":"ephemeral","ttl":"1h"}"#,
            );
            assert_eq!(
                serde_json::to_string(&conversation_cache_control(PromptCacheTtl::FiveMin))
                    .unwrap(),
                r#"{"type":"ephemeral"}"#,
            );
        }
    }
}
