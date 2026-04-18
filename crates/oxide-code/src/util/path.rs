//! Small path-related utilities shared across modules.

use std::path::Path;

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
