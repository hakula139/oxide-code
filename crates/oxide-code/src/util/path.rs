//! Small path-related utilities shared across modules.

use std::path::{Path, PathBuf};

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

/// Replaces a `$HOME` prefix with `~/`, falling back to the full path.
pub(crate) fn tildify(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        .map_or_else(
            || path.display().to_string(),
            |rel| format!("~/{}", rel.display()),
        )
}

/// Expands a leading `~` or `~/` to the user's home directory. Bare `~` resolves to `$HOME`;
/// `~/foo/bar` resolves to `$HOME/foo/bar`. Per-user forms like `~alice/...` pass through
/// unchanged (no passwd lookup) and non-tilde paths pass through as well. Errors when the input
/// starts with `~` but `dirs::home_dir()` yields no value, since a literal `~`-prefixed path
/// would otherwise fail far downstream with a misleading filesystem error.
pub(crate) fn expand_user(raw: &str) -> Result<PathBuf> {
    let Some(tail) = raw.strip_prefix('~') else {
        return Ok(PathBuf::from(raw));
    };
    if !(tail.is_empty() || tail.starts_with('/')) {
        return Ok(PathBuf::from(raw));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("cannot expand `~` in {raw:?}: no home directory (set $HOME)"))?;
    if tail.is_empty() {
        return Ok(home);
    }
    Ok(home.join(tail.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    // ── xdg_dir ──

    #[test]
    fn xdg_dir_prefers_absolute_xdg_over_home_fallback() {
        let result = xdg_dir(
            Some(PathBuf::from("/custom/data")),
            Some(PathBuf::from("/home/u")),
            Path::new(".local/share"),
            Path::new("ox/sessions"),
        );
        assert_eq!(result, Some(PathBuf::from("/custom/data/ox/sessions")));
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
        let Some(home) = dirs::home_dir() else {
            return; // unusual CI envs without HOME
        };
        let path = home.join("work/project");
        assert_eq!(tildify(&path), "~/work/project");
    }

    #[test]
    fn tildify_preserves_paths_outside_home() {
        let path = PathBuf::from("/tmp/not-home/session");
        assert_eq!(tildify(&path), "/tmp/not-home/session");
    }

    #[test]
    fn tildify_leaves_home_itself_as_tilde() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(tildify(&home), "~/");
    }

    // ── expand_user ──

    #[test]
    fn expand_user_rewrites_leading_tilde_slash_to_home() {
        temp_env::with_var("HOME", Some("/tmp/oxide-fake-home"), || {
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
        temp_env::with_var("HOME", Some("/tmp/oxide-fake-home"), || {
            for raw in ["~//foo", "~///foo/bar"] {
                let got = expand_user(raw).unwrap();
                assert!(
                    got.starts_with("/tmp/oxide-fake-home"),
                    "{raw:?} -> {got:?}",
                );
            }
        });
    }

    #[test]
    fn expand_user_bare_tilde_resolves_to_home() {
        temp_env::with_var("HOME", Some("/tmp/oxide-fake-home"), || {
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
        // `~alice/foo` stays verbatim; no passwd lookup to resolve the user's home.
        assert_eq!(
            expand_user("~alice/foo").unwrap(),
            PathBuf::from("~alice/foo")
        );
    }

    // The `expand_user` home-unset error branch is unreachable from a Linux unit test:
    // `dirs::home_dir()` falls back to `getpwuid_r` when `$HOME` is empty, so the `None`
    // arm only fires in exotic environments (no passwd entry, Windows without profile).
    // The `Result` signature is still the right contract for those cases.
}
