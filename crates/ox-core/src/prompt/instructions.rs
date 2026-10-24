//! Instruction file discovery and loading (`AGENTS.md`, with `CLAUDE.md` fallbacks).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{fs, io, iter};

use indoc::{formatdoc, indoc};

use crate::permission::project::{ProjectFileError, resolve_project_file};
use crate::util::path::{XdgBase, app_dir, claude_dir};

/// The cross-agent instruction file, which takes precedence wherever it is read.
pub(crate) const AGENTS_MD: &str = "AGENTS.md";

/// Claude Code's instruction file, read where no `AGENTS.md` has instructions.
pub(crate) const CLAUDE_MD: &str = "CLAUDE.md";

/// The directory in each project directory that may hold its Claude Code `CLAUDE.md`.
const CLAUDE_DIR: &str = ".claude";

/// Candidates tried in order at a single location.
struct Slot {
    candidates: Vec<PathBuf>,
    label: &'static str,
    /// The project root a checkout's candidates must resolve inside, or `None` for the user's own
    /// files, which may link anywhere.
    root: Option<PathBuf>,
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

/// Loads global instructions, then project instructions from `project_root` through `cwd`,
/// collecting warnings for skipped files. Each location uses its first readable, nonblank
/// candidate, and resolved files load once.
pub(super) async fn load(cwd: &Path, project_root: &Path) -> (String, Vec<String>) {
    let slots = candidate_slots(cwd, project_root);
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

/// Reads the project candidate at `path` as [`read`] does, when it resolves to a Markdown file that
/// [`resolve_project_file`] lets the checkout at `root` supply, so a checked-in symlink cannot pull
/// in a file outside the root, or one inside it that holds no instructions, such as `.env`. `Err`
/// gives the reason a file there is skipped.
pub(crate) fn read_project(path: &Path, root: &Path) -> Result<Option<Found>, String> {
    let resolved = match resolve_project_file(path, root) {
        Ok(resolved) => resolved,
        Err(ProjectFileError::Io(e)) if is_absent(&e) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !resolved
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    {
        return Err("it links to a file without a .md extension".to_owned());
    }
    read(&resolved).map_err(|e| e.to_string())
}

/// Emits the user slot, then a project slot for each directory from `project_root` to `cwd`. A
/// project candidate that is also a user candidate, as at `$HOME`, stays in the user slot only.
fn candidate_slots(cwd: &Path, project_root: &Path) -> Vec<Slot> {
    let user = Slot {
        candidates: user_candidates(),
        label: "user's global instructions",
        root: None,
    };
    let projects = walk_root_to_cwd(project_root, cwd)
        .into_iter()
        .map(|dir| Slot {
            candidates: project_candidates(&dir)
                .into_iter()
                .filter(|path| !user.candidates.contains(path))
                .collect(),
            label: "project instructions",
            root: Some(project_root.to_owned()),
        })
        .collect::<Vec<_>>();

    iter::once(user).chain(projects).collect()
}

/// oxide-code's user file, then Claude Code's, so existing Claude Code instructions apply until the
/// user writes their own.
fn user_candidates() -> Vec<PathBuf> {
    let ox = app_dir(XdgBase::Config).map(|dir| dir.join(AGENTS_MD));
    let claude = claude_dir().map(|dir| dir.join(CLAUDE_MD));
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

/// Loads the first readable, nonblank file in each slot and collects skipped-file warnings. A file
/// already loaded by an earlier slot ends the slot without adding another copy.
fn load_files(slots: Vec<Slot>) -> (Vec<InstructionFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut loaded = HashSet::new();

    for Slot {
        candidates,
        label,
        root,
    } in slots
    {
        for path in candidates {
            let found = match &root {
                Some(root) => read_project(&path, root),
                None => read(&path).map_err(|e| e.to_string()),
            };
            match found {
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
    use crate::util::fs::testing::{symlink_dir, symlink_file};
    use crate::util::path::local_testing::abs;
    use crate::util::path::testing::{HOME_VAR, pinned_home};

    fn skipped(path: &Path, reason: &str) -> String {
        format!("Skipped instruction file {}: {reason}", path.display())
    }

    mod load {
        use super::*;

        #[tokio::test]
        async fn reads_project_instructions_at_the_root() {
            let (home, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            std::fs::write(dir.path().join("CLAUDE.md"), "Root rules.").unwrap();

            let (result, warnings) =
                temp_env::async_with_vars(pinned_home(home.path()), load(dir.path(), dir.path()))
                    .await;

            let path = dir.path().join("CLAUDE.md");
            assert_eq!(
                result,
                render(&[InstructionFile {
                    path,
                    content: "Root rules.".to_owned(),
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
                temp_env::async_with_vars(pinned_home(home.path()), load(home.path(), home.path()))
                    .await;

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
                temp_env::async_with_vars(pinned_home(home.path()), load(&sub, root)).await;

            assert_eq!(
                result,
                render(&[InstructionFile {
                    path: root.join("AGENTS.md"),
                    content: "Shared rules.".to_owned(),
                    label: "project instructions",
                }]),
            );
        }

        #[tokio::test]
        async fn skips_project_files_that_resolve_outside_the_root() {
            let (home, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let secret = outside.path().join("secret.md");
            std::fs::write(&secret, "OUTSIDE-FILE-MARKER").unwrap();
            std::fs::write(outside.path().join("CLAUDE.md"), "OUTSIDE-DIR-MARKER").unwrap();
            let project = tempfile::tempdir().unwrap();
            let sub = project.path().join("sub");
            std::fs::create_dir(&sub).unwrap();
            symlink_file(&secret, &project.path().join("AGENTS.md"));
            std::fs::write(project.path().join("CLAUDE.md"), "Project rules.").unwrap();
            symlink_dir(outside.path(), &sub.join(".claude"));

            let (result, warnings) =
                temp_env::async_with_vars(pinned_home(home.path()), load(&sub, project.path()))
                    .await;

            assert_eq!(
                result,
                render(&[InstructionFile {
                    path: project.path().join("CLAUDE.md"),
                    content: "Project rules.".to_owned(),
                    label: "project instructions",
                }]),
            );
            let reason = "it resolves outside the project root";
            assert_eq!(
                warnings,
                [
                    skipped(&project.path().join("AGENTS.md"), reason),
                    skipped(&sub.join(".claude").join("CLAUDE.md"), reason),
                ],
            );
        }

        #[tokio::test]
        async fn skips_a_project_link_to_a_file_that_is_not_markdown() {
            let (home, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            std::fs::write(project.path().join(".env"), "TOKEN=secret-marker").unwrap();
            symlink_file(
                &project.path().join(".env"),
                &project.path().join("AGENTS.md"),
            );

            let (result, warnings) = temp_env::async_with_vars(
                pinned_home(home.path()),
                load(project.path(), project.path()),
            )
            .await;

            assert_eq!(result, "");
            assert_eq!(
                warnings,
                [skipped(
                    &project.path().join("AGENTS.md"),
                    "it links to a file without a .md extension",
                )],
            );
        }

        #[tokio::test]
        async fn skips_any_project_link_when_the_root_contains_home() {
            let home = tempfile::tempdir().unwrap();
            let project = home.path().join("project");
            std::fs::create_dir(&project).unwrap();
            std::fs::write(project.join("notes.md"), "Linked rules.").unwrap();
            symlink_file(&project.join("notes.md"), &project.join("AGENTS.md"));
            std::fs::write(project.join("CLAUDE.md"), "Plain rules.").unwrap();

            let (result, warnings) =
                temp_env::async_with_vars(pinned_home(home.path()), load(&project, home.path()))
                    .await;

            assert_eq!(
                result,
                render(&[InstructionFile {
                    path: project.join("CLAUDE.md"),
                    content: "Plain rules.".to_owned(),
                    label: "project instructions",
                }]),
            );
            assert_eq!(
                warnings,
                [skipped(
                    &project.join("AGENTS.md"),
                    "symlinks are not followed when the project root contains the home directory",
                )],
            );
        }

        #[tokio::test]
        async fn follows_user_file_links_anywhere() {
            let (home, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            std::fs::write(outside.path().join("rules.txt"), "Linked global rules.").unwrap();
            let claude_dir = home.path().join(".claude");
            std::fs::create_dir(&claude_dir).unwrap();
            symlink_file(
                &outside.path().join("rules.txt"),
                &claude_dir.join("CLAUDE.md"),
            );

            let (result, warnings) =
                temp_env::async_with_vars(pinned_home(home.path()), load(home.path(), home.path()))
                    .await;

            assert_eq!(
                result,
                render(&[InstructionFile {
                    path: claude_dir.join("CLAUDE.md"),
                    content: "Linked global rules.".to_owned(),
                    label: "user's global instructions",
                }]),
            );
            assert_eq!(warnings, [] as [String; 0]);
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

    mod read_project {
        use super::*;

        #[test]
        fn follows_a_link_to_markdown_inside_the_root() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("docs")).unwrap();
            std::fs::write(dir.path().join("docs").join("ai.md"), "Rules.").unwrap();
            symlink_file(
                &dir.path().join("docs").join("ai.md"),
                &dir.path().join("AGENTS.md"),
            );

            let found = read_project(&dir.path().join("AGENTS.md"), dir.path())
                .unwrap()
                .unwrap();

            let canonical = std::fs::canonicalize(dir.path()).unwrap();
            assert_eq!(found.canonical, canonical.join("docs").join("ai.md"));
            assert_eq!(found.content, "Rules.");
        }

        #[test]
        fn absent_file_is_skipped_silently() {
            let dir = tempfile::tempdir().unwrap();

            let found = read_project(&dir.path().join("AGENTS.md"), dir.path());

            assert!(matches!(found, Ok(None)), "{found:?}");
        }
    }

    mod candidate_slots {
        use super::*;

        #[test]
        fn lists_the_user_files_then_each_directory_from_root_to_cwd() {
            let (home, xdg) = (PathBuf::from(abs("/home/u")), PathBuf::from(abs("/xdg")));
            let claude = PathBuf::from(abs("/srv/claude"));
            let (root, cwd) = (
                PathBuf::from(abs("/repo")),
                PathBuf::from(abs("/repo/crates")),
            );
            let vars = [
                (HOME_VAR, Some(&home)),
                ("XDG_CONFIG_HOME", Some(&xdg)),
                ("CLAUDE_CONFIG_DIR", Some(&claude)),
            ];

            let slots = temp_env::with_vars(vars, || candidate_slots(&cwd, &root));

            let slots: Vec<_> = slots
                .into_iter()
                .map(
                    |Slot {
                         candidates, label, ..
                     }| (label, candidates),
                )
                .collect();
            assert_eq!(
                slots,
                [
                    (
                        "user's global instructions",
                        vec![xdg.join("ox").join("AGENTS.md"), claude.join("CLAUDE.md"),],
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
                root: None,
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
                    root: None,
                },
                Slot {
                    candidates: vec![b_path.clone()],
                    label: "second",
                    root: None,
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
                    root: None,
                },
                Slot {
                    candidates: vec![dir.path().join(".").join("a.md"), b_path],
                    label: "second",
                    root: None,
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
                root: None,
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
