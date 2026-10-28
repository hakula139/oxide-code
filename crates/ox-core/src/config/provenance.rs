//! Where resolved settings came from: the config file layers and the per-setting source `/config`
//! shows beside each value.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::permission::defaults::Shipped;
use crate::permission::{PermissionClass, Tier};

// ── Layer ──

/// One config file that loaded. Layers apply lowest precedence first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    /// `$XDG_CONFIG_HOME/ox/config.toml`.
    User(PathBuf),
    /// A `$XDG_CONFIG_HOME/ox/config.d/*.toml` drop-in, applied in name order after the user file.
    DropIn(PathBuf),
    /// `.ox/config.toml` in the nearest directory from the working directory up to the project root
    /// that has one.
    Project(PathBuf),
}

impl Layer {
    pub fn path(&self) -> &Path {
        match self {
            Self::User(path) | Self::DropIn(path) | Self::Project(path) => path,
        }
    }

    /// Returns whether this is the project layer, which comes from the checkout and so may not set
    /// credentials, endpoints, or permission grants.
    pub(super) const fn is_project(&self) -> bool {
        matches!(self, Self::Project(_))
    }

    /// Short name for the layer, as `/config` shows it.
    fn label(&self) -> String {
        match self {
            Self::User(_) => "user".to_owned(),
            Self::DropIn(path) => format!(
                "config.d/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            Self::Project(_) => "project".to_owned(),
        }
    }
}

// ── Source ──

/// Where one resolved setting came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Source {
    /// The built-in default, including a model-derived value when nothing sets one.
    #[default]
    Default,
    File(Layer),
    Env(&'static str),
    Flag(&'static str),
    /// Changed during the session by `/model`, `/effort`, `/mode`, `/theme`, or Shift+Tab.
    Session,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::File(layer) => f.write_str(&layer.label()),
            Self::Env(var) => write!(f, "${var}"),
            Self::Flag(flag) => f.write_str(flag),
            Self::Session => f.write_str("session"),
        }
    }
}

// ── Sourced ──

/// A resolved value paired with the source that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
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

/// The config files that loaded, the source of each setting `/config` lists, and each entry of the
/// permission lists the files set, with the layer that added it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// Lowest precedence first.
    pub layers: Vec<Layer>,
    pub model: Source,
    pub effort: Source,
    pub auth: Source,
    pub base_url: Source,
    pub extra_ca_certs: Source,
    pub custom_headers: Source,
    pub attribution: Source,
    pub structured_outputs: Source,
    pub max_tokens: Source,
    pub max_tool_rounds: Source,
    pub prompt_cache_ttl: Source,
    pub compaction: Source,
    pub permission_mode: Source,
    pub show_thinking: Source,
    pub show_welcome: Source,
    pub status_line: Source,
    pub theme: Source,
    /// The layers that set any `[tui.theme.overrides]` slot, lowest first.
    pub theme_overrides: Vec<Source>,
    /// `permission.extra_roots` as written, in merged order.
    pub extra_roots: Vec<Sourced<String>>,
    /// The `deny`, `ask`, and `allow` patterns the config files add, grouped by tier and class.
    pub rules: Vec<Sourced<TierPattern>>,
    /// The shipped rule groups that `[permission.defaults]` turns off.
    pub disabled_defaults: Vec<Sourced<Shipped>>,
}

/// One pattern in a config file's `deny`, `ask`, or `allow` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierPattern {
    pub tier: Tier,
    pub class: PermissionClass,
    pub pattern: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod layer {
        use super::*;

        mod label {
            use super::*;

            #[test]
            fn names_each_layer_kind() {
                assert_eq!(
                    Layer::User(PathBuf::from("/x/ox/config.toml")).label(),
                    "user"
                );
                assert_eq!(
                    Layer::DropIn(PathBuf::from("/x/ox/config.d/10-nix.toml")).label(),
                    "config.d/10-nix.toml",
                );
                assert_eq!(
                    Layer::Project(PathBuf::from("/repo/.ox/config.toml")).label(),
                    "project"
                );
            }
        }
    }

    mod source {
        use super::*;

        #[test]
        fn display_names_the_layer_variable_or_flag() {
            assert_eq!(Source::Default.to_string(), "default");
            assert_eq!(Source::Env("OX_EFFORT").to_string(), "$OX_EFFORT");
            assert_eq!(Source::Flag("--mode").to_string(), "--mode");
            assert_eq!(Source::Session.to_string(), "session");
            assert_eq!(
                Source::File(Layer::DropIn(PathBuf::from("/x/config.d/a.toml"))).to_string(),
                "config.d/a.toml",
            );
        }
    }
}
