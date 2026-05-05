//! Configuration loading.
//!
//! Precedence (highest wins): env > project `ox.toml` > user `~/.config/ox/config.toml` > defaults.
//! Auth stops at the first source that resolves (API key env > API key in file > OAuth).

pub(crate) mod file;
mod oauth;

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::tui::theme::{self, Theme};
use crate::util::env;

const DEFAULT_MODEL: &str = "claude-opus-4-7[1m]";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

// ── Auth ──

/// Resolved credential. First source wins, in order: `ANTHROPIC_API_KEY` env, `client.api_key`
/// in the config file, then OAuth (macOS Keychain → `~/.claude/.credentials.json`).
#[derive(Debug, Clone)]
pub(crate) enum Auth {
    ApiKey(String),
    OAuth(String),
}

impl Auth {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::ApiKey(_) => "API key",
            Self::OAuth(_) => "OAuth",
        }
    }
}

// ── ConfigSnapshot ──

/// Resolved-config view minus the secret. Survives [`Config`] being consumed by the client.
#[derive(Debug, Clone)]
pub(crate) struct ConfigSnapshot {
    pub(crate) model_id: String,
    pub(crate) effort: Option<Effort>,
    pub(crate) auth_label: &'static str,
    pub(crate) base_url: String,
    pub(crate) max_tokens: u32,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) show_thinking: bool,
}

// ── ThinkingConfig ──

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ThinkingConfig {
    /// Model decides the thinking budget (4.6+).
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
}

/// `thinking.display` values accepted on 4.7+.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThinkingDisplay {
    Summarized,
}

// ── Effort ──

/// Intelligence-vs-latency tier sent as `output_config.effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub(crate) const ALL: [Self; 5] = [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];
    pub(crate) const VALID_VALUES: &str = "low, medium, high, xhigh, max";

    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => bail!(
                "invalid effort {s:?}; expected one of: {}",
                Self::VALID_VALUES
            ),
        }
    }
}

// ── PromptCacheTtl ──

/// Sent as `cache_control.ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PromptCacheTtl {
    #[serde(rename = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    OneHour,
}

impl PromptCacheTtl {
    /// `None` for the server default (5 m).
    pub(crate) const fn wire(self) -> Option<&'static str> {
        match self {
            Self::FiveMin => None,
            Self::OneHour => Some("1h"),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::FiveMin => "5m",
            Self::OneHour => "1h",
        }
    }
}

impl fmt::Display for PromptCacheTtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PromptCacheTtl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "5m" => Ok(Self::FiveMin),
            "1h" => Ok(Self::OneHour),
            _ => bail!("invalid prompt_cache_ttl {s:?}; expected one of: 5m, 1h"),
        }
    }
}

// ── Config ──

/// Resolved configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) model: String,
    /// `None` when the model doesn't accept it.
    pub(crate) effort: Option<Effort>,
    pub(crate) auth: Auth,
    pub(crate) base_url: String,
    pub(crate) max_tokens: u32,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) thinking: Option<ThinkingConfig>,
    pub(crate) show_thinking: bool,
    pub(crate) theme: Theme,
}

impl Config {
    /// Resolves config from layered sources. Per-field precedence is env > project `ox.toml` >
    /// user `~/.config/ox/config.toml` > built-in default; see the module docs for the auth
    /// chain. Parse errors (TOML, env, theme) propagate so a typo doesn't degrade silently into
    /// "no credentials".
    pub(crate) async fn load() -> Result<Self> {
        let fc = file::load()?;
        let client = fc.client.unwrap_or_default();
        let tui = fc.tui.unwrap_or_default();
        let theme_config = tui.theme.unwrap_or_default();

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

        let caps = crate::model::capabilities_for(&model);

        let effort_pick = match env::string("ANTHROPIC_EFFORT") {
            Some(raw) => Some(raw.parse::<Effort>().context("ANTHROPIC_EFFORT")?),
            None => client.effort,
        };
        let effort = caps.resolve_effort(effort_pick);

        let max_tokens = env::string("ANTHROPIC_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .or(client.max_tokens)
            .unwrap_or_else(|| default_max_tokens(effort));

        let show_thinking = env::bool("OX_SHOW_THINKING")
            .or(tui.show_thinking)
            .unwrap_or(false);

        // 4.7 silently defaulted to `omitted`; `display` opts back into summarized. 4.6 and older
        // ignore the field.
        let thinking = Some(ThinkingConfig::Adaptive {
            display: show_thinking.then_some(ThinkingDisplay::Summarized),
        });

        let prompt_cache_ttl = match env::string("OX_PROMPT_CACHE_TTL") {
            Some(raw) => raw
                .parse::<PromptCacheTtl>()
                .context("OX_PROMPT_CACHE_TTL")?,
            None => client.prompt_cache_ttl.unwrap_or(PromptCacheTtl::OneHour),
        };

        let theme = theme::resolve_theme(
            theme_config.base.as_deref(),
            &theme_config.overrides.unwrap_or_default(),
        )?;

        Ok(Self {
            model,
            effort,
            auth,
            base_url,
            max_tokens,
            prompt_cache_ttl,
            thinking,
            show_thinking,
            theme,
        })
    }

    /// Descriptors for `/config` and `/status`, minus the auth secret.
    pub(crate) fn snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            model_id: self.model.clone(),
            effort: self.effort,
            auth_label: self.auth.label(),
            base_url: self.base_url.clone(),
            max_tokens: self.max_tokens,
            prompt_cache_ttl: self.prompt_cache_ttl,
            show_thinking: self.show_thinking,
        }
    }
}

// ── Helpers ──

pub(crate) fn display_effort(effort: Option<Effort>) -> String {
    effort.map_or_else(|| "(no effort tier)".to_owned(), |e| e.to_string())
}

fn default_max_tokens(effort: Option<Effort>) -> u32 {
    match effort {
        Some(Effort::Xhigh | Effort::Max) => 64_000,
        Some(Effort::High) => 32_000,
        _ => 16_000,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    // ── Auth::label ──

    #[test]
    fn label_distinguishes_api_key_from_oauth() {
        // Swap would mislabel every user's auth source.
        assert_eq!(Auth::ApiKey("secret".to_owned()).label(), "API key");
        assert_eq!(Auth::OAuth("token".to_owned()).label(), "OAuth");
    }

    // ── ThinkingConfig ──

    #[test]
    fn thinking_config_adaptive_without_display_serializes_bare() {
        let json = serde_json::to_value(&ThinkingConfig::Adaptive { display: None }).unwrap();
        assert_eq!(json["type"], "adaptive");
        assert!(json.get("display").is_none(), "display omitted: {json}");
    }

    #[test]
    fn thinking_config_adaptive_with_summarized_display_serializes() {
        let json = serde_json::to_value(&ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Summarized),
        })
        .unwrap();
        assert_eq!(json["type"], "adaptive");
        assert_eq!(json["display"], "summarized");
    }

    // ── Effort ──

    #[test]
    fn effort_round_trips_through_serde_and_fromstr() {
        for (variant, token) in Effort::ALL
            .into_iter()
            .zip(["low", "medium", "high", "xhigh", "max"])
        {
            assert_eq!(serde_json::to_value(variant).unwrap(), token);
            assert_eq!(variant.to_string(), token);
            assert_eq!(token.parse::<Effort>().unwrap(), variant);
        }
    }

    #[test]
    fn effort_rejects_unknown_tokens_with_actionable_error() {
        let err = "extra-high".parse::<Effort>().expect_err("unknown token");
        let msg = format!("{err:#}");
        assert!(msg.contains("extra-high"), "{msg}");
        for token in ["low", "medium", "high", "xhigh", "max"] {
            assert!(msg.contains(token), "{token}: {msg}");
        }
    }

    #[test]
    fn effort_round_trips_through_toml_deserialize() {
        #[derive(Deserialize)]
        struct Wrap {
            effort: Effort,
        }
        let wrap: Wrap = toml::from_str(r#"effort = "xhigh""#).unwrap();
        assert_eq!(wrap.effort, Effort::Xhigh);
    }

    // ── PromptCacheTtl ──

    #[test]
    fn prompt_cache_ttl_wire_shape() {
        assert_eq!(PromptCacheTtl::FiveMin.wire(), None);
        assert_eq!(PromptCacheTtl::OneHour.wire(), Some("1h"));
    }

    #[test]
    fn prompt_cache_ttl_round_trips_through_serde_and_fromstr() {
        for (variant, token) in [
            (PromptCacheTtl::FiveMin, "5m"),
            (PromptCacheTtl::OneHour, "1h"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), token);
            assert_eq!(variant.to_string(), token);
            assert_eq!(token.parse::<PromptCacheTtl>().unwrap(), variant);
        }
    }

    #[test]
    fn prompt_cache_ttl_rejects_unknown_tokens_with_actionable_error() {
        let err = "30m".parse::<PromptCacheTtl>().expect_err("unknown token");
        let msg = format!("{err:#}");
        assert!(msg.contains("30m"), "{msg}");
        assert!(msg.contains("5m"), "{msg}");
        assert!(msg.contains("1h"), "{msg}");
    }

    // ── Config::load ──

    /// Env keys `Config::load` reads. `ANTHROPIC_API_KEY` defaults non-empty in [`env_vars`] so
    /// tests land on `ApiKey` without consulting OAuth.
    const ENV_KEYS: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MAX_TOKENS",
        "ANTHROPIC_EFFORT",
        "OX_SHOW_THINKING",
        "OX_PROMPT_CACHE_TTL",
        "XDG_CONFIG_HOME",
    ];

    fn write_user_config(xdg_dir: &Path, body: &str) {
        let config_dir = xdg_dir.join("ox");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), body).unwrap();
    }

    /// Baseline env (all unset, `ANTHROPIC_API_KEY` = `"sk-default"`) plus overrides.
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
        // Opus 4.7 supports `xhigh`; `effort` / `max_tokens` derive from that ceiling.
        let dir = tempfile::tempdir().unwrap();
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.max_tokens, 64_000);
        assert_eq!(config.effort, Some(Effort::Xhigh));
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
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
        assert!(matches!(
            config.thinking,
            Some(ThinkingConfig::Adaptive { display: None }),
        ));
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

    /// Regression: misplaced fields used to drop the whole config silently and surface as
    /// "no credentials". Parse errors must propagate.
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

    #[tokio::test]
    async fn load_propagates_theme_resolution_error() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [tui.theme]
                base = "no-such-theme"
            "#},
        );
        let err = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .expect_err("unknown theme name must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("no-such-theme"), "{msg}");
        assert!(
            msg.contains("not a built-in name") || msg.contains("failed to read"),
            "{msg}",
        );
    }

    // ── Config::load / effort resolution ──

    #[tokio::test]
    async fn load_effort_default_follows_model_ceiling() {
        for (model, expected) in [
            ("claude-opus-4-7", Some(Effort::Xhigh)),
            ("claude-opus-4-6", Some(Effort::High)),
            ("claude-sonnet-4-6", Some(Effort::High)),
            ("claude-sonnet-4-5", None),
            ("claude-haiku-4-5", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_MODEL", model)]);
            let config = temp_env::async_with_vars(vars, Config::load())
                .await
                .unwrap();
            assert_eq!(config.effort, expected, "model={model}");
        }
    }

    #[tokio::test]
    async fn load_effort_env_overrides_per_model_default() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-4-7"),
            env("ANTHROPIC_EFFORT", "low"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::Low));
    }

    #[tokio::test]
    async fn load_effort_clamps_xhigh_down_to_high_on_sonnet_4_6() {
        // Sonnet 4.6 has effort but not `xhigh` / `max`; must clamp, not 400 the gateway.
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-sonnet-4-6"),
            env("ANTHROPIC_EFFORT", "xhigh"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::High));
    }

    #[tokio::test]
    async fn load_effort_clamps_to_none_on_non_effort_capable_model() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-haiku-4-5"),
            env("ANTHROPIC_EFFORT", "max"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, None);
    }

    #[tokio::test]
    async fn load_effort_file_picks_up_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-opus-4-7"
                effort = "medium"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::Medium));
    }

    #[tokio::test]
    async fn load_effort_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-opus-4-7"
                effort = "low"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_EFFORT", "max")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.effort, Some(Effort::Max));
    }

    #[tokio::test]
    async fn load_effort_invalid_env_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_EFFORT", "insane")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid effort must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("ANTHROPIC_EFFORT"), "{msg}");
        assert!(msg.contains("insane"), "{msg}");
    }

    // ── Config::load / prompt_cache_ttl ──

    #[tokio::test]
    async fn load_prompt_cache_ttl_env_overrides_default() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_PROMPT_CACHE_TTL", "5m")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::FiveMin);
    }

    #[tokio::test]
    async fn load_prompt_cache_ttl_file_picks_up_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                prompt_cache_ttl = "5m"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::FiveMin);
    }

    #[tokio::test]
    async fn load_prompt_cache_ttl_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                prompt_cache_ttl = "5m"
            "#},
        );
        let vars = env_vars(vec![xdg(&dir), env("OX_PROMPT_CACHE_TTL", "1h")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.prompt_cache_ttl, PromptCacheTtl::OneHour);
    }

    #[tokio::test]
    async fn load_prompt_cache_ttl_invalid_env_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![xdg(&dir), env("OX_PROMPT_CACHE_TTL", "forever")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid ttl must propagate");
        let msg = format!("{err:#}");
        assert!(msg.contains("OX_PROMPT_CACHE_TTL"), "{msg}");
        assert!(msg.contains("forever"), "{msg}");
    }

    // ── Config::snapshot ──

    #[test]
    fn snapshot_copies_every_user_facing_field_and_drops_secret() {
        // `/config` prints from the snapshot; secret must reduce to `label()`.
        let cfg = Config {
            auth: Auth::OAuth("token-must-not-leak".to_owned()),
            base_url: "https://api.example.test".to_owned(),
            model: "claude-test-1-0".to_owned(),
            effort: Some(Effort::Xhigh),
            max_tokens: 64_000,
            prompt_cache_ttl: PromptCacheTtl::FiveMin,
            thinking: None,
            show_thinking: true,
            theme: Theme::default(),
        };
        let snap = cfg.snapshot();
        assert_eq!(snap.auth_label, "OAuth");
        assert_eq!(snap.base_url, "https://api.example.test");
        assert_eq!(snap.model_id, "claude-test-1-0");
        assert_eq!(snap.effort, Some(Effort::Xhigh));
        assert_eq!(snap.max_tokens, 64_000);
        assert_eq!(snap.prompt_cache_ttl, PromptCacheTtl::FiveMin);
        assert!(snap.show_thinking);
    }

    // ── display_effort ──

    #[test]
    fn display_effort_names_effective_tier_or_no_tier() {
        assert_eq!(display_effort(Some(Effort::High)), "high");
        assert_eq!(display_effort(None), "(no effort tier)");
    }

    // ── default_max_tokens ──

    #[test]
    fn default_max_tokens_scales_with_effort() {
        assert_eq!(default_max_tokens(Some(Effort::Max)), 64_000);
        assert_eq!(default_max_tokens(Some(Effort::Xhigh)), 64_000);
        assert_eq!(default_max_tokens(Some(Effort::High)), 32_000);
        assert_eq!(default_max_tokens(Some(Effort::Medium)), 16_000);
        assert_eq!(default_max_tokens(Some(Effort::Low)), 16_000);
        assert_eq!(default_max_tokens(None), 16_000);
    }
}
