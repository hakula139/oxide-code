//! Small path-related utilities shared across modules.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow};

/// Resolves an XDG base directory with a `$HOME`-rooted fallback.
///
/// `$XDG_*_HOME` is honoured only when absolute (the spec rejects relative values, which would
/// otherwise resolve against the process cwd). Returns `None` when neither input can produce an
/// absolute base.
pub(crate) fn xdg_dir(
    xdg: Option<PathBuf>,
    home: Option<PathBuf>,
    home_fallback: &Path,
    subdir: &Path,
) -> Option<PathBuf> {
    let base = xdg
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(home_fallback)))?;
    Some(base.join(subdir))
}

/// oxide-code's data directory, `$XDG_DATA_HOME/ox` or `~/.local/share/ox`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    xdg_dir(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::home_dir(),
        Path::new(".local/share"),
        Path::new("ox"),
    )
}

/// Replaces a `$HOME` prefix with `~/`, falling back to the full path.
pub(crate) fn tildify(path: &Path) -> String {
    std::env::home_dir()
        .and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        .map_or_else(
            || path.display().to_string(),
            |rel| format!("~/{}", slash_separated(&rel)),
        )
}

/// Expands a leading `~` or `~/` (or `~\` on Windows) to the user's home directory. Bare `~`
/// resolves to `$HOME`, and `~/foo/bar` resolves to `$HOME/foo/bar`. Per-user forms like
/// `~alice/...` pass through unchanged (no passwd lookup) and non-tilde paths pass through as well.
/// Errors when the input starts with `~` but [`std::env::home_dir`] yields no value, since a
/// literal `~`-prefixed path would otherwise fail downstream with a misleading filesystem error.
pub(crate) fn expand_user(raw: &str) -> Result<PathBuf> {
    let Some(tail) = raw.strip_prefix('~') else {
        return Ok(PathBuf::from(raw));
    };
    if !(tail.is_empty() || tail.starts_with(std::path::is_separator)) {
        return Ok(PathBuf::from(raw));
    }
    let home = std::env::home_dir()
        .ok_or_else(|| anyhow!("cannot expand `~` in {raw:?}: no home directory"))?;
    if tail.is_empty() {
        return Ok(home);
    }
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

#[cfg(test)]
pub(crate) mod testing {
    /// The variable [`std::env::home_dir`] reads first, so tests can point it at a fixture.
    pub(crate) const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

    /// A rooted Unix fixture path made absolute for the host. Windows needs a drive, so the path
    /// moves onto `C:` there.
    pub(crate) fn abs(path: &str) -> String {
        let drive = if cfg!(windows) { "C:" } else { "" };
        format!("{drive}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{HOME_VAR, abs};
    use super::*;

    // ── xdg_dir ──

    #[test]
    fn xdg_dir_prefers_absolute_xdg_over_home_fallback() {
        let result = xdg_dir(
            Some(PathBuf::from(abs("/custom/data"))),
            Some(PathBuf::from("/home/u")),
            Path::new(".local/share"),
            Path::new("ox/sessions"),
        );
        assert_eq!(result, Some(PathBuf::from(abs("/custom/data/ox/sessions"))));
    }

    #[test]
    fn xdg_dir_falls_back_to_home_when_xdg_unset() {
        let result = xdg_dir(
            None,
            Some(PathBuf::from("/home/u")),
            Path::new(".config"),
            Path::new("ox/config.toml"),
        );
        assert_eq!(
            result,
            Some(PathBuf::from("/home/u/.config/ox/config.toml"))
        );
    }

    #[test]
    fn xdg_dir_ignores_relative_xdg_and_uses_home() {
        let result = xdg_dir(
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("/home/u")),
            Path::new(".local/share"),
            Path::new("ox/sessions"),
        );
        assert_eq!(
            result,
            Some(PathBuf::from("/home/u/.local/share/ox/sessions"))
        );
    }

    #[test]
    fn xdg_dir_is_none_without_home_or_xdg() {
        assert!(
            xdg_dir(
                None,
                None,
                Path::new(".local/share"),
                Path::new("ox/sessions")
            )
            .is_none()
        );
    }

    // ── tildify ──

    #[test]
    fn tildify_rewrites_home_prefix_to_tilde() {
        temp_env::with_var(HOME_VAR, Some("/tmp/oxide-home"), || {
            assert_eq!(
                tildify(&PathBuf::from("/tmp/oxide-home/work/project")),
                "~/work/project",
            );
        });
    }

    #[test]
    fn tildify_preserves_paths_outside_home() {
        temp_env::with_var(HOME_VAR, Some("/tmp/oxide-home"), || {
            assert_eq!(
                tildify(&PathBuf::from("/tmp/not-home/session")),
                "/tmp/not-home/session",
            );
        });
    }

    #[test]
    fn tildify_leaves_home_itself_as_tilde() {
        temp_env::with_var(HOME_VAR, Some("/tmp/oxide-home"), || {
            assert_eq!(tildify(&PathBuf::from("/tmp/oxide-home")), "~/");
        });
    }

    // ── expand_user ──

    #[test]
    fn expand_user_rewrites_leading_tilde_slash_to_home() {
        temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
            assert_eq!(
                expand_user("~/work/project").unwrap(),
                PathBuf::from("/tmp/oxide-fake-home/work/project")
            );
        });
    }

    #[test]
    fn expand_user_collapses_redundant_slashes_after_tilde() {
        // `~//foo` and `~///foo` should land at $HOME/foo, not at /foo. PathBuf::join replaces
        // the receiver when the argument is absolute, so the tail must be trimmed first.
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
    fn expand_user_bare_tilde_resolves_to_home() {
        temp_env::with_var(HOME_VAR, Some("/tmp/oxide-fake-home"), || {
            assert_eq!(
                expand_user("~").unwrap(),
                PathBuf::from("/tmp/oxide-fake-home")
            );
        });
    }

    #[test]
    fn expand_user_leaves_absolute_and_relative_paths_untouched() {
        for raw in ["/etc/ssl/cert.pem", "./certs/ca.pem", ""] {
            assert_eq!(expand_user(raw).unwrap(), PathBuf::from(raw), "{raw:?}");
        }
    }

    #[test]
    fn expand_user_does_not_handle_per_user_home() {
        // `~alice/foo` stays verbatim, since there is no passwd lookup to resolve the user's home.
        assert_eq!(
            expand_user("~alice/foo").unwrap(),
            PathBuf::from("~alice/foo")
        );
    }

    // ── lexical_normalize ──

    #[test]
    fn lexical_normalize_resolves_dot_and_dot_dot_components() {
        assert_eq!(
            lexical_normalize(Path::new("/work/./src/../lib/a.rs")),
            PathBuf::from("/work/lib/a.rs"),
        );
    }

    #[test]
    fn lexical_normalize_drops_a_leading_dot_in_relative_paths() {
        assert_eq!(
            lexical_normalize(Path::new("./src/a.rs")),
            PathBuf::from("src/a.rs")
        );
    }

    #[test]
    fn lexical_normalize_clamps_dot_dot_at_the_root() {
        assert_eq!(
            lexical_normalize(Path::new("/a/../../b")),
            PathBuf::from("/b")
        );
    }

    #[test]
    fn lexical_normalize_keeps_leading_dot_dot_in_relative_paths() {
        assert_eq!(
            lexical_normalize(Path::new("../x/./../y")),
            PathBuf::from("../y")
        );
    }
}
