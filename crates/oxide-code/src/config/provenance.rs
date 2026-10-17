//! Where resolved settings came from: the config file layers and the per-setting source `/config`
//! shows beside each value.

use std::fmt;
use std::path::{Path, PathBuf};

use super::load::project_config_path;
use crate::util::env;
use crate::util::path::tildify;

// ── Layer ──

/// One config file that loaded. Layers apply lowest precedence first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Layer {
    /// `$XDG_CONFIG_HOME/ox/config.toml`.
    User(PathBuf),
    /// A `$XDG_CONFIG_HOME/ox/config.d/*.toml` drop-in, applied in file-name order after the user
    /// file.
    DropIn(PathBuf),
    /// A project `ox.toml` from before the `.ox/` directory, applied below `.ox/config.toml`.
    LegacyProject(PathBuf),
    /// `.ox/config.toml` in the nearest directory at or above the working directory that has one.
    Project(PathBuf),
}

impl Layer {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::User(path)
            | Self::DropIn(path)
            | Self::LegacyProject(path)
            | Self::Project(path) => path,
        }
    }

    /// Project layers come from the checkout, so they may not set credentials, endpoints, or
    /// permission grants.
    pub(super) const fn is_project(&self) -> bool {
        matches!(self, Self::LegacyProject(_) | Self::Project(_))
    }

    /// Short name for the layer, as `/config` shows it.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::User(_) => "user".to_owned(),
            Self::DropIn(path) => format!(
                "config.d/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::LegacyProject(_) => "ox.toml".to_owned(),
            Self::Project(_) => "project".to_owned(),
        }
    }

    /// Startup notice for a layer that still loads from a superseded location.
    pub(super) fn notice(&self) -> Option<String> {
        let Self::LegacyProject(path) = self else {
            return None;
        };
        let current = project_config_path(path.parent()?);
        Some(format!(
            "{} is in the legacy project config location. Move its settings to {} and delete it.",
            tildify(path),
            tildify(&current),
        ))
    }
}

// ── Source ──

/// Where one resolved setting came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Source {
    /// The built-in default, including a model-derived value when nothing sets one.
    #[default]
    Default,
    File(Layer),
    Env(&'static str),
    /// Changed during the session by `/model` or `/effort`.
    Session,
}

impl Source {
    /// [`Source::Env`] when `var` is set and non-empty, matching how env vars override.
    pub(super) fn env_if_set(var: &'static str) -> Self {
        env::string(var).map_or(Self::Default, |_| Self::Env(var))
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::File(layer) => f.write_str(&layer.label()),
            Self::Env(var) => write!(f, "${var}"),
            Self::Session => f.write_str("session"),
        }
    }
}

// ── Sourced ──

/// A resolved value paired with the source that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sourced<T> {
    pub(super) value: T,
    pub(super) source: Source,
}

impl<T> Sourced<T> {
    pub(super) const fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }

    pub(super) const fn fallback(value: T) -> Self {
        Self::new(value, Source::Default)
    }

    pub(super) fn map<U>(self, f: impl FnOnce(T) -> U) -> Sourced<U> {
        Sourced::new(f(self.value), self.source)
    }
}

impl<T> Sourced<Option<T>> {
    /// An optional setting that nothing sets is `None` from the default source.
    pub(super) fn from_option(value: Option<Sourced<T>>) -> Self {
        value.map_or_else(|| Self::fallback(None), |set| set.map(Some))
    }
}

// ── Provenance ──

/// The config files that loaded and the source of each setting `/config` lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Provenance {
    /// Lowest precedence first.
    pub(crate) layers: Vec<Layer>,
    pub(crate) model: Source,
    pub(crate) effort: Source,
    pub(crate) auth: Source,
    pub(crate) base_url: Source,
    pub(crate) extra_ca_certs: Source,
    pub(crate) custom_headers: Source,
    pub(crate) attribution: Source,
    pub(crate) structured_outputs: Source,
    pub(crate) max_tokens: Source,
    pub(crate) max_tool_rounds: Source,
    pub(crate) prompt_cache_ttl: Source,
    pub(crate) compaction: Source,
    pub(crate) permission_mode: Source,
    pub(crate) show_thinking: Source,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::path::testing::HOME_VAR;

    // ── Layer::label ──

    #[test]
    fn label_names_each_layer_kind() {
        assert_eq!(
            Layer::User(PathBuf::from("/x/ox/config.toml")).label(),
            "user"
        );
        assert_eq!(
            Layer::DropIn(PathBuf::from("/x/ox/config.d/10-nix.toml")).label(),
            "config.d/10-nix.toml",
        );
        assert_eq!(
            Layer::LegacyProject(PathBuf::from("/repo/ox.toml")).label(),
            "ox.toml"
        );
        assert_eq!(
            Layer::Project(PathBuf::from("/repo/.ox/config.toml")).label(),
            "project"
        );
    }

    // ── Layer::notice ──

    #[test]
    fn notice_names_the_new_project_path_for_a_legacy_file() {
        let legacy = Layer::LegacyProject(PathBuf::from("/repo/ox.toml"));
        let current = Path::new("/repo").join(".ox").join("config.toml");

        let notice = temp_env::with_var(HOME_VAR, Some("/home/u"), || legacy.notice());

        assert_eq!(
            notice,
            Some(format!(
                "/repo/ox.toml is in the legacy project config location. Move its settings to \
                 {} and delete it.",
                current.display(),
            )),
        );
        assert_eq!(
            Layer::Project(PathBuf::from("/repo/.ox/config.toml")).notice(),
            None
        );
    }

    // ── Source ──

    #[test]
    fn source_display_names_the_layer_or_variable() {
        assert_eq!(Source::Default.to_string(), "default");
        assert_eq!(Source::Env("OX_EFFORT").to_string(), "$OX_EFFORT");
        assert_eq!(Source::Session.to_string(), "session");
        assert_eq!(
            Source::File(Layer::DropIn(PathBuf::from("/x/config.d/a.toml"))).to_string(),
            "config.d/a.toml",
        );
    }

    #[test]
    fn env_if_set_treats_an_empty_variable_as_unset() {
        const VAR: &str = "OX_TEST_PROVENANCE_VAR";
        temp_env::with_var(VAR, Some("1"), || {
            assert_eq!(Source::env_if_set(VAR), Source::Env(VAR));
        });
        temp_env::with_var(VAR, Some(""), || {
            assert_eq!(Source::env_if_set(VAR), Source::Default);
        });
    }
}
