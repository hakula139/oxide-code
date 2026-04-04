mod oauth;

use anyhow::{Context, Result};
use serde::Serialize;

const DEFAULT_MODEL: &str = "claude-opus-4-6";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_MAX_TOKENS: u32 = 16384;

#[derive(Debug, Clone)]
pub enum Auth {
    /// Explicit API key (`x-api-key` header).
    ApiKey(String),
    /// OAuth access token from Claude Code (`Authorization: Bearer` header).
    OAuth(String),
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Enabled variant is constructed only in tests; Adaptive is the sole production path"
    )
)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Model decides the thinking budget (Claude 4.6+).
    Adaptive,
    /// Fixed token budget for thinking.
    Enabled { budget_tokens: u32 },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub auth: Auth,
    pub model: String,
    pub base_url: String,
    pub max_tokens: u32,
    pub thinking: Option<ThinkingConfig>,
    pub show_thinking: bool,
}

impl Config {
    /// Load configuration.
    ///
    /// Auth priority: `ANTHROPIC_API_KEY` env var > Claude Code OAuth
    /// credentials at `~/.claude/.credentials.json`.
    pub async fn load() -> Result<Self> {
        let auth = if let Some(key) = non_empty_env("ANTHROPIC_API_KEY") {
            Auth::ApiKey(key)
        } else {
            let token = oauth::load_token()
                .await
                .context("ANTHROPIC_API_KEY not set and Claude Code credentials not found")?;
            Auth::OAuth(token)
        };

        let model = non_empty_env("ANTHROPIC_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_owned());

        let base_url =
            non_empty_env("ANTHROPIC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());

        let max_tokens = non_empty_env("ANTHROPIC_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_TOKENS);

        // Adaptive thinking is always enabled — the model decides the budget.
        let thinking = Some(ThinkingConfig::Adaptive);

        let show_thinking =
            non_empty_env("OX_SHOW_THINKING").is_some_and(|v| v == "1" || v == "true");

        Ok(Self {
            auth,
            model,
            base_url,
            max_tokens,
            thinking,
            show_thinking,
        })
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ThinkingConfig ──

    #[test]
    fn thinking_config_adaptive_serializes() {
        let json = serde_json::to_value(&ThinkingConfig::Adaptive).unwrap();
        assert_eq!(json["type"], "adaptive");
    }

    #[test]
    fn thinking_config_enabled_serializes_with_budget() {
        let json = serde_json::to_value(&ThinkingConfig::Enabled {
            budget_tokens: 10000,
        })
        .unwrap();
        assert_eq!(json["type"], "enabled");
        assert_eq!(json["budget_tokens"], 10000);
    }
}
