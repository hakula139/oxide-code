use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};

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
}

/// Terminal UI settings (`[tui]` section).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TuiConfig {
    pub show_thinking: Option<bool>,
}

// ── Merge ──

impl FileConfig {
    /// Merge two configs. Fields in `other` take precedence over `self`.
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

/// Merge two optional config sections. When both are present, merge their
/// fields. When only one is present, use it as-is.
fn merge_section<T>(base: Option<T>, other: Option<T>, merge: fn(T, T) -> T) -> Option<T> {
    match (base, other) {
        (Some(b), Some(o)) => Some(merge(b, o)),
        (base, other) => other.or(base),
    }
}

// ── Loading ──

/// Load and merge configuration from user and project TOML files.
///
/// Precedence (highest wins): project config > user config.
/// Environment variable overrides are applied later in [`super::Config::load`].
pub(super) fn load() -> FileConfig {
    let user = user_config_path().and_then(|p| load_file(&p));
    let project = find_project_config().and_then(|p| load_file(&p));

    let base = user.unwrap_or_default();
    match project {
        Some(p) => base.merge(p),
        None => base,
    }
}

fn load_file(path: &Path) -> Option<FileConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(config) => {
            debug!("loaded config from {}", path.display());
            Some(config)
        }
        Err(e) => {
            warn!("invalid config at {}: {e}", path.display());
            None
        }
    }
}

// ── Path Discovery ──

/// User config: `$XDG_CONFIG_HOME/ox/config.toml`, falling back to
/// `~/.config/ox/config.toml`.
fn user_config_path() -> Option<PathBuf> {
    resolve_user_config(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Resolve the user config path from explicit XDG and home directory values.
///
/// Separated from [`user_config_path`] for testability (avoids mutating env
/// vars, which is `unsafe` in Rust 2024 edition).
fn resolve_user_config(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = xdg
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join(USER_CONFIG_DIR).join(USER_CONFIG_FILENAME))
}

/// Walk from CWD upward to find the nearest `ox.toml`.
fn find_project_config() -> Option<PathBuf> {
    find_project_config_from(std::env::current_dir().ok()?)
}

/// Walk from `start` upward to find the nearest `ox.toml`.
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

        let config = load_file(&path).expect("should parse valid TOML");

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

        let config = load_file(&path).expect("should parse partial TOML");
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

        let config = load_file(&path).expect("empty TOML is valid");
        assert!(config.client.is_none());
        assert!(config.tui.is_none());
    }

    #[test]
    fn load_file_rejects_unknown_top_level_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, r#"model = "misplaced""#).unwrap();

        assert!(load_file(&path).is_none());
    }

    #[test]
    fn load_file_rejects_unknown_section_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            indoc! {r#"
                [client]
                api-key = "typo"
            "#},
        )
        .unwrap();

        assert!(load_file(&path).is_none());
    }

    #[test]
    fn load_file_invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[invalid").unwrap();

        assert!(load_file(&path).is_none());
    }

    #[test]
    fn load_file_missing_file_returns_none() {
        assert!(load_file(Path::new("/nonexistent/config.toml")).is_none());
    }

    // ── resolve_user_config ──

    #[test]
    fn resolve_user_config_prefers_xdg() {
        let xdg = PathBuf::from("/custom/config");
        let home = PathBuf::from("/home/user");
        let result = resolve_user_config(Some(xdg.clone()), Some(home));
        assert_eq!(
            result,
            Some(xdg.join(USER_CONFIG_DIR).join(USER_CONFIG_FILENAME))
        );
    }

    #[test]
    fn resolve_user_config_falls_back_to_home_dot_config() {
        let home = PathBuf::from("/home/user");
        let result = resolve_user_config(None, Some(home.clone()));
        assert_eq!(
            result,
            Some(
                home.join(".config")
                    .join(USER_CONFIG_DIR)
                    .join(USER_CONFIG_FILENAME)
            )
        );
    }

    #[test]
    fn resolve_user_config_ignores_relative_xdg() {
        let home = PathBuf::from("/home/user");
        let result = resolve_user_config(Some(PathBuf::from("relative/path")), Some(home.clone()));
        assert_eq!(
            result,
            Some(
                home.join(".config")
                    .join(USER_CONFIG_DIR)
                    .join(USER_CONFIG_FILENAME)
            )
        );
    }

    #[test]
    fn resolve_user_config_returns_none_without_home_or_xdg() {
        assert!(resolve_user_config(None, None).is_none());
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
