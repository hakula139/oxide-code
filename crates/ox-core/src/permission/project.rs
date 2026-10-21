//! Files a checkout supplies, such as project config and instruction files, which must resolve
//! inside the project root.

use std::io;
use std::path::{Path, PathBuf};

use super::is_workspace;

/// Why a file a checkout supplies may not be read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProjectFileError {
    /// The file or the root does not resolve, as when no file is there.
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("it resolves outside the project root")]
    Outside,
    #[error("symlinks are not followed when the project root contains the home directory")]
    Linked,
}

/// Canonicalizes `path`, which must sit below `root` as spelled, and returns it when a checkout at
/// `root` may supply it. It must resolve inside `root`, and when `root` is `/`, `$HOME`, or an
/// ancestor of `$HOME`, where "inside" spans every dotfile, no component below `root` may be a
/// symlink.
pub(crate) fn resolve_project_file(path: &Path, root: &Path) -> Result<PathBuf, ProjectFileError> {
    let resolved = std::fs::canonicalize(path)?;
    let canonical_root = std::fs::canonicalize(root)?;
    if !resolved.starts_with(&canonical_root) {
        return Err(ProjectFileError::Outside);
    }
    if !is_workspace(&canonical_root) && passes_through_link(path, root) {
        return Err(ProjectFileError::Linked);
    }
    Ok(resolved)
}

/// Returns whether `path`, or a directory between it and `root`, is a symlink.
fn passes_through_link(path: &Path, root: &Path) -> bool {
    path.ancestors()
        .take_while(|dir| *dir != root && dir.starts_with(root))
        .any(|dir| std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_symlink()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::util::fs::testing::{symlink_dir, symlink_file};
    use crate::util::path::testing::HOME_VAR;

    mod resolve_project_file {
        use super::*;

        #[test]
        fn inside_the_root_is_its_canonical_target() {
            let dir = tempdir().unwrap();
            fs::create_dir(dir.path().join("sub")).unwrap();
            fs::write(dir.path().join("AGENTS.md"), "x").unwrap();
            symlink_file(&dir.path().join("AGENTS.md"), &dir.path().join("CLAUDE.md"));
            let canonical = fs::canonicalize(dir.path()).unwrap().join("AGENTS.md");

            let resolved = [
                dir.path().join("sub").join("..").join("AGENTS.md"),
                dir.path().join("CLAUDE.md"),
            ]
            .map(|path| resolve_project_file(&path, dir.path()).unwrap());

            assert_eq!(resolved, [canonical.clone(), canonical]);
        }

        #[test]
        fn rejects_a_link_outside_the_root() {
            let outside = tempdir().unwrap();
            fs::write(outside.path().join("secret"), "x").unwrap();
            let dir = tempdir().unwrap();
            symlink_file(&outside.path().join("secret"), &dir.path().join("file"));
            symlink_dir(outside.path(), &dir.path().join("linked"));

            for path in [
                dir.path().join("file"),
                dir.path().join("linked").join("secret"),
            ] {
                let result = resolve_project_file(&path, dir.path());

                assert!(
                    matches!(result, Err(ProjectFileError::Outside)),
                    "{path:?}: {result:?}"
                );
            }
        }

        #[test]
        fn rejects_any_link_below_a_root_that_contains_home() {
            let home = tempdir().unwrap();
            let project = home.path().join("project");
            fs::create_dir_all(home.path().join("shared")).unwrap();
            fs::create_dir(&project).unwrap();
            fs::write(home.path().join("notes.md"), "x").unwrap();
            fs::write(home.path().join("shared").join("CLAUDE.md"), "x").unwrap();
            fs::write(project.join("AGENTS.md"), "x").unwrap();
            symlink_file(&home.path().join("notes.md"), &project.join("CLAUDE.md"));
            symlink_dir(&home.path().join("shared"), &project.join(".claude"));

            let [regular, file_link, dir_link] =
                temp_env::with_var(HOME_VAR, Some(home.path()), || {
                    [
                        project.join("AGENTS.md"),
                        project.join("CLAUDE.md"),
                        project.join(".claude").join("CLAUDE.md"),
                    ]
                    .map(|path| resolve_project_file(&path, home.path()))
                });

            assert!(regular.is_ok(), "{regular:?}");
            assert!(
                matches!(file_link, Err(ProjectFileError::Linked)),
                "{file_link:?}"
            );
            assert!(
                matches!(dir_link, Err(ProjectFileError::Linked)),
                "{dir_link:?}"
            );
        }

        #[test]
        fn missing_file_is_not_found() {
            let dir = tempdir().unwrap();

            let result = resolve_project_file(&dir.path().join("missing.md"), dir.path());

            assert!(
                matches!(&result, Err(ProjectFileError::Io(e)) if e.kind() == io::ErrorKind::NotFound),
                "{result:?}"
            );
        }
    }
}
