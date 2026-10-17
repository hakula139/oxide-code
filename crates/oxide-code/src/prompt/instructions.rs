//! Instruction file discovery and loading (CLAUDE.md, AGENTS.md).

use std::io;
use std::path::{Path, PathBuf};

use indoc::{formatdoc, indoc};
use tokio::fs;

/// Names tried in order at each location. The first file with non-blank content wins.
pub(crate) const INSTRUCTION_FILENAMES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// Subdirectories walked alongside each location (e.g. `<dir>/.claude/CLAUDE.md`).
pub(crate) const INSTRUCTION_DIRS: &[&str] = &[".claude"];

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

/// Walks `~/.claude/`, then root → cwd. Subdirectory files appear later (higher priority). Also
/// returns a warning for each candidate that exists but cannot be read.
pub(super) async fn load(cwd: &Path, git_root: Option<&Path>) -> (String, Vec<String>) {
    let slots = candidate_slots(cwd, git_root.unwrap_or(cwd));
    let (files, warnings) = load_files(slots).await;

    if files.is_empty() {
        return (String::new(), warnings);
    }

    (render(&files), warnings)
}

/// Emits a global slot (when home exists), then a root-level and a `.claude/` slot per directory.
fn candidate_slots(cwd: &Path, project_root: &Path) -> Vec<Slot> {
    let mut slots = Vec::new();

    if let Some(home) = std::env::home_dir() {
        for subdir in INSTRUCTION_DIRS {
            slots.push(Slot {
                candidates: INSTRUCTION_FILENAMES
                    .iter()
                    .map(|f| home.join(subdir).join(f))
                    .collect(),
                label: "user's global instructions",
            });
        }
    }

    for dir in walk_root_to_cwd(project_root, cwd) {
        slots.push(Slot {
            candidates: INSTRUCTION_FILENAMES.iter().map(|f| dir.join(f)).collect(),
            label: "project instructions",
        });
        for subdir in INSTRUCTION_DIRS {
            slots.push(Slot {
                candidates: INSTRUCTION_FILENAMES
                    .iter()
                    .map(|f| dir.join(subdir).join(f))
                    .collect(),
                label: "project instructions (.claude/)",
            });
        }
    }

    slots
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

/// The first non-empty file of each slot, and a warning for each candidate that cannot be read. A
/// slot whose first candidate cannot be read falls back to the next one.
async fn load_files(slots: Vec<Slot>) -> (Vec<InstructionFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();

    for Slot { candidates, label } in slots {
        for path in candidates {
            let content = match fs::read_to_string(&path).await {
                Ok(content) => content.trim().to_owned(),
                Err(e) if is_absent(&e) => continue,
                Err(e) => {
                    warnings.push(format!("Skipped instruction file {}: {e}", path.display()));
                    continue;
                }
            };
            if !content.is_empty() {
                files.push(InstructionFile {
                    path,
                    content,
                    label,
                });
                break;
            }
        }
    }

    (files, warnings)
}

/// Whether a read failed because the path names no file, as when a parent is a regular file.
fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn render(files: &[InstructionFile]) -> String {
    let header = indoc! {"
        # User instructions

        Codebase and user instructions are shown below.
        Be sure to adhere to these instructions."
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

    std::iter::once(header.to_owned())
        .chain(sections)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::path::testing::HOME_VAR;

    // ── load ──

    #[tokio::test]
    async fn load_uses_cwd_as_fallback_root_when_git_root_is_none() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        fs::write(dir.path().join("CLAUDE.md"), "Fallback rules.")
            .await
            .unwrap();

        let (result, _) = load(dir.path(), None).await;
        assert!(
            result.contains("Fallback rules."),
            "should discover CLAUDE.md using cwd as project root"
        );
    }

    #[tokio::test]
    async fn load_discovers_claude_dir_instructions() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("CLAUDE.md"), "Hidden rules.")
            .await
            .unwrap();

        let (result, _) = load(dir.path(), Some(dir.path())).await;
        assert!(
            result.contains("Hidden rules."),
            "should discover .claude/CLAUDE.md"
        );
    }

    // ── candidate_slots ──

    #[test]
    fn candidate_slots_at_root_includes_project_and_companion_files() {
        let root = PathBuf::from("/home/user/project");
        let slots = candidate_slots(&root, &root);

        let project: Vec<_> = slots
            .iter()
            .filter(|s| s.label == "project instructions")
            .collect();
        assert_eq!(project.len(), 1);
        assert_eq!(
            project[0].candidates,
            vec![root.join("CLAUDE.md"), root.join("AGENTS.md")]
        );

        let claude_dir: Vec<_> = slots
            .iter()
            .filter(|s| s.label == "project instructions (.claude/)")
            .collect();
        assert_eq!(claude_dir.len(), 1);
        assert_eq!(
            claude_dir[0].candidates,
            vec![
                root.join(".claude").join("CLAUDE.md"),
                root.join(".claude").join("AGENTS.md"),
            ]
        );
    }

    #[test]
    fn candidate_slots_walks_root_to_cwd() {
        let root = PathBuf::from("/repo");
        let cwd = PathBuf::from("/repo/crates/core");
        let slots = candidate_slots(&cwd, &root);

        let project: Vec<_> = slots
            .iter()
            .filter(|s| s.label == "project instructions")
            .collect();
        // 3 levels: /repo, /repo/crates, /repo/crates/core
        assert_eq!(project.len(), 3);
        assert_eq!(project[0].candidates[0], root.join("CLAUDE.md"));
        assert_eq!(
            project[1].candidates[0],
            root.join("crates").join("CLAUDE.md")
        );
        assert_eq!(project[2].candidates[0], cwd.join("CLAUDE.md"));
    }

    #[test]
    fn candidate_slots_lists_global_before_project() {
        let (home, root) = (Path::new("/home/u"), Path::new("/repo"));

        let slots = temp_env::with_var(HOME_VAR, Some(home), || candidate_slots(root, root));

        let labels: Vec<_> = slots.iter().map(|s| s.label).collect();
        assert_eq!(
            labels,
            [
                "user's global instructions",
                "project instructions",
                "project instructions (.claude/)",
            ]
        );
        assert_eq!(
            slots[0].candidates,
            vec![
                home.join(".claude").join("CLAUDE.md"),
                home.join(".claude").join("AGENTS.md"),
            ]
        );
    }

    // ── walk_root_to_cwd ──

    #[test]
    fn walk_root_to_cwd_same_directory_includes_root_only() {
        let root = PathBuf::from("/repo");
        let dirs = walk_root_to_cwd(&root, &root);
        assert_eq!(dirs, vec![PathBuf::from("/repo")]);
    }

    #[test]
    fn walk_root_to_cwd_nested_includes_each_directory_in_order() {
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
    fn walk_root_to_cwd_outside_root_includes_root_only() {
        let root = PathBuf::from("/repo");
        let cwd = PathBuf::from("/other/dir");
        let dirs = walk_root_to_cwd(&root, &cwd);
        assert_eq!(dirs, vec![PathBuf::from("/repo")]);
    }

    // ── load_files ──

    #[tokio::test]
    async fn load_files_prefers_first_candidate() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let claude_path = dir.path().join("CLAUDE.md");
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&claude_path, "claude content").await.unwrap();
        fs::write(&agents_path, "agents content").await.unwrap();

        let slots = vec![Slot {
            candidates: vec![claude_path.clone(), agents_path],
            label: "test",
        }];
        let (files, _) = load_files(slots).await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, claude_path);
        assert_eq!(files[0].content, "claude content");
    }

    #[tokio::test]
    async fn load_files_falls_back_to_second_candidate() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&agents_path, "agents content").await.unwrap();

        let slots = vec![Slot {
            candidates: vec![dir.path().join("CLAUDE.md"), agents_path.clone()],
            label: "test",
        }];
        let (files, _) = load_files(slots).await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, agents_path);
        assert_eq!(files[0].content, "agents content");
    }

    #[tokio::test]
    async fn load_files_collects_one_file_per_slot() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let a_path = dir.path().join("a.md");
        let b_path = dir.path().join("b.md");
        fs::write(&a_path, "slot 1").await.unwrap();
        fs::write(&b_path, "slot 2").await.unwrap();

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
        let (files, _) = load_files(slots).await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, a_path);
        assert_eq!(files[1].path, b_path);
    }

    #[tokio::test]
    async fn load_files_skips_whitespace_only_files() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let empty_path = dir.path().join("CLAUDE.md");
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&empty_path, "  \n  ").await.unwrap();
        fs::write(&agents_path, "real content").await.unwrap();

        let slots = vec![Slot {
            candidates: vec![empty_path, agents_path.clone()],
            label: "test",
        }];
        let (files, _) = load_files(slots).await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, agents_path);
    }

    #[tokio::test]
    async fn load_files_warns_of_an_unreadable_file_and_falls_back_to_the_next() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let claude_path = dir.path().join("CLAUDE.md");
        let agents_path = dir.path().join("AGENTS.md");
        fs::write(&claude_path, b"\xff\xfe").await.unwrap();
        fs::write(&agents_path, "agents content").await.unwrap();
        let cause = std::fs::read_to_string(&claude_path).unwrap_err();
        let slots = vec![Slot {
            candidates: vec![claude_path.clone(), agents_path.clone()],
            label: "test",
        }];

        let (files, warnings) = load_files(slots).await;

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, agents_path);
        assert_eq!(
            warnings,
            [format!(
                "Skipped instruction file {}: {cause}",
                claude_path.display()
            )],
        );
    }

    #[tokio::test]
    async fn load_files_absent_yields_empty_without_warnings() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let regular = dir.path().join("regular");
        fs::write(&regular, "a file, not a directory")
            .await
            .unwrap();
        let slots = vec![Slot {
            candidates: vec![dir.path().join("CLAUDE.md"), regular.join("CLAUDE.md")],
            label: "test",
        }];

        let (files, warnings) = load_files(slots).await;

        assert!(files.is_empty());
        assert_eq!(warnings, [] as [String; 0]);
    }

    // ── render ──

    #[test]
    fn render_formats_files_with_header_and_preserves_order() {
        let files = vec![
            InstructionFile {
                path: PathBuf::from("/home/.claude/CLAUDE.md"),
                content: "Global rules.".to_owned(),
                label: "user's global instructions",
            },
            InstructionFile {
                path: PathBuf::from("/project/CLAUDE.md"),
                content: "Project rules.".to_owned(),
                label: "project instructions",
            },
        ];
        let out = render(&files);

        assert_eq!(
            out,
            indoc! {"
                # User instructions

                Codebase and user instructions are shown below.
                Be sure to adhere to these instructions.

                Contents of /home/.claude/CLAUDE.md (user's global instructions):

                Global rules.

                Contents of /project/CLAUDE.md (project instructions):

                Project rules."
            },
        );
    }
}
