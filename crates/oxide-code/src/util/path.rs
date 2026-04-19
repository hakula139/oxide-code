//! Small path-related utilities shared across modules.

use std::path::{Path, PathBuf};

/// Resolve an XDG-style directory: prefer `xdg` when set and absolute,
/// otherwise fall back to `home/home_fallback`. Both branches append
/// `subdir`. Returns `None` when neither base directory is available.
///
/// The `home_fallback` parameter selects the legacy XDG default
/// (typically `.local/share` for `$XDG_DATA_HOME` or `.config` for
/// `$XDG_CONFIG_HOME`).
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

/// Return `path` as a display string, replacing a `$HOME` prefix with
/// `~/` when applicable. Falls back to the full absolute display when
/// the home directory cannot be determined or the path does not live
/// under it.
pub(crate) fn tildify(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        .map_or_else(
            || path.display().to_string(),
            |rel| format!("~/{}", rel.display()),
        )
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
    fn xdg_dir_returns_none_without_home_or_xdg() {
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
}
