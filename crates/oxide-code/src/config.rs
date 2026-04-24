//! Configuration loading.
//!
//! Layered precedence (highest wins): env vars > project `ox.toml` >
//! user `~/.config/ox/config.toml` > built-in defaults. Auth follows
//! the same precedence but terminates at the first source that
//! resolves (API key env > API key in file > OAuth credentials).

mod file;
mod oauth;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::util::env;

const DEFAULT_MODEL: &str = "claude-opus-4-7";
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
    /// Loads configuration from files and environment variables.
    ///
    /// Precedence (highest wins): env vars > project `ox.toml` > user
    /// `~/.config/ox/config.toml` > built-in defaults.
    ///
    /// Auth priority: `ANTHROPIC_API_KEY` env var > `api_key` in config
    /// file > Claude Code OAuth credentials.
    pub async fn load() -> Result<Self> {
        let fc = file::load()?;
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
    use std::collections::HashSet;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    // ── ThinkingConfig ──

    #[test]
    fn thinking_config_adaptive_serializes() {
        let json = serde_json::to_value(&ThinkingConfig::Adaptive).unwrap();
        assert_eq!(json["type"], "adaptive");
    }

    // ── Config::load ──

    /// Env keys `Config::load` reads. Baseline for [`env_vars`] so
    /// nothing bleeds in from the caller's environment; `ANTHROPIC_API_KEY`
    /// ships with a non-empty default so tests land on the `ApiKey` arm
    /// and never consult the real OAuth credential sources.
    const ENV_KEYS: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MAX_TOKENS",
        "OX_SHOW_THINKING",
        "XDG_CONFIG_HOME",
    ];

    fn write_user_config(xdg_dir: &Path, body: &str) {
        let config_dir = xdg_dir.join("ox");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), body).unwrap();
    }

    /// Baseline env list (every [`ENV_KEYS`] entry unset, `ANTHROPIC_API_KEY`
    /// set to `"sk-default"`) with `overrides` applied on top. Panics if
    /// an override key is not in [`ENV_KEYS`] so a misspelling surfaces
    /// immediately. Returns a `Vec` because `temp_env::async_with_vars`
    /// takes `AsRef<[(K, Option<V>)]>`.
    fn env_vars(
        overrides: impl IntoIterator<Item = (&'static str, Option<String>)>,
    ) -> Vec<(&'static str, Option<String>)> {
        let known: HashSet<&'static str> = ENV_KEYS.iter().copied().collect();
        let mut out: Vec<(&'static str, Option<String>)> = ENV_KEYS
            .iter()
            .copied()
            .map(|k| {
                (
                    k,
                    (k == "ANTHROPIC_API_KEY").then(|| "sk-default".to_owned()),
                )
            })
            .collect();
        for (key, value) in overrides {
            assert!(known.contains(key), "env key {key:?} not in ENV_KEYS");
            if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            }
        }
        out
    }

    fn xdg(dir: &TempDir) -> (&'static str, Option<String>) {
        (
            "XDG_CONFIG_HOME",
            Some(dir.path().to_string_lossy().into_owned()),
        )
    }

    fn env(key: &'static str, value: &str) -> (&'static str, Option<String>) {
        (key, Some(value.to_owned()))
    }

    #[tokio::test]
    async fn load_defaults_apply_when_no_config_and_no_env() {
        let dir = tempfile::tempdir().unwrap();
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(!config.show_thinking);
        assert!(matches!(config.auth, Auth::ApiKey(k) if k == "sk-default"));
    }

    #[tokio::test]
    async fn load_env_overrides_every_client_field() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-4-7"),
            env("ANTHROPIC_BASE_URL", "https://example.invalid"),
            env("ANTHROPIC_MAX_TOKENS", "64"),
            env("OX_SHOW_THINKING", "1"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, "claude-opus-4-7");
        assert_eq!(config.base_url, "https://example.invalid");
        assert_eq!(config.max_tokens, 64);
        assert!(config.show_thinking);
    }

    #[tokio::test]
    async fn load_user_config_supplies_values_without_env_overrides() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-sonnet-4-6"
                base_url = "https://config-file.invalid"
                max_tokens = 128

                [tui]
                show_thinking = true
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.base_url, "https://config-file.invalid");
        assert_eq!(config.max_tokens, 128);
        assert!(config.show_thinking);
    }

    #[tokio::test]
    async fn load_env_beats_config_file_field_by_field() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-sonnet-4-6"
                max_tokens = 128

                [tui]
                show_thinking = true
            "#},
        );
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-4-7"),
            // `max_tokens` env is unset — the file value must win.
            env("OX_SHOW_THINKING", "0"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, "claude-opus-4-7");
        assert_eq!(config.max_tokens, 128);
        assert!(!config.show_thinking, "env `0` overrides file `true`");
    }

    #[tokio::test]
    async fn load_env_api_key_beats_file_api_key() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                api_key = "sk-from-file"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_API_KEY", "sk-from-env")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(matches!(config.auth, Auth::ApiKey(k) if k == "sk-from-env"));
    }

    #[tokio::test]
    async fn load_file_api_key_used_when_env_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                api_key = "sk-from-file"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_API_KEY", "")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(matches!(config.auth, Auth::ApiKey(k) if k == "sk-from-file"));
    }

    #[tokio::test]
    async fn load_adaptive_thinking_is_always_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert!(matches!(config.thinking, Some(ThinkingConfig::Adaptive)));
    }

    #[tokio::test]
    async fn load_invalid_max_tokens_env_falls_through_to_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {"
                [client]
                max_tokens = 128
            "},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_MAX_TOKENS", "not-a-number")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.max_tokens, 128,
            "unparsable env must fall through to file value",
        );
    }

    /// Regression: a misplaced field used to drop the entire config
    /// silently (parse error logged at `warn`, invisible without
    /// `RUST_LOG`), which then surfaced as a confusing
    /// "no credentials" error when the dropped config also held the
    /// API key. The parse error must propagate instead.
    #[tokio::test]
    async fn load_propagates_invalid_config_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                api_key = "sk-from-file"
                show_thinking = true
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_API_KEY", "")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("misplaced field must surface as an error");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid config at"), "{msg}");
        assert!(msg.contains("unknown field `show_thinking`"), "{msg}");
    }
}
