//! Instruction file discovery and loading (`AGENTS.md`, with `CLAUDE.md` fallbacks).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{fs, io, iter};

use indoc::{formatdoc, indoc};

use crate::util::path::{XdgBase, app_dir};

/// The cross-agent instruction file, which takes precedence wherever it is read.
pub(crate) const AGENTS_MD: &str = "AGENTS.md";

/// Claude Code's instruction file, read where no `AGENTS.md` has instructions.
pub(crate) const CLAUDE_MD: &str = "CLAUDE.md";

/// Claude Code's directory, which holds a `CLAUDE.md` in the home directory and in each project
/// directory.
const CLAUDE_DIR: &str = ".claude";

/// Candidates tried in order at a single location.
struct Slot {
    candidates: Vec<PathBuf>,
    label: &'static str,
}

struct InstructionFile {
    path: PathBuf,
    content: String,
    label: &'static str,
}

/// An instruction file with instructions in it.
#[derive(Debug)]
pub(crate) struct Found {
    /// The path with every symlink resolved, which identifies the file across spellings.
    pub(crate) canonical: PathBuf,
    /// The text without surrounding whitespace.
    content: String,
}

/// Loads the user's file, then one file per directory from the project root (`git_root`, or else
/// `cwd`) down to `cwd`, so files closer to `cwd` appear later and take precedence. A file loads
/// once however many locations resolve to it. Also returns a warning for each candidate that
/// exists but cannot be read.
pub(super) async fn load(cwd: &Path, git_root: Option<&Path>) -> (String, Vec<String>) {
    let slots = candidate_slots(cwd, git_root.unwrap_or(cwd));
    let (files, warnings) = tokio::task::spawn_blocking(|| load_files(slots))
        .await
        .expect("loading instruction files never panics");

    if files.is_empty() {
        return (String::new(), warnings);
    }

    (render(&files), warnings)
}

/// The files that can hold `dir`'s project instructions, in the order they are tried.
pub(crate) fn project_candidates(dir: &Path) -> [PathBuf; 3] {
    [
        dir.join(AGENTS_MD),
        dir.join(CLAUDE_MD),
        dir.join(CLAUDE_DIR).join(CLAUDE_MD),
    ]
}

/// Reads the instruction file at `path`, following symlinks. `None` when `path` names no regular
/// file or the file is blank, since a blank file leaves the next candidate to supply instructions.
pub(crate) fn read(path: &Path) -> io::Result<Option<Found>> {
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(e) if is_absent(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    if !fs::metadata(&canonical)?.is_file() {
        return Ok(None);
    }

    let content = fs::read_to_string(&canonical)?.trim().to_owned();
    Ok((!content.is_empty()).then_some(Found { canonical, content }))
}

/// Emits the user slot, then a project slot for each directory from `project_root` to `cwd`.
fn candidate_slots(cwd: &Path, project_root: &Path) -> Vec<Slot> {
    let user = Slot {
        candidates: user_candidates(),
        label: "user's global instructions",
    };
    let projects = walk_root_to_cwd(project_root, cwd)
        .into_iter()
        .map(|dir| Slot {
            candidates: project_candidates(&dir).into(),
            label: "project instructions",
        });

    iter::once(user).chain(projects).collect()
}

/// oxide-code's user file, then Claude Code's, so existing Claude Code instructions apply until
/// the user writes their own.
fn user_candidates() -> Vec<PathBuf> {
    let ox = app_dir(XdgBase::Config).map(|dir| dir.join(AGENTS_MD));
    let claude = std::env::home_dir().map(|home| home.join(CLAUDE_DIR).join(CLAUDE_MD));
    ox.into_iter().chain(claude).collect()
}

/// Every directory from `root` down to `cwd` inclusive. `[root]` when `cwd` is outside `root`.
fn walk_root_to_cwd(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let Ok(relative) = cwd.strip_prefix(root) else {
        return vec![root.to_path_buf()];
    };

    let mut dirs = vec![root.to_path_buf()];
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        dirs.push(current.clone());
    }

    dirs
}

/// The first file with instructions in each slot, and a warning for each candidate that cannot be
/// read. A slot whose candidate cannot be read falls back to the next one, while a slot whose file
/// an earlier slot loaded contributes nothing.
fn load_files(slots: Vec<Slot>) -> (Vec<InstructionFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut loaded = HashSet::new();

    for Slot { candidates, label } in slots {
        for path in candidates {
            match read(&path) {
                Ok(None) => {}
                Ok(Some(Found { canonical, content })) => {
                    if loaded.insert(canonical) {
                        files.push(InstructionFile {
                            path,
                            content,
                            label,
                        });
                    }
                    break;
                }
                Err(e) => {
                    warnings.push(format!("Skipped instruction file {}: {e}", path.display()));
                }
            }
        }
    }

    (files, warnings)
}

/// Returns whether a lookup failed since the path names no file, as when a parent is a regular
/// file.
fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn render(files: &[InstructionFile]) -> String {
    let header = indoc! {"
        # User instructions

        Codebase and user instructions are shown below. Be sure to adhere to these instructions."
    };
    let sections = files.iter().map(|file| {
        let InstructionFile {
            path,
            content,
            label,
        } = file;
        let path = path.display();
        formatdoc! {"
            Contents of {path} ({label}):

            {content}"
        }
    });

    iter::once(header.to_owned())
        .chain(sections)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::path::local_testing::abs;
    use crate::util::path::testing::HOME_VAR;

    /// The variables [`user_candidates`] reads, pinned to `home` with `XDG_CONFIG_HOME` unset.
    fn pinned_home(home: &Path) -> [(&'static str, Option<&Path>); 2] {
        [(HOME_VAR, Some(home)), ("XDG_CONFIG_HOME", None)]
    }

    mod load {
        use super::*;

        #[tokio::test]
        async fn uses_cwd_as_the_project_root_outside_a_git_repo() {
            let (home, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            std::fs::write(dir.path().join("CLAUDE.md"), "Fallback rules.").unwrap();

            let (result, warnings) =
                temp_env::async_with_vars(pinned_home(home.path()), load(dir.path(), None)).await;

            let path = dir.path().join("CLAUDE.md");
            assert_eq!(
                result,
                render(&[InstructionFile {
                    path,
                    content: "Fallback rules.".to_owned(),
                    label: "project instructions",
                }]),
            );
            assert_eq!(warnings, [] as [String; 0]);
        }

        #[tokio::test]
        async fn loads_the_user_file_once_in_the_home_directory() {
            let home = tempfile::tempdir().unwrap();
            let claude_dir = home.path().join(".claude");
            std::fs::create_dir(&claude_dir).unwrap();
            std::fs::write(claude_dir.join("CLAUDE.md"), "Global rules.").unwrap();

            let (result, _) =
                temp_env::async_with_vars(pinned_home(home.path()), load(home.path(), None)).await;

            assert_eq!(
                result,
                render(&[InstructionFile {
                    path: claude_dir.join("CLAUDE.md"),
                    content: "Global rules.".to_owned(),
                    label: "user's global instructions",
                }]),
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn loads_a_symlinked_file_once() {
            use std::os::unix::fs::symlink;

            let (home, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let root = root.path();
            std::fs::write(root.join("AGENTS.md"), "Shared rules.").unwrap();
            symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
            let sub = root.join("crates");
            std::fs::create_dir_all(sub.join(".claude")).unwrap();
            symlink("../AGENTS.md", sub.join("AGENTS.md")).unwrap();
            std::fs::write(sub.join(".claude/CLAUDE.md"), "Shadowed rules.").unwrap();

            let (result, _) =
                temp_env::async_with_vars(pinned_home(home.path()), load(&sub, Some(root))).await;

            assert_eq!(
                result,
                render(&[InstructionFile {
                    path: root.join("AGENTS.md"),
                    content: "Shared rules.".to_owned(),
                    label: "project instructions",
                }]),
            );
        }
    }

    mod project_candidates {
        use super::*;

        #[test]
        fn tries_agents_md_then_claude_md_then_the_claude_dir() {
            let dir = Path::new("/repo");

            assert_eq!(
                project_candidates(dir),
                [
                    dir.join("AGENTS.md"),
                    dir.join("CLAUDE.md"),
                    dir.join(".claude").join("CLAUDE.md"),
                ],
            );
        }
    }

    mod read {
        use super::*;

        #[test]
        fn trims_the_text_and_resolves_the_path() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("AGENTS.md"), "\n  Rules.  \n").unwrap();

            let found = read(&dir.path().join(".").join("AGENTS.md"))
                .unwrap()
                .unwrap();

            let canonical = std::fs::canonicalize(dir.path()).unwrap().join("AGENTS.md");
            assert_eq!(found.canonical, canonical);
            assert_eq!(found.content, "Rules.");
        }

        #[test]
        fn skips_blank_files_directories_and_absent_paths() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("AGENTS.md"), "  \n\t").unwrap();
            std::fs::create_dir(dir.path().join("CLAUDE.md")).unwrap();
            std::fs::write(dir.path().join("regular"), "Rules.").unwrap();

            for path in [
                dir.path().join("AGENTS.md"),
                dir.path().join("CLAUDE.md"),
                dir.path().join("missing.md"),
                dir.path().join("regular").join("AGENTS.md"),
            ] {
                assert!(matches!(read(&path), Ok(None)), "{}", path.display());
            }
        }

        #[test]
        fn fails_on_a_file_that_is_not_utf8() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("AGENTS.md");
            std::fs::write(&path, b"\xff\xfe").unwrap();

            let err = read(&path).unwrap_err();

            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    mod candidate_slots {
        use super::*;

        #[test]
        fn lists_the_user_files_then_each_directory_from_root_to_cwd() {
            let (home, xdg) = (PathBuf::from(abs("/home/u")), PathBuf::from(abs("/xdg")));
            let (root, cwd) = (
                PathBuf::from(abs("/repo")),
                PathBuf::from(abs("/repo/crates")),
            );
            let vars = [(HOME_VAR, Some(&home)), ("XDG_CONFIG_HOME", Some(&xdg))];

            let slots = temp_env::with_vars(vars, || candidate_slots(&cwd, &root));

            let slots: Vec<_> = slots
                .into_iter()
                .map(|Slot { candidates, label }| (label, candidates))
                .collect();
            assert_eq!(
                slots,
                [
                    (
                        "user's global instructions",
                        vec![
                            xdg.join("ox").join("AGENTS.md"),
                            home.join(".claude").join("CLAUDE.md"),
                        ],
                    ),
                    ("project instructions", project_candidates(&root).into()),
                    ("project instructions", project_candidates(&cwd).into()),
                ],
            );
        }
    }

    mod walk_root_to_cwd {
        use super::*;

        #[test]
        fn same_directory_includes_root_only() {
            let root = PathBuf::from("/repo");
            let dirs = walk_root_to_cwd(&root, &root);
            assert_eq!(dirs, vec![PathBuf::from("/repo")]);
        }

        #[test]
        fn nested_includes_each_directory_in_order() {
            let root = PathBuf::from("/repo");
            let cwd = PathBuf::from("/repo/a/b");
            let dirs = walk_root_to_cwd(&root, &cwd);
            assert_eq!(
                dirs,
                vec![
                    PathBuf::from("/repo"),
                    PathBuf::from("/repo/a"),
                    PathBuf::from("/repo/a/b"),
                ]
            );
        }

        #[test]
        fn outside_root_includes_root_only() {
            let root = PathBuf::from("/repo");
            let cwd = PathBuf::from("/other/dir");
            let dirs = walk_root_to_cwd(&root, &cwd);
            assert_eq!(dirs, vec![PathBuf::from("/repo")]);
        }
    }

    mod load_files {
        use super::*;

        fn loaded(files: &[InstructionFile]) -> Vec<(&Path, &str, &str)> {
            files
                .iter()
                .map(|file| (file.path.as_path(), file.content.as_str(), file.label))
                .collect()
        }

        #[test]
        fn loads_only_the_first_candidate_with_instructions() {
            let dir = tempfile::tempdir().unwrap();
            let [agents, claude, nested] = project_candidates(dir.path());
            std::fs::write(&agents, "  \n").unwrap();
            std::fs::write(&claude, "claude content").unwrap();
            std::fs::create_dir(dir.path().join(".claude")).unwrap();
            std::fs::write(&nested, "nested content").unwrap();
            let slots = vec![Slot {
                candidates: vec![
                    dir.path().join("missing.md"),
                    agents,
                    claude.clone(),
                    nested,
                ],
                label: "test",
            }];

            let (files, warnings) = load_files(slots);

            assert_eq!(
                loaded(&files),
                [(claude.as_path(), "claude content", "test")]
            );
            assert_eq!(warnings, [] as [String; 0]);
        }

        #[test]
        fn collects_one_file_per_slot() {
            let dir = tempfile::tempdir().unwrap();
            let a_path = dir.path().join("a.md");
            let b_path = dir.path().join("b.md");
            std::fs::write(&a_path, "slot 1").unwrap();
            std::fs::write(&b_path, "slot 2").unwrap();
            let slots = vec![
                Slot {
                    candidates: vec![a_path.clone()],
                    label: "first",
                },
                Slot {
                    candidates: vec![b_path.clone()],
                    label: "second",
                },
            ];

            let (files, _) = load_files(slots);

            assert_eq!(
                loaded(&files),
                [
                    (a_path.as_path(), "slot 1", "first"),
                    (b_path.as_path(), "slot 2", "second"),
                ],
            );
        }

        #[test]
        fn skips_a_slot_whose_file_an_earlier_slot_loaded() {
            let dir = tempfile::tempdir().unwrap();
            let a_path = dir.path().join("a.md");
            let b_path = dir.path().join("b.md");
            std::fs::write(&a_path, "shared").unwrap();
            std::fs::write(&b_path, "shadowed").unwrap();
            let slots = vec![
                Slot {
                    candidates: vec![a_path.clone()],
                    label: "first",
                },
                Slot {
                    candidates: vec![dir.path().join(".").join("a.md"), b_path],
                    label: "second",
                },
            ];

            let (files, _) = load_files(slots);

            assert_eq!(loaded(&files), [(a_path.as_path(), "shared", "first")]);
        }

        #[test]
        fn warns_of_an_unreadable_file_and_falls_back_to_the_next() {
            let dir = tempfile::tempdir().unwrap();
            let agents_path = dir.path().join("AGENTS.md");
            let claude_path = dir.path().join("CLAUDE.md");
            std::fs::write(&agents_path, b"\xff\xfe").unwrap();
            std::fs::write(&claude_path, "claude content").unwrap();
            let cause = std::fs::read_to_string(&agents_path).unwrap_err();
            let slots = vec![Slot {
                candidates: vec![agents_path.clone(), claude_path.clone()],
                label: "test",
            }];

            let (files, warnings) = load_files(slots);

            assert_eq!(
                loaded(&files),
                [(claude_path.as_path(), "claude content", "test")],
            );
            assert_eq!(
                warnings,
                [format!(
                    "Skipped instruction file {}: {cause}",
                    agents_path.display()
                )],
            );
        }
    }

    mod render {
        use super::*;

        #[test]
        fn formats_files_with_header_and_preserves_order() {
            let files = vec![
                InstructionFile {
                    path: PathBuf::from("/home/.config/ox/AGENTS.md"),
                    content: "Global rules.".to_owned(),
                    label: "user's global instructions",
                },
                InstructionFile {
                    path: PathBuf::from("/project/AGENTS.md"),
                    content: "Project rules.".to_owned(),
                    label: "project instructions",
                },
            ];
            let out = render(&files);

            assert_eq!(
                out,
                indoc! {"
                    # User instructions

                    Codebase and user instructions are shown below. Be sure to adhere to these instructions.

                    Contents of /home/.config/ox/AGENTS.md (user's global instructions):

                    Global rules.

                    Contents of /project/AGENTS.md (project instructions):

                    Project rules."
                },
            );
        }
    }
}
