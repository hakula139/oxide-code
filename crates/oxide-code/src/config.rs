mod file;
mod oauth;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::util::env;

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

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Model decides the thinking budget (Claude 4.6+).
    Adaptive,
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
    /// Load configuration from files and environment variables.
    ///
    /// Precedence (highest wins): env vars > project `ox.toml` > user
    /// `~/.config/ox/config.toml` > built-in defaults.
    ///
    /// Auth priority: `ANTHROPIC_API_KEY` env var > `api_key` in config
    /// file > Claude Code OAuth credentials.
    pub async fn load() -> Result<Self> {
        let fc = file::load();
        let client = fc.client.unwrap_or_default();
        let tui = fc.tui.unwrap_or_default();

        let auth = if let Some(key) = env::string("ANTHROPIC_API_KEY").or(client.api_key) {
            Auth::ApiKey(key)
        } else {
            let token = oauth::load_token().await.context(
                "no credentials available: set ANTHROPIC_API_KEY, add `api_key` to \
                 ox.toml, or sign in with Claude Code (checks macOS Keychain and \
                 ~/.claude/.credentials.json)",
            )?;
            Auth::OAuth(token)
        };

        let model = env::string("ANTHROPIC_MODEL")
            .or(client.model)
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());

        let base_url = env::string("ANTHROPIC_BASE_URL")
            .or(client.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());

        let max_tokens = env::string("ANTHROPIC_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .or(client.max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        // Adaptive thinking is always enabled — the model decides the budget.
        let thinking = Some(ThinkingConfig::Adaptive);

        let show_thinking = env::bool("OX_SHOW_THINKING")
            .or(tui.show_thinking)
            .unwrap_or(false);

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

#[cfg(test)]
mod tests {
    use super::*;

    // ── ThinkingConfig ──

    #[test]
    fn thinking_config_adaptive_serializes() {
        let json = serde_json::to_value(&ThinkingConfig::Adaptive).unwrap();
        assert_eq!(json["type"], "adaptive");
    }
}
