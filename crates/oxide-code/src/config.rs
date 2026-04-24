//! Configuration loading.
//!
//! Layered precedence (highest wins): env vars > project `ox.toml` >
//! user `~/.config/ox/config.toml` > built-in defaults. Auth follows
//! the same precedence but terminates at the first source that
//! resolves (API key env > API key in file > OAuth credentials).

mod file;
mod oauth;

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
    /// Model decides the thinking budget (Claude 4.6+). `display`
    /// controls what the API streams back: `Omitted` (4.7 default,
    /// empty `thinking` field) or `Summarized` (the 4.6 default, and
    /// what oxide-code enables whenever `show_thinking=true`).
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
}

/// `thinking.display` values accepted by the API on 4.7+. Only
/// `Summarized` is ever emitted — omitting the field entirely (via
/// `display: None`) already yields the `omitted` default on 4.7.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Summarized,
}

/// Intelligence-vs-latency tier sent as `output_config.effort` on
/// effort-capable models. The per-model ceiling lives in
/// [`crate::model::Capabilities`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
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
            _ => bail!("invalid effort {s:?}; expected one of: low, medium, high, xhigh, max"),
        }
    }
}

/// Prompt-cache TTL sent as `cache_control.ttl`. Anthropic silently
/// dropped the default from 1h to 5m on 2026-03-06, so `OneHour` is
/// explicit opt-in. oxide-code defaults to `OneHour`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum PromptCacheTtl {
    #[serde(rename = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    OneHour,
}

impl PromptCacheTtl {
    /// Wire value for `cache_control.ttl`. `None` when the TTL is
    /// the server default (5 m) so the JSON omits the field entirely.
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

/// Resolved configuration. Fields are grouped by concern so adjacent
/// lines stay related: connection (`auth`, `base_url`), model selection
/// (`model`, `effort`), request tuning (`max_tokens`,
/// `prompt_cache_ttl`, `thinking`), then display (`show_thinking`).
#[derive(Debug, Clone)]
pub struct Config {
    pub auth: Auth,
    pub base_url: String,
    pub model: String,
    /// `output_config.effort` for the streaming path. `None` means
    /// the model doesn't accept the parameter and the field is
    /// omitted. Resolved once at [`Config::load`] — callers forward.
    pub effort: Option<Effort>,
    pub max_tokens: u32,
    /// `cache_control.ttl` for every cacheable block. Default is
    /// [`PromptCacheTtl::OneHour`] since Anthropic's 2026-03 TTL
    /// drop made the server default (5 m) a silent cost regression
    /// on long sessions.
    pub prompt_cache_ttl: PromptCacheTtl,
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

        let caps = crate::model::capabilities_for(&model);

        let effort_pick = match env::string("ANTHROPIC_EFFORT") {
            Some(raw) => Some(raw.parse::<Effort>().context("ANTHROPIC_EFFORT")?),
            None => client.effort,
        };
        let effort = match effort_pick {
            Some(pick) => caps.clamp_effort(pick),
            None => caps.default_effort(),
        };

        let max_tokens = env::string("ANTHROPIC_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .or(client.max_tokens)
            .unwrap_or_else(|| default_max_tokens(effort));

        let show_thinking = env::bool("OX_SHOW_THINKING")
            .or(tui.show_thinking)
            .unwrap_or(false);

        // Adaptive thinking is always enabled — the model decides the
        // budget. `display` opts 4.7 into streaming summarized thinking
        // text (its default changed to `omitted` silently); 4.6 and
        // older ignore the field.
        let thinking = Some(ThinkingConfig::Adaptive {
            display: show_thinking.then_some(ThinkingDisplay::Summarized),
        });

        let prompt_cache_ttl = match env::string("OX_PROMPT_CACHE_TTL") {
            Some(raw) => raw
                .parse::<PromptCacheTtl>()
                .context("OX_PROMPT_CACHE_TTL")?,
            None => client.prompt_cache_ttl.unwrap_or(PromptCacheTtl::OneHour),
        };

        Ok(Self {
            auth,
            base_url,
            model,
            effort,
            max_tokens,
            prompt_cache_ttl,
            thinking,
            show_thinking,
        })
    }
}

/// Per-effort `max_tokens` default. Matches claude-code 2.1.119's
/// observed values: 64 K for the top two tiers (xhigh / max), 32 K
/// for high, the legacy 16 384 for everything else. Users override
/// via `ANTHROPIC_MAX_TOKENS` / `[client].max_tokens`.
fn default_max_tokens(effort: Option<Effort>) -> u32 {
    match effort {
        Some(Effort::Xhigh | Effort::Max) => 64_000,
        Some(Effort::High) => 32_000,
        _ => DEFAULT_MAX_TOKENS,
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
    fn thinking_config_adaptive_without_display_serializes_bare() {
        // Older models ignore `display`; absence keeps the wire as
        // pre-4.7 clients expect.
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
    fn effort_serialize_matches_wire_tokens() {
        for (variant, wire) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), wire);
            assert_eq!(variant.to_string(), wire);
        }
    }

    #[test]
    fn effort_parses_all_valid_tokens() {
        for (token, expected) in [
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::Xhigh),
            ("max", Effort::Max),
        ] {
            assert_eq!(token.parse::<Effort>().unwrap(), expected);
        }
    }

    #[test]
    fn effort_rejects_unknown_tokens_with_actionable_error() {
        let err = "extra-high".parse::<Effort>().expect_err("unknown token");
        let msg = format!("{err:#}");
        assert!(msg.contains("extra-high"), "names the input: {msg}");
        for token in ["low", "medium", "high", "xhigh", "max"] {
            assert!(msg.contains(token), "lists {token}: {msg}");
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
        // 5m is the server default → field omitted. 1h opts in → "1h".
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

    /// Env keys `Config::load` reads. Baseline for [`env_vars`] so
    /// nothing bleeds in from the caller's environment; `ANTHROPIC_API_KEY`
    /// ships with a non-empty default so tests land on the `ApiKey` arm
    /// and never consult the real OAuth credential sources.
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
        // Default model (Opus 4.7) supports `xhigh`, so both `effort`
        // and `max_tokens` derive from that ceiling — matches the
        // claude-code 2.1.119 packet capture. Prompt cache defaults
        // to 1h (opt-out via `OX_PROMPT_CACHE_TTL=5m`).
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
        // Sonnet 4.6 supports `effort` but not `xhigh` / `max` — the
        // user's pick must clamp rather than 400 the gateway.
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

    // ── default_max_tokens ──

    #[test]
    fn default_max_tokens_scales_with_effort() {
        assert_eq!(default_max_tokens(Some(Effort::Max)), 64_000);
        assert_eq!(default_max_tokens(Some(Effort::Xhigh)), 64_000);
        assert_eq!(default_max_tokens(Some(Effort::High)), 32_000);
        assert_eq!(default_max_tokens(Some(Effort::Medium)), DEFAULT_MAX_TOKENS);
        assert_eq!(default_max_tokens(Some(Effort::Low)), DEFAULT_MAX_TOKENS);
        assert_eq!(default_max_tokens(None), DEFAULT_MAX_TOKENS);
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
}
