//! Configuration loading.
//!
//! Precedence (highest wins): env > project `ox.toml` > user `~/.config/ox/config.toml` > defaults.
//! Auth stops at the first source that resolves (API key env > API key in file > OAuth).

pub(crate) mod file;
mod oauth;

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::tui::theme::{self, Theme};
use crate::util::env;
use crate::util::path::expand_user;

const DEFAULT_MODEL: &str = "claude-opus-4-7[1m]";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const AUTO_COMPACTION_OUTPUT_RESERVE_CAP: u32 = 20_000;
const AUTO_COMPACTION_BUFFER_TOKENS: u32 = 13_000;
const MIN_AUTO_COMPACTION_THRESHOLD_TOKENS: u32 = 50_000;
/// Mirrors the fallback `loader::resolve_theme` applies when no `[tui.theme] base` is set.
pub(crate) const DEFAULT_THEME: &str = "mocha";

// ── Auth ──

/// Resolved credential. First source wins, in order: `ANTHROPIC_API_KEY` env, `client.api_key`
/// in the user config file, then OAuth (macOS Keychain → `~/.claude/.credentials.json`).
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
    /// User-config path appended to reqwest's trust anchors; `None` when unset.
    pub(crate) extra_ca_certs: Option<PathBuf>,
    pub(crate) max_tokens: u32,
    /// `None` means the agent loop runs without a per-turn round cap.
    pub(crate) max_tool_rounds: Option<u32>,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) compaction: CompactionConfig,
    pub(crate) show_thinking: bool,
    pub(crate) show_welcome: bool,
    /// Resolved theme base name — built-in catalogue key or filesystem path. `/theme` reads this
    /// to mark the active row in the picker.
    pub(crate) theme_name: String,
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

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// One-line UX hint for the typed-arg popup.
    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Low => "Fastest, shallow reasoning",
            Self::Medium => "Balanced",
            Self::High => "Deep reasoning",
            Self::Xhigh => "Extended thinking",
            Self::Max => "Maximum thinking",
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

// ── CompactionConfig ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionConfig {
    pub(crate) auto: AutoCompactionConfig,
    policy: AutoCompactionPolicy,
}

impl CompactionConfig {
    #[cfg(test)]
    pub(crate) const fn disabled() -> Self {
        Self {
            auto: AutoCompactionConfig::disabled(),
            policy: AutoCompactionPolicy::Disabled,
        }
    }

    pub(crate) fn for_model(self, model: &str, max_tokens: u32) -> Result<Self> {
        resolve_compaction_policy(self.policy, model, max_tokens)
    }

    #[cfg(test)]
    pub(crate) const fn resolved_for_test(auto: AutoCompactionConfig) -> Self {
        let policy = if auto.enabled {
            match auto.threshold_tokens {
                Some(tokens) => AutoCompactionPolicy::Tokens(tokens),
                None => AutoCompactionPolicy::Default,
            }
        } else {
            AutoCompactionPolicy::Disabled
        };
        Self { auto, policy }
    }

    #[cfg(test)]
    pub(crate) fn default_for_test(model: &str, max_tokens: u32) -> Self {
        resolve_compaction_policy(AutoCompactionPolicy::Default, model, max_tokens).unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoCompactionConfig {
    pub(crate) enabled: bool,
    pub(crate) threshold_tokens: Option<u32>,
}

impl AutoCompactionConfig {
    pub(crate) const fn disabled() -> Self {
        Self {
            enabled: false,
            threshold_tokens: None,
        }
    }

    pub(crate) const fn should_trigger(self, total_tokens: u32) -> bool {
        match (self.enabled, self.threshold_tokens) {
            (true, Some(threshold)) => total_tokens >= threshold,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCompactionPolicy {
    Disabled,
    Default,
    Tokens(u32),
    Percent(u8),
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
    /// Absolute path to a PEM bundle appended to reqwest's trust anchors. `None` means the
    /// client keeps only the built-in Mozilla roots.
    pub(crate) extra_ca_certs: Option<PathBuf>,
    pub(crate) max_tokens: u32,
    /// `None` means the agent loop runs without a per-turn round cap.
    pub(crate) max_tool_rounds: Option<u32>,
    pub(crate) prompt_cache_ttl: PromptCacheTtl,
    pub(crate) compaction: CompactionConfig,
    pub(crate) thinking: Option<ThinkingConfig>,
    pub(crate) show_thinking: bool,
    pub(crate) show_welcome: bool,
    pub(crate) theme: Theme,
    /// Built-in catalogue key (e.g. `"mocha"`) or filesystem path; mirrors `[tui.theme] base`,
    /// falling back to [`DEFAULT_THEME`] when unset.
    pub(crate) theme_name: String,
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

        // Resolve before `auth` so the OAuth refresh can also thread the extra trust anchors
        // through reqwest (relevant under SSL-inspecting corporate proxies).
        let extra_ca_certs = env::string("OX_EXTRA_CA_CERTS")
            .or(client.extra_ca_certs)
            .map(|raw| expand_user(&raw))
            .transpose()
            .context("invalid client.extra_ca_certs")?;

        let auth = if let Some(key) = env::string("ANTHROPIC_API_KEY").or(client.api_key) {
            Auth::ApiKey(key)
        } else {
            let token = oauth::load_token(extra_ca_certs.as_deref()).await.context(
                "no credentials available: set ANTHROPIC_API_KEY, add `api_key` to \
                 ~/.config/ox/config.toml, or sign in with Claude Code (checks macOS Keychain and \
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
        validate_base_url(&base_url)?;

        let caps = crate::model::capabilities_for(&model);

        let effort_pick = match env::string("ANTHROPIC_EFFORT") {
            Some(raw) => Some(raw.parse::<Effort>().context("ANTHROPIC_EFFORT")?),
            None => client.effort,
        };
        let effort = caps.resolve_effort(effort_pick);

        let max_tokens = match env::string("ANTHROPIC_MAX_TOKENS") {
            Some(raw) => raw
                .parse::<u32>()
                .with_context(|| format!("ANTHROPIC_MAX_TOKENS={raw:?}"))?,
            None => client
                .max_tokens
                .unwrap_or_else(|| default_max_tokens(effort)),
        };

        let max_tool_rounds = match env::string("OX_MAX_TOOL_ROUNDS") {
            Some(raw) => Some(
                raw.parse::<u32>()
                    .with_context(|| format!("OX_MAX_TOOL_ROUNDS={raw:?}"))?,
            ),
            None => client.max_tool_rounds,
        };

        let show_thinking = env::bool("OX_SHOW_THINKING")
            .or(tui.show_thinking)
            .unwrap_or(false);

        let show_welcome = env::bool("OX_SHOW_WELCOME")
            .or(tui.show_welcome)
            .unwrap_or(true);

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

        let compaction = resolve_compaction(client.compaction, &model, max_tokens)?;

        let theme_name = theme_config
            .base
            .clone()
            .unwrap_or_else(|| DEFAULT_THEME.to_owned());
        let theme = theme::resolve_theme(
            theme_config.base.as_deref(),
            &theme_config.overrides.unwrap_or_default(),
        )?;

        Ok(Self {
            model,
            effort,
            auth,
            base_url,
            extra_ca_certs,
            max_tokens,
            max_tool_rounds,
            prompt_cache_ttl,
            compaction,
            thinking,
            show_thinking,
            show_welcome,
            theme,
            theme_name,
        })
    }

    /// Descriptors for `/config` and `/status`, minus the auth secret.
    pub(crate) fn snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            model_id: self.model.clone(),
            effort: self.effort,
            auth_label: self.auth.label(),
            base_url: self.base_url.clone(),
            extra_ca_certs: self.extra_ca_certs.clone(),
            max_tokens: self.max_tokens,
            max_tool_rounds: self.max_tool_rounds,
            prompt_cache_ttl: self.prompt_cache_ttl,
            compaction: self.compaction,
            show_thinking: self.show_thinking,
            show_welcome: self.show_welcome,
            theme_name: self.theme_name.clone(),
        }
    }
}

// ── Helpers ──

pub(crate) fn display_effort(effort: Option<Effort>) -> String {
    effort.map_or_else(|| "(no effort tier)".to_owned(), |e| e.to_string())
}

pub(crate) fn display_bool(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

pub(crate) fn display_max_tool_rounds(cap: Option<u32>) -> String {
    cap.map_or_else(|| "unbounded".to_owned(), |n| n.to_string())
}

pub(crate) fn display_auto_compaction(auto: AutoCompactionConfig) -> String {
    match (auto.enabled, auto.threshold_tokens) {
        (true, Some(threshold)) => format!("at {threshold} tokens"),
        (true, None) => "off (no threshold)".to_owned(),
        _ => "off".to_owned(),
    }
}

fn default_max_tokens(effort: Option<Effort>) -> u32 {
    match effort {
        Some(Effort::Xhigh | Effort::Max) => 64_000,
        Some(Effort::High) => 32_000,
        _ => 16_000,
    }
}

fn resolve_compaction(
    file: Option<file::CompactionConfig>,
    model: &str,
    max_tokens: u32,
) -> Result<CompactionConfig> {
    let auto_requested = env::bool("OX_COMPACTION_AUTO_ENABLED")
        .or_else(|| file.as_ref().and_then(|c| c.enabled))
        .unwrap_or(true);

    let policy = if auto_requested {
        resolve_auto_policy(file.as_ref())?
    } else {
        AutoCompactionPolicy::Disabled
    };
    resolve_compaction_policy(policy, model, max_tokens)
}

fn resolve_compaction_policy(
    policy: AutoCompactionPolicy,
    model: &str,
    max_tokens: u32,
) -> Result<CompactionConfig> {
    let auto = resolve_auto_compaction(policy, model, max_tokens)?;
    Ok(CompactionConfig { auto, policy })
}

fn resolve_auto_compaction(
    policy: AutoCompactionPolicy,
    model: &str,
    max_tokens: u32,
) -> Result<AutoCompactionConfig> {
    let threshold = match policy {
        AutoCompactionPolicy::Disabled => return Ok(AutoCompactionConfig::disabled()),
        AutoCompactionPolicy::Default => default_auto_threshold(model, max_tokens),
        AutoCompactionPolicy::Tokens(tokens) => {
            Some(threshold_from_tokens(tokens, model, max_tokens))
        }
        AutoCompactionPolicy::Percent(percent) => {
            threshold_from_percent(percent, model, max_tokens)?
        }
    };
    Ok(AutoCompactionConfig {
        enabled: threshold.is_some(),
        threshold_tokens: threshold,
    })
}

fn resolve_auto_policy(file: Option<&file::CompactionConfig>) -> Result<AutoCompactionPolicy> {
    let env_tokens = env_u32("OX_COMPACTION_AUTO_THRESHOLD_TOKENS")?;
    let env_percent = env_u8("OX_COMPACTION_AUTO_THRESHOLD_PERCENT")?;
    let env_threshold_set = env_tokens.is_some() || env_percent.is_some();
    let file_tokens = file.and_then(|c| c.threshold_tokens);
    let file_percent = file.and_then(|c| c.threshold_percent);
    let (tokens, percent) = if env_threshold_set {
        (env_tokens, env_percent)
    } else {
        (file_tokens, file_percent)
    };

    match (tokens, percent) {
        (Some(_), Some(_)) => {
            bail!("set only one of auto_threshold_tokens or auto_threshold_percent for compaction")
        }
        (Some(tokens), None) => Ok(AutoCompactionPolicy::Tokens(tokens)),
        (None, Some(percent)) => Ok(AutoCompactionPolicy::Percent(percent)),
        (None, None) => Ok(AutoCompactionPolicy::Default),
    }
}

fn threshold_from_tokens(tokens: u32, model: &str, max_tokens: u32) -> u32 {
    clamp_threshold_floor(clamp_threshold_ceil(tokens, model, max_tokens))
}

fn threshold_from_percent(percent: u8, model: &str, max_tokens: u32) -> Result<Option<u32>> {
    if !(1..=100).contains(&percent) {
        bail!("auto compaction threshold percent must be between 1 and 100");
    }
    let Some(context_window) = crate::model::context_window_for(model) else {
        return Ok(None);
    };
    let raw = context_window.saturating_mul(u32::from(percent)) / 100;
    Ok(Some(clamp_threshold_floor(clamp_threshold_ceil(
        raw, model, max_tokens,
    ))))
}

/// Snaps `tokens` down to the model's safe trigger when one is known.
fn clamp_threshold_ceil(tokens: u32, model: &str, max_tokens: u32) -> u32 {
    default_auto_threshold(model, max_tokens).map_or(tokens, |max| tokens.min(max))
}

/// Snaps `tokens` up to [`MIN_AUTO_COMPACTION_THRESHOLD_TOKENS`].
fn clamp_threshold_floor(tokens: u32) -> u32 {
    tokens.max(MIN_AUTO_COMPACTION_THRESHOLD_TOKENS)
}

fn default_auto_threshold(model: &str, max_tokens: u32) -> Option<u32> {
    crate::model::context_window_for(model)
        .and_then(|window| default_auto_threshold_for_window(window, max_tokens))
}

fn default_auto_threshold_for_window(context_window: u32, max_tokens: u32) -> Option<u32> {
    let reserve = max_tokens.min(AUTO_COMPACTION_OUTPUT_RESERVE_CAP);
    context_window
        .checked_sub(reserve)?
        .checked_sub(AUTO_COMPACTION_BUFFER_TOKENS)
}

fn env_u32(key: &'static str) -> Result<Option<u32>> {
    env::string(key)
        .map(|raw| raw.parse::<u32>().with_context(|| format!("{key}={raw:?}")))
        .transpose()
}

fn env_u8(key: &'static str) -> Result<Option<u8>> {
    env::string(key)
        .map(|raw| raw.parse::<u8>().with_context(|| format!("{key}={raw:?}")))
        .transpose()
}

fn validate_base_url(raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw).with_context(|| format!("invalid base URL {raw:?}"))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_url(&url) => Ok(()),
        "http" => bail!("base URL must use https unless it points at localhost"),
        scheme => bail!("base URL must use http or https, got {scheme:?}"),
    }
}

fn is_loopback_url(url: &reqwest::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    )
}

/// Outputs of [`default_auto_threshold`] when `max_tokens` saturates the reserve cap.
#[cfg(test)]
pub(crate) mod test_thresholds {
    pub(crate) const WINDOW_200K: u32 = 167_000;
    pub(crate) const WINDOW_1M: u32 = 967_000;
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
        "OX_EXTRA_CA_CERTS",
        "OX_MAX_TOOL_ROUNDS",
        "OX_COMPACTION_AUTO_ENABLED",
        "OX_COMPACTION_AUTO_THRESHOLD_PERCENT",
        "OX_COMPACTION_AUTO_THRESHOLD_TOKENS",
        "OX_SHOW_THINKING",
        "OX_SHOW_WELCOME",
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
        assert!(config.compaction.auto.enabled);
        assert_eq!(
            config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_1M),
        );
        assert!(!config.show_thinking);
        assert!(
            config.show_welcome,
            "default-on so the empty chat surfaces the welcome"
        );
        assert!(matches!(config.auth, Auth::ApiKey(k) if k == "sk-default"));
        assert_eq!(config.theme_name, DEFAULT_THEME);
    }

    #[tokio::test]
    async fn load_theme_name_reflects_user_picked_base() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [tui.theme]
                base = "latte"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.theme_name, "latte");
    }

    #[tokio::test]
    async fn load_env_overrides_every_client_field() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-4-7"),
            env("ANTHROPIC_BASE_URL", "https://example.invalid"),
            env("ANTHROPIC_MAX_TOKENS", "64"),
            env("OX_MAX_TOOL_ROUNDS", "200"),
            env("OX_SHOW_THINKING", "1"),
            env("OX_SHOW_WELCOME", "0"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, "claude-opus-4-7");
        assert_eq!(config.base_url, "https://example.invalid");
        assert_eq!(config.max_tokens, 64);
        assert_eq!(config.max_tool_rounds, Some(200));
        assert!(config.compaction.auto.enabled);
        assert!(config.show_thinking);
        assert!(
            !config.show_welcome,
            "env `0` flips the default-on welcome off"
        );
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
                max_tool_rounds = 75

                [tui]
                show_thinking = true
                show_welcome = false
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.base_url, "https://config-file.invalid");
        assert_eq!(config.max_tokens, 128);
        assert_eq!(config.max_tool_rounds, Some(75));
        assert!(config.compaction.auto.enabled);
        assert!(config.show_thinking);
        assert!(
            !config.show_welcome,
            "file `false` opts out of the default-on welcome"
        );
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
    async fn load_compaction_file_can_disable_default_on_auto_behavior() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_enabled = false
            "},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert!(!config.compaction.auto.enabled);
    }

    #[tokio::test]
    async fn load_compaction_auto_env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_enabled = false
            "},
        );
        let vars = env_vars(vec![xdg(&dir), env("OX_COMPACTION_AUTO_ENABLED", "1")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(config.compaction.auto.enabled);
    }

    #[tokio::test]
    async fn load_compaction_auto_threshold_tokens_sets_absolute_trigger() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_threshold_tokens = 400000
            "},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(400_000));
    }

    #[tokio::test]
    async fn load_compaction_auto_threshold_percent_uses_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-4-7[1m]"),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "40"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(400_000));
    }

    #[tokio::test]
    async fn load_compaction_rejects_ambiguous_auto_thresholds() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_threshold_tokens = 400000
                auto_threshold_percent = 40
            "},
        );
        let err = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .expect_err("ambiguous thresholds must fail config load");
        let msg = format!("{err:#}");
        assert!(msg.contains("only one"), "{msg}");
    }

    #[tokio::test]
    async fn load_compaction_clamps_zero_auto_threshold_tokens_up_to_floor() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("OX_COMPACTION_AUTO_THRESHOLD_TOKENS", "0"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn load_compaction_clamps_too_low_auto_threshold_tokens_up_to_floor() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r"
                [client.compaction]
                auto_threshold_tokens = 49999
            "},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn load_compaction_clamps_too_low_auto_threshold_percent_up_to_floor() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "claude-opus-4-7[1m]"),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "4"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.compaction.auto.threshold_tokens, Some(50_000));
    }

    #[tokio::test]
    async fn load_compaction_clamps_threshold_above_model_safe_window() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                model = "claude-sonnet-4-6"

                [client.compaction]
                auto_threshold_tokens = 400000
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.compaction.auto.threshold_tokens,
            Some(test_thresholds::WINDOW_200K),
        );
    }

    #[tokio::test]
    async fn load_compaction_rejects_out_of_range_auto_threshold_percent() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "101"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("out-of-range threshold percent must fail config load");
        let msg = format!("{err:#}");
        assert!(msg.contains("between 1 and 100"), "{msg}");
    }

    #[tokio::test]
    async fn load_compaction_percent_for_unknown_model_disables_auto_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_MODEL", "custom-model"),
            env("OX_COMPACTION_AUTO_THRESHOLD_PERCENT", "40"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert!(!config.compaction.auto.enabled);
        assert_eq!(config.compaction.auto.threshold_tokens, None);
    }

    #[tokio::test]
    async fn load_invalid_max_tokens_env_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {"
                [client]
                max_tokens = 128
            "},
        );
        let vars = env_vars(vec![xdg(&dir), env("ANTHROPIC_MAX_TOKENS", "not-a-number")]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("invalid max-token env should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("ANTHROPIC_MAX_TOKENS"), "{msg}");
        assert!(msg.contains("not-a-number"), "{msg}");
    }

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
        // Sonnet 4.6 has effort but caps below `xhigh`; clamping keeps the request from 400ing.
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

    // ── Config::load / base URL validation ──

    #[tokio::test]
    async fn load_rejects_plain_http_base_url_unless_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_BASE_URL", "http://example.com"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("non-loopback http must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("https"), "{msg}");
        assert!(msg.contains("localhost"), "{msg}");
    }

    #[tokio::test]
    async fn load_rejects_non_http_base_url_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_BASE_URL", "ftp://example.com"),
        ]);
        let err = temp_env::async_with_vars(vars, Config::load())
            .await
            .expect_err("non-http schemes must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("http or https"), "{msg}");
        assert!(msg.contains("ftp"), "{msg}");
    }

    #[tokio::test]
    async fn load_accepts_loopback_http_base_url_for_local_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let vars = env_vars(vec![
            xdg(&dir),
            env("ANTHROPIC_BASE_URL", "http://127.0.0.1:8080"),
        ]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:8080");
    }

    // ── Config::load / extra_ca_certs ──

    #[tokio::test]
    async fn load_extra_ca_certs_default_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert!(config.extra_ca_certs.is_none());
    }

    #[tokio::test]
    async fn load_extra_ca_certs_env_beats_file_and_expands_tilde() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                extra_ca_certs = "/etc/ssl/from-file.pem"
            "#},
        );
        // Env supplies a `~`-prefixed path so the expansion branch is also covered.
        let vars = env_vars(vec![xdg(&dir), env("OX_EXTRA_CA_CERTS", "~/certs/env.pem")]);
        let config = temp_env::async_with_vars(vars, Config::load())
            .await
            .unwrap();
        let home = dirs::home_dir().expect("HOME set");
        assert_eq!(config.extra_ca_certs, Some(home.join("certs/env.pem")));
    }

    #[tokio::test]
    async fn load_extra_ca_certs_file_used_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_user_config(
            dir.path(),
            indoc::indoc! {r#"
                [client]
                extra_ca_certs = "/etc/ssl/from-file.pem"
            "#},
        );
        let config = temp_env::async_with_vars(env_vars(vec![xdg(&dir)]), Config::load())
            .await
            .unwrap();
        assert_eq!(
            config.extra_ca_certs,
            Some(PathBuf::from("/etc/ssl/from-file.pem")),
        );
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
        // `/config` prints from the snapshot, so the secret must reduce to `label()`.
        let cfg = Config {
            auth: Auth::OAuth("token-must-not-leak".to_owned()),
            base_url: "https://api.example.test".to_owned(),
            extra_ca_certs: Some(PathBuf::from("/etc/ssl/corp-ca.pem")),
            model: "claude-test-1-0".to_owned(),
            effort: Some(Effort::Xhigh),
            max_tokens: 64_000,
            max_tool_rounds: Some(100),
            prompt_cache_ttl: PromptCacheTtl::FiveMin,
            compaction: CompactionConfig::resolved_for_test(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(42),
            }),
            thinking: None,
            show_thinking: true,
            show_welcome: false,
            theme: Theme::default(),
            theme_name: "macchiato".to_owned(),
        };
        let snap = cfg.snapshot();
        assert_eq!(snap.auth_label, "OAuth");
        assert_eq!(snap.base_url, "https://api.example.test");
        assert_eq!(
            snap.extra_ca_certs.as_deref(),
            Some(Path::new("/etc/ssl/corp-ca.pem")),
        );
        assert_eq!(snap.model_id, "claude-test-1-0");
        assert_eq!(snap.effort, Some(Effort::Xhigh));
        assert_eq!(snap.max_tokens, 64_000);
        assert_eq!(snap.max_tool_rounds, Some(100));
        assert_eq!(snap.prompt_cache_ttl, PromptCacheTtl::FiveMin);
        assert_eq!(snap.compaction.auto.threshold_tokens, Some(42));
        assert!(snap.show_thinking);
        assert!(!snap.show_welcome);
        assert_eq!(snap.theme_name, "macchiato");
    }

    // ── display_effort ──

    #[test]
    fn display_effort_names_effective_tier_or_no_tier() {
        assert_eq!(display_effort(Some(Effort::High)), "high");
        assert_eq!(display_effort(None), "(no effort tier)");
    }

    // ── display_bool ──

    #[test]
    fn display_bool_names_the_two_flag_states() {
        assert_eq!(display_bool(true), "on");
        assert_eq!(display_bool(false), "off");
    }

    // ── display_auto_compaction ──

    #[test]
    fn display_auto_compaction_names_enabled_threshold_or_off() {
        assert_eq!(
            display_auto_compaction(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(400_000),
            }),
            "at 400000 tokens",
        );
        assert_eq!(
            display_auto_compaction(AutoCompactionConfig {
                enabled: false,
                threshold_tokens: Some(400_000),
            }),
            "off",
        );
        assert_eq!(
            display_auto_compaction(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: None,
            }),
            "off (no threshold)",
        );
    }

    // ── AutoCompactionConfig::should_trigger ──

    #[test]
    fn should_trigger_requires_enabled_threshold_and_enough_tokens() {
        let enabled = AutoCompactionConfig {
            enabled: true,
            threshold_tokens: Some(10),
        };
        assert!(enabled.should_trigger(10));
        assert!(!enabled.should_trigger(9));
        assert!(!AutoCompactionConfig::disabled().should_trigger(100));
        assert!(
            !AutoCompactionConfig {
                enabled: true,
                threshold_tokens: None,
            }
            .should_trigger(100)
        );
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

    // ── test_thresholds ──

    #[test]
    fn test_thresholds_pin_default_auto_threshold_per_window() {
        for model in ["claude-sonnet-4-6", "claude-opus-4-7", "claude-haiku-4-5"] {
            assert_eq!(
                default_auto_threshold(model, 32_000),
                Some(test_thresholds::WINDOW_200K),
                "{model}",
            );
        }
        assert_eq!(
            default_auto_threshold("claude-opus-4-7[1m]", 64_000),
            Some(test_thresholds::WINDOW_1M),
        );
    }
}
