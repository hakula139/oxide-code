use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::debug;

use crate::util::path::xdg_dir;

const USER_CONFIG_DIR: &str = "ox";
const USER_CONFIG_FILENAME: &str = "config.toml";
const PROJECT_CONFIG_FILENAME: &str = "ox.toml";

// ── Config Structs ──

/// Top-level configuration loaded from a TOML file.
///
/// All sections and fields are optional — each source contributes only the
/// values it sets. Higher-priority sources override lower-priority ones
/// field by field via [`FileConfig::merge`].
///
/// ```toml
/// [client]
/// model = "claude-sonnet-4-6"
///
/// [tui]
/// show_thinking = true
/// ```
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileConfig {
    pub client: Option<ClientConfig>,
    pub tui: Option<TuiConfig>,
}

/// API client settings (`[client]` section).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientConfig {
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub effort: Option<super::Effort>,
    pub prompt_cache_ttl: Option<super::PromptCacheTtl>,
}

/// Terminal UI settings (`[tui]` section).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TuiConfig {
    pub show_thinking: Option<bool>,
}

// ── Merge ──

impl FileConfig {
    /// Merges two configs. Fields in `other` take precedence over `self`.
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
            model: other.model.or(self.model),
            base_url: other.base_url.or(self.base_url),
            max_tokens: other.max_tokens.or(self.max_tokens),
            effort: other.effort.or(self.effort),
            prompt_cache_ttl: other.prompt_cache_ttl.or(self.prompt_cache_ttl),
        }
    }
}

impl TuiConfig {
    #[expect(
        clippy::needless_pass_by_value,
        reason = "signature must match merge_section's fn(T, T) -> T"
    )]
    fn merge(self, other: Self) -> Self {
        Self {
            show_thinking: other.show_thinking.or(self.show_thinking),
        }
    }
}

/// Merges two optional config sections. When both are present, merges their
/// fields. When only one is present, use it as-is.
fn merge_section<T>(base: Option<T>, other: Option<T>, merge: fn(T, T) -> T) -> Option<T> {
    match (base, other) {
        (Some(b), Some(o)) => Some(merge(b, o)),
        (base, other) => other.or(base),
    }
}

// ── Loading ──

/// Loads and merges configuration from user and project TOML files.
///
/// Precedence (highest wins): project config > user config.
/// Environment variable overrides are applied later in [`super::Config::load`].
///
/// Returns an error if any discovered file is unreadable or malformed —
/// silent fallthrough would otherwise hide typos (e.g. a misplaced
/// `show_thinking` under `[client]`) and surface as a confusing
/// downstream "no credentials" error after the dropped config takes
/// the API key with it.
pub(super) fn load() -> Result<FileConfig> {
    let user = user_config_path()
        .map(|p| load_file(&p))
        .transpose()?
        .flatten();
    let project = find_project_config()
        .map(|p| load_file(&p))
        .transpose()?
        .flatten();

    let base = user.unwrap_or_default();
    Ok(match project {
        Some(p) => base.merge(p),
        None => base,
    })
}

/// Reads a single config file. `Ok(None)` when the file does not
/// exist; `Err` when it exists but cannot be read or parsed (so the
/// caller can surface the path and underlying TOML diagnostic).
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

/// User config: `$XDG_CONFIG_HOME/ox/config.toml`, falling back to
/// `~/.config/ox/config.toml`.
fn user_config_path() -> Option<PathBuf> {
    xdg_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
        Path::new(".config"),
        &Path::new(USER_CONFIG_DIR).join(USER_CONFIG_FILENAME),
    )
}

/// Walks from CWD upward to find the nearest `ox.toml`.
fn find_project_config() -> Option<PathBuf> {
    find_project_config_from(std::env::current_dir().ok()?)
}

/// Walks from `start` upward to find the nearest `ox.toml`.
///
/// Separated from [`find_project_config`] for testability (avoids changing
/// the process CWD).
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
                model: Some("base-model".to_owned()),
                base_url: Some("https://base.example.com".to_owned()),
                max_tokens: Some(1000),
                effort: Some(super::super::Effort::Low),
                prompt_cache_ttl: Some(super::super::PromptCacheTtl::FiveMin),
            }),
            tui: Some(TuiConfig {
                show_thinking: Some(false),
            }),
        };
        let other = FileConfig {
            client: Some(ClientConfig {
                api_key: Some("other-key".to_owned()),
                model: Some("other-model".to_owned()),
                base_url: Some("https://other.example.com".to_owned()),
                max_tokens: Some(2000),
                effort: Some(super::super::Effort::Max),
                prompt_cache_ttl: Some(super::super::PromptCacheTtl::OneHour),
            }),
            tui: Some(TuiConfig {
                show_thinking: Some(true),
            }),
        };
        let merged = base.merge(other);

        let client = merged.client.expect("client section should be present");
        assert_eq!(client.api_key.as_deref(), Some("other-key"));
        assert_eq!(client.model.as_deref(), Some("other-model"));
        assert_eq!(
            client.base_url.as_deref(),
            Some("https://other.example.com")
        );
        assert_eq!(client.max_tokens, Some(2000));
        assert_eq!(client.effort, Some(super::super::Effort::Max));
        assert_eq!(
            client.prompt_cache_ttl,
            Some(super::super::PromptCacheTtl::OneHour)
        );

        let tui = merged.tui.expect("tui section should be present");
        assert_eq!(tui.show_thinking, Some(true));
    }

    #[test]
    fn merge_falls_back_to_base_when_other_is_none() {
        let base = FileConfig {
            client: Some(ClientConfig {
                api_key: Some("key".to_owned()),
                model: Some("model".to_owned()),
                base_url: Some("https://example.com".to_owned()),
                max_tokens: Some(4096),
                effort: Some(super::super::Effort::High),
                prompt_cache_ttl: Some(super::super::PromptCacheTtl::FiveMin),
            }),
            tui: Some(TuiConfig {
                show_thinking: Some(true),
            }),
        };
        let merged = base.merge(FileConfig::default());

        let client = merged.client.expect("client section should survive");
        assert_eq!(client.api_key.as_deref(), Some("key"));
        assert_eq!(client.model.as_deref(), Some("model"));
        assert_eq!(client.base_url.as_deref(), Some("https://example.com"));
        assert_eq!(client.max_tokens, Some(4096));
        assert_eq!(client.effort, Some(super::super::Effort::High));
        assert_eq!(
            client.prompt_cache_ttl,
            Some(super::super::PromptCacheTtl::FiveMin)
        );

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

    // ── load_file ──

    #[test]
    fn load_file_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            indoc! {r#"
                [client]
                api_key = "sk-test"
                model = "claude-test"
                base_url = "https://test.example.com"
                max_tokens = 4096

                [tui]
                show_thinking = true
            "#},
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

        let tui = config.tui.expect("tui section should be present");
        assert_eq!(tui.show_thinking, Some(true));
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
    fn load_file_empty_toml_returns_defaults() {
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
    fn load_file_missing_file_returns_none() {
        let result =
            load_file(Path::new("/nonexistent/config.toml")).expect("missing file is not an error");
        assert!(result.is_none());
    }

    /// Read errors other than `NotFound` (here: pointing at a
    /// directory raises `IsADirectory`) must surface with the
    /// offending path so the user can act on it. Splitting this from
    /// the missing-file branch is the whole point of distinguishing
    /// `NotFound` from other IO errors in `load_file`.
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

    /// Surfaced as a hard error so the user sees the offending path
    /// and the TOML diagnostic instead of a silent fallthrough.
    /// Covers `FileConfig`'s `deny_unknown_fields` (top-level keys);
    /// the section-level analog is [`load_file_rejects_misplaced_field`].
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

    /// Catches the original bug report shape: `show_thinking` belongs
    /// in `[tui]`, not `[client]`. Without `deny_unknown_fields` +
    /// hard-fail, the whole file used to be dropped silently and the
    /// user got an unrelated "no credentials" error instead. Also
    /// covers a generic typo within a section (same code path).
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
    fn find_project_config_from_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_project_config_from(dir.path().to_path_buf()).is_none());
    }
}
