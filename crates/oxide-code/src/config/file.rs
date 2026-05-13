//! TOML config file discovery, parsing, and layered merge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::debug;

use crate::tui::theme::SlotPatch;
use crate::util::path::xdg_dir;

const USER_CONFIG_DIR: &str = "ox";
const USER_CONFIG_FILENAME: &str = "config.toml";
const PROJECT_CONFIG_FILENAME: &str = "ox.toml";

// ── Config Structs ──

/// All fields optional; merge via [`FileConfig::merge`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileConfig {
    pub(super) client: Option<ClientConfig>,
    pub(super) tui: Option<TuiConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientConfig {
    // Any new field that influences credentials, endpoint identity, or TLS trust must also be
    // listed in `reject_project_secrets` so a checked-in `ox.toml` cannot set it.
    pub(super) api_key: Option<String>,
    pub(super) base_url: Option<String>,
    pub(super) extra_ca_certs: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<super::Effort>,
    pub(super) max_tokens: Option<u32>,
    pub(super) max_tool_rounds: Option<u32>,
    pub(super) prompt_cache_ttl: Option<super::PromptCacheTtl>,
    pub(super) compaction: Option<CompactionConfig>,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompactionConfig {
    #[serde(rename = "auto_enabled")]
    pub(super) enabled: Option<bool>,
    #[serde(rename = "auto_threshold_tokens")]
    pub(super) threshold_tokens: Option<u32>,
    #[serde(rename = "auto_threshold_percent")]
    pub(super) threshold_percent: Option<u8>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TuiConfig {
    pub(super) show_thinking: Option<bool>,
    pub(super) show_welcome: Option<bool>,
    pub(super) theme: Option<ThemeFileConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ThemeFileConfig {
    pub(super) base: Option<String>,
    pub(super) overrides: Option<HashMap<String, SlotPatch>>,
}

// ── Merge ──

impl FileConfig {
    /// Fields in `other` take precedence over `self`.
    fn merge(self, other: Self) -> Self {
        Self {
            client: merge_section(self.client, other.client, ClientConfig::merge),
            tui: merge_section(self.tui, other.tui, TuiConfig::merge),
        }
    }
}

impl ClientConfig {
    fn merge(self, other: Self) -> Self {
        Self {
            api_key: other.api_key.or(self.api_key),
            base_url: other.base_url.or(self.base_url),
            extra_ca_certs: other.extra_ca_certs.or(self.extra_ca_certs),
            model: other.model.or(self.model),
            effort: other.effort.or(self.effort),
            max_tokens: other.max_tokens.or(self.max_tokens),
            max_tool_rounds: other.max_tool_rounds.or(self.max_tool_rounds),
            prompt_cache_ttl: other.prompt_cache_ttl.or(self.prompt_cache_ttl),
            compaction: merge_section(self.compaction, other.compaction, CompactionConfig::merge),
        }
    }
}

impl CompactionConfig {
    fn merge(self, other: Self) -> Self {
        let other_sets_threshold =
            other.threshold_tokens.is_some() || other.threshold_percent.is_some();
        let (threshold_tokens, threshold_percent) = if other_sets_threshold {
            (other.threshold_tokens, other.threshold_percent)
        } else {
            (self.threshold_tokens, self.threshold_percent)
        };

        Self {
            enabled: other.enabled.or(self.enabled),
            threshold_tokens,
            threshold_percent,
        }
    }
}

impl TuiConfig {
    fn merge(self, other: Self) -> Self {
        Self {
            show_thinking: other.show_thinking.or(self.show_thinking),
            show_welcome: other.show_welcome.or(self.show_welcome),
            theme: merge_section(self.theme, other.theme, ThemeFileConfig::merge),
        }
    }
}

impl ThemeFileConfig {
    /// `base`: other wins. `overrides`: merged key-by-key, `other` wins collisions.
    fn merge(self, other: Self) -> Self {
        let overrides = match (self.overrides, other.overrides) {
            (Some(mut s), Some(o)) => {
                s.extend(o);
                Some(s)
            }
            (s, o) => o.or(s),
        };
        Self {
            base: other.base.or(self.base),
            overrides,
        }
    }
}

fn merge_section<T>(base: Option<T>, other: Option<T>, merge: fn(T, T) -> T) -> Option<T> {
    match (base, other) {
        (Some(b), Some(o)) => Some(merge(b, o)),
        (base, other) => other.or(base),
    }
}

// ── Loading ──

/// Loads + merges user and project TOML. Project config wins for non-secret fields; credential
/// and endpoint settings are user/env-only so a checkout cannot redirect secrets via `ox.toml`.
pub(super) fn load() -> Result<FileConfig> {
    let user = user_config_path()
        .map(|p| load_file(&p))
        .transpose()?
        .flatten();
    let project = find_project_config()
        .map(|p| load_project_file(&p))
        .transpose()?
        .flatten();

    let base = user.unwrap_or_default();
    Ok(match project {
        Some(p) => base.merge(p),
        None => base,
    })
}

fn load_project_file(path: &Path) -> Result<Option<FileConfig>> {
    let config = load_file(path)?;
    if let Some(config) = &config {
        reject_project_secrets(config, path)?;
    }
    Ok(config)
}

fn reject_project_secrets(config: &FileConfig, path: &Path) -> Result<()> {
    let Some(client) = &config.client else {
        return Ok(());
    };

    let mut blocked = Vec::new();
    if client.api_key.is_some() {
        blocked.push("client.api_key");
    }
    if client.base_url.is_some() {
        blocked.push("client.base_url");
    }
    if client.extra_ca_certs.is_some() {
        blocked.push("client.extra_ca_certs");
    }
    if blocked.is_empty() {
        return Ok(());
    }

    bail!(
        "{} cannot set {}; move credential and endpoint settings to ~/.config/ox/config.toml or environment variables",
        path.display(),
        blocked.join(", "),
    )
}

/// `Ok(None)` when missing; `Err` when present but unreadable or malformed.
fn load_file(path: &Path) -> Result<Option<FileConfig>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read config at {}", path.display()));
        }
    };
    let config = toml::from_str(&content)
        .with_context(|| format!("invalid config at {}", path.display()))?;
    debug!("loaded config from {}", path.display());
    Ok(Some(config))
}

// ── Path Discovery ──

pub(crate) fn user_config_path() -> Option<PathBuf> {
    xdg_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
        Path::new(".config"),
        &Path::new(USER_CONFIG_DIR).join(USER_CONFIG_FILENAME),
    )
}

/// Walks from CWD upward to find the nearest `ox.toml`.
pub(crate) fn find_project_config() -> Option<PathBuf> {
    find_project_config_from(std::env::current_dir().ok()?)
}

fn find_project_config_from(mut dir: PathBuf) -> Option<PathBuf> {
    loop {
        let candidate = dir.join(PROJECT_CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    // ── FileConfig::merge ──

    #[test]
    fn merge_other_wins_when_both_set() {
        let base = FileConfig {
            client: Some(ClientConfig {
                api_key: Some("base-key".to_owned()),
                base_url: Some("https://base.example.com".to_owned()),
                extra_ca_certs: Some("/etc/ssl/base.pem".to_owned()),
                model: Some("base-model".to_owned()),
                effort: Some(super::super::Effort::Low),
                max_tokens: Some(1000),
                max_tool_rounds: Some(50),
                prompt_cache_ttl: Some(super::super::PromptCacheTtl::FiveMin),
                compaction: Some(CompactionConfig {
                    enabled: Some(false),
                    threshold_tokens: Some(400_000),
                    threshold_percent: None,
                }),
            }),
            tui: Some(TuiConfig {
                show_thinking: Some(false),
                show_welcome: None,
                theme: None,
            }),
        };
        let other = FileConfig {
            client: Some(ClientConfig {
                api_key: Some("other-key".to_owned()),
                base_url: Some("https://other.example.com".to_owned()),
                extra_ca_certs: Some("/etc/ssl/other.pem".to_owned()),
                model: Some("other-model".to_owned()),
                effort: Some(super::super::Effort::Max),
                max_tokens: Some(2000),
                max_tool_rounds: Some(100),
                prompt_cache_ttl: Some(super::super::PromptCacheTtl::OneHour),
                compaction: Some(CompactionConfig {
                    enabled: Some(true),
                    threshold_tokens: None,
                    threshold_percent: Some(40),
                }),
            }),
            tui: Some(TuiConfig {
                show_thinking: Some(true),
                show_welcome: None,
                theme: None,
            }),
        };
        let merged = base.merge(other);

        let client = merged.client.expect("client section should be present");
        assert_eq!(client.api_key.as_deref(), Some("other-key"));
        assert_eq!(
            client.base_url.as_deref(),
            Some("https://other.example.com")
        );
        assert_eq!(client.extra_ca_certs.as_deref(), Some("/etc/ssl/other.pem"));
        assert_eq!(client.model.as_deref(), Some("other-model"));
        assert_eq!(client.effort, Some(super::super::Effort::Max));
        assert_eq!(client.max_tokens, Some(2000));
        assert_eq!(client.max_tool_rounds, Some(100));
        assert_eq!(
            client.prompt_cache_ttl,
            Some(super::super::PromptCacheTtl::OneHour)
        );
        let compaction = client.compaction.expect("compaction section should merge");
        assert_eq!(compaction.enabled, Some(true));
        assert_eq!(compaction.threshold_tokens, None);
        assert_eq!(compaction.threshold_percent, Some(40));

        let tui = merged.tui.expect("tui section should be present");
        assert_eq!(tui.show_thinking, Some(true));
    }

    #[test]
    fn merge_compaction_enabled_does_not_clear_base_threshold() {
        let base = CompactionConfig {
            enabled: Some(false),
            threshold_tokens: Some(400_000),
            threshold_percent: None,
        };
        let other = CompactionConfig {
            enabled: Some(true),
            threshold_tokens: None,
            threshold_percent: None,
        };
        let merged = base.merge(other);

        assert_eq!(merged.enabled, Some(true));
        assert_eq!(merged.threshold_tokens, Some(400_000));
        assert_eq!(merged.threshold_percent, None);
    }

    #[test]
    fn merge_falls_back_to_base_when_other_is_none() {
        let base = FileConfig {
            client: Some(ClientConfig {
                api_key: Some("key".to_owned()),
                base_url: Some("https://example.com".to_owned()),
                extra_ca_certs: Some("/etc/ssl/ca.pem".to_owned()),
                model: Some("model".to_owned()),
                effort: Some(super::super::Effort::High),
                max_tokens: Some(4096),
                max_tool_rounds: Some(75),
                prompt_cache_ttl: Some(super::super::PromptCacheTtl::FiveMin),
                compaction: Some(CompactionConfig {
                    enabled: Some(false),
                    threshold_tokens: Some(400_000),
                    threshold_percent: None,
                }),
            }),
            tui: Some(TuiConfig {
                show_thinking: Some(true),
                show_welcome: None,
                theme: None,
            }),
        };
        let merged = base.merge(FileConfig::default());

        let client = merged.client.expect("client section should survive");
        assert_eq!(client.api_key.as_deref(), Some("key"));
        assert_eq!(client.base_url.as_deref(), Some("https://example.com"));
        assert_eq!(client.extra_ca_certs.as_deref(), Some("/etc/ssl/ca.pem"));
        assert_eq!(client.model.as_deref(), Some("model"));
        assert_eq!(client.effort, Some(super::super::Effort::High));
        assert_eq!(client.max_tokens, Some(4096));
        assert_eq!(client.max_tool_rounds, Some(75));
        assert_eq!(
            client.prompt_cache_ttl,
            Some(super::super::PromptCacheTtl::FiveMin)
        );
        let compaction = client
            .compaction
            .expect("compaction section should survive");
        assert_eq!(compaction.enabled, Some(false));
        assert_eq!(compaction.threshold_tokens, Some(400_000));

        let tui = merged.tui.expect("tui section should survive");
        assert_eq!(tui.show_thinking, Some(true));
    }

    #[test]
    fn merge_cross_section_fills_gaps() {
        let base = FileConfig {
            client: Some(ClientConfig {
                model: Some("base-model".to_owned()),
                ..Default::default()
            }),
            tui: None,
        };
        let other = FileConfig {
            client: None,
            tui: Some(TuiConfig {
                show_thinking: Some(true),
                show_welcome: None,
                theme: None,
            }),
        };
        let merged = base.merge(other);

        let client = merged.client.expect("client from base should survive");
        assert_eq!(client.model.as_deref(), Some("base-model"));

        let tui = merged.tui.expect("tui from other should survive");
        assert_eq!(tui.show_thinking, Some(true));
    }

    #[test]
    fn merge_both_empty_produces_empty() {
        let merged = FileConfig::default().merge(FileConfig::default());
        assert!(merged.client.is_none());
        assert!(merged.tui.is_none());
    }

    // ── ThemeFileConfig::merge ──

    fn theme_with(base: Option<&str>, overrides: &[(&str, &str)]) -> ThemeFileConfig {
        ThemeFileConfig {
            base: base.map(str::to_owned),
            overrides: (!overrides.is_empty()).then(|| {
                overrides
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), SlotPatch::Bare((*v).to_owned())))
                    .collect()
            }),
        }
    }

    #[test]
    fn theme_merge_other_base_wins_over_self() {
        let base = theme_with(Some("mocha"), &[]);
        let other = theme_with(Some("latte"), &[]);
        let merged = base.merge(other);
        assert_eq!(merged.base.as_deref(), Some("latte"));
    }

    #[test]
    fn theme_merge_overrides_extend_when_both_set() {
        let base = theme_with(None, &[("error", "#aaaaaa")]);
        let other = theme_with(None, &[("accent", "#bbbbbb")]);
        let merged = base.merge(other);
        let map = merged.overrides.expect("merged overrides present");
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("error"), "user-level slot survives");
        assert!(map.contains_key("accent"), "project-level slot lands");
    }

    #[test]
    fn theme_merge_other_override_wins_on_slot_collision() {
        let base = theme_with(None, &[("error", "#aaaaaa")]);
        let other = theme_with(None, &[("error", "#bbbbbb")]);
        let merged = base.merge(other);
        let map = merged.overrides.expect("merged overrides present");
        let patch = map.get("error").expect("error patch present");
        assert!(
            matches!(patch, SlotPatch::Bare(value) if value == "#bbbbbb"),
            "project patch wins on collision; got {patch:?}",
        );
    }

    #[test]
    fn theme_merge_overrides_pass_through_when_one_side_is_none() {
        let base = theme_with(None, &[("error", "#aaaaaa")]);
        let other = ThemeFileConfig::default();
        let merged = base.merge(other);
        let map = merged.overrides.expect("base overrides survive");
        assert!(map.contains_key("error"));
    }

    // ── load_file ──

    #[test]
    fn load_file_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            indoc! {r##"
                [client]
                api_key = "sk-test"
                model = "claude-test"
                base_url = "https://test.example.com"
                max_tokens = 4096
                max_tool_rounds = 100

                [tui]
                show_thinking = true

                [tui.theme]
                base = "latte"

                [tui.theme.overrides]
                error = "#ff0000"
                accent = { bold = false }
            "##},
        )
        .unwrap();

        let config = load_file(&path)
            .expect("should parse valid TOML")
            .expect("file should exist");

        let client = config.client.expect("client section should be present");
        assert_eq!(client.api_key.as_deref(), Some("sk-test"));
        assert_eq!(client.model.as_deref(), Some("claude-test"));
        assert_eq!(client.base_url.as_deref(), Some("https://test.example.com"));
        assert_eq!(client.max_tokens, Some(4096));
        assert_eq!(client.max_tool_rounds, Some(100));

        let tui = config.tui.expect("tui section should be present");
        assert_eq!(tui.show_thinking, Some(true));

        let theme = tui.theme.expect("theme section should be present");
        assert_eq!(theme.base.as_deref(), Some("latte"));
        let overrides = theme.overrides.expect("overrides should parse");
        assert!(overrides.contains_key("error"));
        assert!(overrides.contains_key("accent"));
    }

    #[test]
    fn load_file_single_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            indoc! {r#"
                [client]
                model = "claude-test"
            "#},
        )
        .unwrap();

        let config = load_file(&path)
            .expect("should parse partial TOML")
            .expect("file should exist");
        assert_eq!(
            config
                .client
                .expect("client section should be present")
                .model
                .as_deref(),
            Some("claude-test")
        );
        assert!(config.tui.is_none());
    }

    #[test]
    fn load_file_empty_toml_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();

        let config = load_file(&path)
            .expect("empty TOML is valid")
            .expect("file should exist");
        assert!(config.client.is_none());
        assert!(config.tui.is_none());
    }

    #[test]
    fn load_file_missing_is_absent() {
        let result =
            load_file(Path::new("/nonexistent/config.toml")).expect("missing file is not an error");
        assert!(result.is_none());
    }

    /// Non-`NotFound` IO errors (e.g. directory → `IsADirectory`) must surface with the path.
    #[test]
    fn load_file_unreadable_path_propagates_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_file(dir.path()).expect_err("directory read should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!(
                "failed to read config at {}",
                dir.path().display()
            )),
            "{msg}",
        );
    }

    #[test]
    fn load_file_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[invalid").unwrap();

        let err = load_file(&path).expect_err("malformed TOML should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid config at"), "{msg}");
    }

    /// Covers top-level `deny_unknown_fields`. Section-level: `load_file_rejects_misplaced_field`.
    #[test]
    fn load_file_rejects_unknown_top_level_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, r#"model = "misplaced""#).unwrap();

        let err = load_file(&path).expect_err("unknown key should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid config at"), "{msg}");
        assert!(msg.contains("unknown field `model`"), "{msg}");
    }

    /// Covers section-level `deny_unknown_fields` (top-level: `load_file_rejects_unknown_top_level_key`).
    #[test]
    fn load_file_rejects_misplaced_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            indoc! {r#"
                [client]
                api_key = "sk-test"
                show_thinking = true
            "#},
        )
        .unwrap();

        let err = load_file(&path).expect_err("misplaced field should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown field `show_thinking`"), "{msg}");
    }

    // ── load_project_file ──

    #[test]
    fn load_project_file_rejects_trust_establishing_client_fields() {
        // `api_key`, `base_url`, and `extra_ca_certs` all influence who receives or is trusted
        // to be the server, so a checked-in `ox.toml` cannot set them.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROJECT_CONFIG_FILENAME);
        std::fs::write(
            &path,
            indoc! {r#"
                [client]
                api_key = "sk-project"
                base_url = "https://capture.invalid"
                extra_ca_certs = "./attacker-ca.pem"
            "#},
        )
        .unwrap();

        let err = load_project_file(&path).expect_err("project secrets must be blocked");
        let msg = format!("{err:#}");
        assert!(msg.contains("client.api_key"), "{msg}");
        assert!(msg.contains("client.base_url"), "{msg}");
        assert!(msg.contains("client.extra_ca_certs"), "{msg}");
        assert!(msg.contains("~/.config/ox/config.toml"), "{msg}");
    }

    #[test]
    fn load_project_file_allows_non_secret_client_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROJECT_CONFIG_FILENAME);
        std::fs::write(
            &path,
            indoc! {r#"
                [client]
                model = "claude-sonnet-4-6"
                max_tokens = 8192
            "#},
        )
        .unwrap();

        let config = load_project_file(&path)
            .expect("project settings should parse")
            .expect("file exists");
        let client = config.client.expect("client section present");
        assert_eq!(client.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(client.max_tokens, Some(8192));
    }

    #[test]
    fn load_project_file_allows_tui_only_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROJECT_CONFIG_FILENAME);
        std::fs::write(
            &path,
            indoc! {"
                [tui]
                show_welcome = false
            "},
        )
        .unwrap();

        let config = load_project_file(&path)
            .expect("project UI settings should parse")
            .expect("file exists");
        assert!(config.client.is_none());
        assert_eq!(config.tui.unwrap().show_welcome, Some(false));
    }

    // ── find_project_config_from ──

    #[test]
    fn find_project_config_from_in_start_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(PROJECT_CONFIG_FILENAME);
        std::fs::write(
            &config_path,
            indoc! {r#"
                [client]
                model = "test"
            "#},
        )
        .unwrap();

        let result = find_project_config_from(dir.path().to_path_buf());
        assert_eq!(result, Some(config_path));
    }

    #[test]
    fn find_project_config_from_walks_upward() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(PROJECT_CONFIG_FILENAME);
        std::fs::write(
            &config_path,
            indoc! {r#"
                [client]
                model = "test"
            "#},
        )
        .unwrap();
        let child = dir.path().join("sub").join("deep");
        std::fs::create_dir_all(&child).unwrap();

        let result = find_project_config_from(child);
        assert_eq!(result, Some(config_path));
    }

    #[test]
    fn find_project_config_from_is_absent_when_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_project_config_from(dir.path().to_path_buf()).is_none());
    }
}
