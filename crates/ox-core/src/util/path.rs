//! XDG and Claude Code directory lookup, `~` expansion and display, lexical path normalization,
//! and the slash-separated spelling of paths the model reads.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow};
use strum::VariantArray;
use unicode_normalization::UnicodeNormalization as _;

use super::env;

/// The directory oxide-code keeps under each XDG base directory.
const APP_DIR: &str = "ox";

/// The variable that moves Claude Code's config directory away from `~/.claude`.
const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

/// The file in [`claude_dir`] that holds Claude Code's login.
pub(crate) const CLAUDE_CREDENTIALS: &str = ".credentials.json";

/// An XDG base directory that oxide-code keeps files under.
#[derive(Debug, Clone, Copy, VariantArray)]
pub(crate) enum XdgBase {
    Config,
    Data,
    State,
}

impl XdgBase {
    const fn var(self) -> &'static str {
        match self {
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::State => "XDG_STATE_HOME",
        }
    }

    /// The spec's default under `$HOME`.
    const fn home_default(self) -> &'static str {
        match self {
            Self::Config => ".config",
            Self::Data => ".local/share",
            Self::State => ".local/state",
        }
    }
}

/// The XDG directory `base`, such as `$XDG_DATA_HOME` or `~/.local/share`. A relative variable is
/// ignored, as the spec requires. `None` without a home directory to fall back to.
pub(crate) fn xdg_dir(base: XdgBase) -> Option<PathBuf> {
    std::env::var_os(base.var())
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::home_dir().map(|home| home.join(base.home_default())))
}

/// oxide-code's directory under [`xdg_dir`], such as `$XDG_DATA_HOME/ox` or `~/.local/share/ox`.
pub(crate) fn app_dir(base: XdgBase) -> Option<PathBuf> {
    Some(xdg_dir(base)?.join(APP_DIR))
}

/// `$CLAUDE_CONFIG_DIR` in Unicode NFC, the form Claude Code reads it in, or `None` when unset or
/// empty, as Claude Code's Keychain naming treats an empty value.
pub(crate) fn claude_config_var() -> Option<String> {
    env::string(CLAUDE_CONFIG_DIR).map(|dir| dir.nfc().collect())
}

/// Claude Code's config directory, which holds its login, user settings, and user `CLAUDE.md`:
/// [`claude_config_var`] made absolute against the working directory, as Claude Code opens it, or
/// `~/.claude`. `None` without a home directory to fall back to.
pub(crate) fn claude_dir() -> Option<PathBuf> {
    match claude_config_var() {
        Some(dir) => std::path::absolute(dir).ok(),
        None => std::env::home_dir().map(|home| home.join(".claude")),
    }
}

/// Replaces a `$HOME` prefix with `~`, falling back to the full path.
pub fn tildify(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok()) {
        Some(rel) if rel.as_os_str().is_empty() => "~".to_owned(),
        Some(rel) => format!("~/{}", slash_separated(rel)),
        None => path.display().to_string(),
    }
}

/// Expands a leading `~` or `~/` (`~\` on Windows) to the home directory, passing `~alice/...`
/// and paths without a tilde through unchanged. Errors when there is no home directory, since the
/// literal path would fail later with a misleading filesystem error.
pub fn expand_user(raw: &str) -> Result<PathBuf> {
    let Some(tail) = raw.strip_prefix('~') else {
        return Ok(PathBuf::from(raw));
    };
    if !(tail.is_empty() || tail.starts_with(std::path::is_separator)) {
        return Ok(PathBuf::from(raw));
    }
    let home = std::env::home_dir()
        .ok_or_else(|| anyhow!("failed to expand `~` in {raw:?}: no home directory"))?;
    if tail.is_empty() {
        return Ok(home);
    }
    // `join` replaces `home` with an absolute argument, such as the `//foo` tail of `~//foo`.
    Ok(home.join(tail.trim_start_matches(std::path::is_separator)))
}

/// Resolves `.` and `..` components without touching the filesystem, so two spellings of one path
/// compare equal. A `..` that would climb above the root is dropped, matching how the OS clamps
/// traversal at `/`, while a leading `..` in a relative path is kept.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` as lossy UTF-8 with every platform separator written as `/`. Paths the model reads use
/// this form, since its shell is bash, where `\` escapes the next character.
pub(crate) fn slash_separated(path: &Path) -> String {
    path.to_string_lossy().replace(std::path::is_separator, "/")
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::path::{Path, PathBuf};

    /// A temp dir with a canonical path, the form a resolved workspace path takes. macOS puts temp
    /// dirs behind the `/var` → `/private/var` symlink, so a raw temp path never compares equal.
    pub(crate) struct CanonicalTempDir {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl CanonicalTempDir {
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    pub(crate) fn canonical_tempdir() -> CanonicalTempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dunce::canonicalize(dir.path()).unwrap();
        CanonicalTempDir { _dir: dir, path }
    }

    /// A rooted Unix fixture path made absolute for the host. Windows needs a drive, so the path
    /// moves onto `C:` there.
    pub(crate) fn abs(path: &str) -> String {
        let drive = if cfg!(windows) { "C:" } else { "" };
        format!("{drive}{path}")
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::path::Path;

    /// The variable [`std::env::home_dir`] reads first, so tests can point it at a fixture.
    pub const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

    /// Variables for `temp_env` that point the home directory at `home` and unset
    /// `XDG_CONFIG_HOME` and `CLAUDE_CONFIG_DIR`, so user-level config and instructions resolve
    /// under `home` alone.
    pub fn pinned_home(home: &Path) -> [(&'static str, Option<&Path>); 3] {
        [
            (HOME_VAR, Some(home)),
            ("XDG_CONFIG_HOME", None),
            ("CLAUDE_CONFIG_DIR", None),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::local_testing::abs;
    use super::testing::HOME_VAR;
    use super::*;

    mod app_dir {
        use super::*;

        #[test]
        fn prefers_the_base_variable_then_its_home_default() {
            let (home, xdg) = (abs("/home/u"), abs("/xdg"));
            for (base, var, home_default) in [
                (XdgBase::Config, "XDG_CONFIG_HOME", ".config"),
                (XdgBase::Data, "XDG_DATA_HOME", ".local/share"),
                (XdgBase::State, "XDG_STATE_HOME", ".local/state"),
            ] {
                let (set, unset) = temp_env::with_vars(
                    [(HOME_VAR, Some(home.as_str())), (var, Some(xdg.as_str()))],
                    || {
                        (
                            app_dir(base),
                            temp_env::with_var_unset(var, || app_dir(base)),
                        )
                    },
                );
                assert_eq!(set, Some(Path::new(&xdg).join("ox")), "{var}");
                assert_eq!(
                    unset,
                    Some(Path::new(&home).join(home_default).join("ox")),
                    "{var}"
                );
            }
        }

        #[test]
        fn ignores_a_relative_base_variable() {
            let home = abs("/home/u");
            let dir = temp_env::with_vars(
                [
                    (HOME_VAR, Some(home.as_str())),
                    ("XDG_STATE_HOME", Some("relative/state")),
                ],
                || app_dir(XdgBase::State),
            );
            assert_eq!(dir, Some(Path::new(&home).join(".local/state/ox")));
        }
    }

    mod claude_config_var {
        use super::*;

        #[test]
        fn normalizes_the_variable_to_nfc_and_reads_empty_as_unset() {
            let read = |value| temp_env::with_var(CLAUDE_CONFIG_DIR, value, claude_config_var);

            assert_eq!(
                read(Some("/srv/cafe\u{301}")),
                Some("/srv/caf\u{e9}".to_owned())
            );
            assert_eq!(read(Some("")), None);
            assert_eq!(read(None), None);
        }
    }

    mod claude_dir {
        use super::*;

        #[test]
        fn prefers_the_variable_then_the_home_default() {
            let (home, custom) = (abs("/home/u"), abs("/srv/claude"));

            let (set, unset) = temp_env::with_vars(
                [
                    (HOME_VAR, Some(home.as_str())),
                    (CLAUDE_CONFIG_DIR, Some(custom.as_str())),
                ],
                || {
                    (
                        claude_dir(),
                        temp_env::with_var_unset(CLAUDE_CONFIG_DIR, claude_dir),
                    )
                },
            );

            assert_eq!(set, Some(PathBuf::from(&custom)));
            assert_eq!(unset, Some(Path::new(&home).join(".claude")));
        }

        #[test]
        fn resolves_a_relative_variable_against_the_working_directory() {
            let dir = temp_env::with_var(CLAUDE_CONFIG_DIR, Some("claude-a"), claude_dir);

            assert_eq!(dir, Some(std::env::current_dir().unwrap().join("claude-a")));
        }
    }

    mod tildify {
        use super::*;

        #[test]
        fn rewrites_home_prefix_to_tilde() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-home"), || {
                assert_eq!(
                    tildify(&PathBuf::from("/tmp/oxide-home/work/project")),
                    "~/work/project",
                );
            });
        }

        #[test]
        fn preserves_paths_outside_home() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-home"), || {
                assert_eq!(
                    tildify(&PathBuf::from("/tmp/not-home/session")),
                    "/tmp/not-home/session",
                );
            });
        }

        #[test]
        fn renders_home_itself_as_a_bare_tilde() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-home"), || {
                assert_eq!(tildify(&PathBuf::from("/tmp/oxide-home")), "~");
            });
        }
    }

    mod expand_user {
        use super::*;

        #[test]
        fn rewrites_leading_tilde_slash_to_home() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
                assert_eq!(
                    expand_user("~/work/project").unwrap(),
                    PathBuf::from("/tmp/oxide-fake-home/work/project")
                );
            });
        }

        #[test]
        fn collapses_redundant_slashes_after_tilde() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
                for (raw, expected) in [
                    ("~//foo", "/tmp/oxide-fake-home/foo"),
                    ("~///foo/bar", "/tmp/oxide-fake-home/foo/bar"),
                ] {
                    assert_eq!(
                        expand_user(raw).unwrap(),
                        PathBuf::from(expected),
                        "{raw:?}"
                    );
                }
            });
        }

        #[test]
        fn bare_tilde_resolves_to_home() {
            temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
                assert_eq!(
                    expand_user("~").unwrap(),
                    PathBuf::from("/tmp/oxide-fake-home")
                );
            });
        }

        #[test]
        fn leaves_absolute_and_relative_paths_untouched() {
            for raw in ["/etc/ssl/cert.pem", "./certs/ca.pem", ""] {
                assert_eq!(expand_user(raw).unwrap(), PathBuf::from(raw), "{raw:?}");
            }
        }

        #[test]
        fn does_not_handle_per_user_home() {
            assert_eq!(
                expand_user("~alice/foo").unwrap(),
                PathBuf::from("~alice/foo")
            );
        }
    }

    mod lexical_normalize {
        use super::*;

        #[test]
        fn resolves_dot_and_dot_dot_components() {
            assert_eq!(
                lexical_normalize(Path::new("/work/./src/../lib/a.rs")),
                PathBuf::from("/work/lib/a.rs"),
            );
        }

        #[test]
        fn drops_a_leading_dot_in_relative_paths() {
            assert_eq!(
                lexical_normalize(Path::new("./src/a.rs")),
                PathBuf::from("src/a.rs")
            );
        }

        #[test]
        fn clamps_dot_dot_at_the_root() {
            assert_eq!(
                lexical_normalize(Path::new("/a/../../b")),
                PathBuf::from("/b")
            );
        }

        #[test]
        fn keeps_leading_dot_dot_in_relative_paths() {
            assert_eq!(
                lexical_normalize(Path::new("../x/./../y")),
                PathBuf::from("../y")
            );
        }
    }
}
