use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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

#[derive(Debug, Clone)]
pub struct Config {
    pub auth: Auth,
    pub model: String,
    pub base_url: String,
    pub max_tokens: u32,
}

impl Config {
    /// Load configuration.
    ///
    /// Auth priority: `ANTHROPIC_API_KEY` env var > Claude Code OAuth
    /// credentials at `~/.claude/.credentials.json`.
    pub fn load() -> Result<Self> {
        let auth = if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            Auth::ApiKey(key)
        } else {
            let token = load_claude_oauth()
                .context("ANTHROPIC_API_KEY not set and Claude Code credentials not found")?;
            Auth::OAuth(token)
        };

        let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());

        let base_url =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());

        let max_tokens = std::env::var("ANTHROPIC_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_TOKENS);

        Ok(Self {
            auth,
            model,
            base_url,
            max_tokens,
        })
    }
}

// ── Claude Code OAuth ──

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OAuthCredential,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthCredential {
    access_token: String,
    expires_at: i64,
}

fn credentials_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join(".credentials.json"))
}

fn load_claude_oauth() -> Result<String> {
    let path = credentials_path().context("could not determine home directory")?;

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let creds: CredentialsFile =
        serde_json::from_str(&content).context("failed to parse Claude Code credentials")?;

    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_millis(),
    )
    .expect("current time fits in u64");

    let expires_at = u64::try_from(creds.claude_ai_oauth.expires_at).unwrap_or(0);
    if expires_at <= now_ms {
        bail!("Claude Code OAuth token has expired — run `claude` to refresh");
    }

    Ok(creds.claude_ai_oauth.access_token)
}
