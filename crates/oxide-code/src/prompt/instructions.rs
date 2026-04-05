use std::path::{Path, PathBuf};

use tokio::fs;

/// Instruction filenames to check at each project location, in priority order.
/// At each location, the first file found is used.
const INSTRUCTION_FILENAMES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// A discovered instruction file with its content and a human-readable label.
struct MemoryFile {
    path: PathBuf,
    content: String,
    label: &'static str,
}

/// Discover and load instruction files, returning the formatted section for the
/// system prompt.
///
/// At each directory level, filenames are checked in
/// [`INSTRUCTION_FILENAMES`] order — the first file found wins. Discovery
/// walks from the project root down to the working directory so that
/// subdirectory-specific instructions appear later (higher priority).
///
/// Discovery locations:
///
/// 1. User global: `~/.claude/CLAUDE.md` or `~/.claude/AGENTS.md`
/// 2. Each directory from project root to CWD (inclusive):
///    - `<dir>/CLAUDE.md` or `<dir>/AGENTS.md`
///    - `<dir>/.claude/CLAUDE.md` or `<dir>/.claude/AGENTS.md`
///
/// The project root is the git repository root when available, otherwise the
/// current working directory. The global file is always checked regardless of
/// whether a project root exists.
///
/// Returns an empty string when no files are found.
pub(super) async fn load(cwd: Option<&Path>, git_root: Option<&Path>) -> String {
    let project_root = git_root.or(cwd);
    let slots = candidate_slots(cwd, project_root);
    let files = load_files(slots).await;

    if files.is_empty() {
        return String::new();
    }

    render(&files)
}

/// Build candidate slots — groups of paths to try at each location.
///
/// Each slot lists [`INSTRUCTION_FILENAMES`] in priority order. The global
/// slot is always included when a home directory exists. Project slots walk
/// from the root to the working directory, generating two slots per directory
/// level (root-level and `.claude/`).
fn candidate_slots(
    cwd: Option<&Path>,
    project_root: Option<&Path>,
) -> Vec<(Vec<PathBuf>, &'static str)> {
    let mut slots = Vec::new();

    if let Some(home) = dirs::home_dir() {
        slots.push((
            INSTRUCTION_FILENAMES
                .iter()
                .map(|f| home.join(".claude").join(f))
                .collect(),
            "user's global instructions",
        ));
    }

    if let Some(root) = project_root {
        for dir in walk_root_to_cwd(root, cwd) {
            slots.push((
                INSTRUCTION_FILENAMES.iter().map(|f| dir.join(f)).collect(),
                "project instructions",
            ));
            slots.push((
                INSTRUCTION_FILENAMES
                    .iter()
                    .map(|f| dir.join(".claude").join(f))
                    .collect(),
                "project instructions (.claude/)",
            ));
        }
    }

    slots
}

/// Return every directory from `root` down to `cwd` (inclusive).
///
/// If `cwd` is not a subdirectory of `root`, or `cwd` is `None`, returns
/// just `[root]`.
fn walk_root_to_cwd(root: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let Some(cwd) = cwd else {
        return vec![root.to_path_buf()];
    };

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

/// Try each slot's candidates in order, loading the first file found per slot.
async fn load_files(slots: Vec<(Vec<PathBuf>, &'static str)>) -> Vec<MemoryFile> {
    let mut files = Vec::new();

    for (candidates, label) in slots {
        for path in candidates {
            if let Ok(content) = fs::read_to_string(&path).await {
                let content = content.trim().to_owned();
                if !content.is_empty() {
                    files.push(MemoryFile {
                        path,
                        content,
                        label,
                    });
                    break;
                }
            }
        }
    }

    files
}

/// Render memory files into a system prompt section.
fn render(files: &[MemoryFile]) -> String {
    use std::fmt::Write;

    let mut out = String::from(
        "# User instructions\n\n\
         Codebase and user instructions are shown below. \
         Be sure to adhere to these instructions.",
    );

    for file in files {
        let _ = write!(
            out,
            "\n\nContents of {} ({}):\n\n{}",
            file.path.display(),
            file.label,
            file.content,
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── candidate_slots ──

    #[test]
    fn candidate_slots_cwd_equals_root() {
        let root = PathBuf::from("/home/user/project");
        let slots = candidate_slots(Some(&root), Some(&root));

        // 1 global + 2 project (root-level + .claude/)
        let project: Vec<_> = slots
            .iter()
            .filter(|(_, l)| *l == "project instructions")
            .collect();
        assert_eq!(project.len(), 1);
        assert_eq!(
            project[0].0,
            vec![root.join("CLAUDE.md"), root.join("AGENTS.md")]
        );
    }

    #[test]
    fn candidate_slots_walks_root_to_cwd() {
        let root = PathBuf::from("/repo");
        let cwd = PathBuf::from("/repo/crates/core");
        let slots = candidate_slots(Some(&cwd), Some(&root));

        let project: Vec<_> = slots
            .iter()
            .filter(|(_, l)| *l == "project instructions")
            .collect();
        // 3 levels: /repo, /repo/crates, /repo/crates/core
        assert_eq!(project.len(), 3);
        assert_eq!(project[0].0[0], root.join("CLAUDE.md"));
        assert_eq!(project[1].0[0], root.join("crates").join("CLAUDE.md"));
        assert_eq!(project[2].0[0], cwd.join("CLAUDE.md"));
    }

    #[test]
    fn candidate_slots_without_project_root_still_includes_global() {
        let slots = candidate_slots(None, None);

        if let Some(home) = dirs::home_dir() {
            assert_eq!(slots.len(), 1);
            assert_eq!(slots[0].1, "user's global instructions");
            assert_eq!(
                slots[0].0,
                vec![
                    home.join(".claude").join("CLAUDE.md"),
                    home.join(".claude").join("AGENTS.md"),
                ]
            );
        } else {
            assert!(slots.is_empty());
        }
    }

    // ── walk_root_to_cwd ──

    #[test]
    fn walk_root_to_cwd_same_directory() {
        let root = PathBuf::from("/repo");
        let dirs = walk_root_to_cwd(&root, Some(&root));
        assert_eq!(dirs, vec![PathBuf::from("/repo")]);
    }

    #[test]
    fn walk_root_to_cwd_nested() {
        let root = PathBuf::from("/repo");
        let cwd = PathBuf::from("/repo/a/b");
        let dirs = walk_root_to_cwd(&root, Some(&cwd));
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
    fn walk_root_to_cwd_outside_root_returns_root_only() {
        let root = PathBuf::from("/repo");
        let cwd = PathBuf::from("/other/dir");
        let dirs = walk_root_to_cwd(&root, Some(&cwd));
        assert_eq!(dirs, vec![PathBuf::from("/repo")]);
    }

    #[test]
    fn walk_root_to_cwd_none_returns_root_only() {
        let root = PathBuf::from("/repo");
        let dirs = walk_root_to_cwd(&root, None);
        assert_eq!(dirs, vec![PathBuf::from("/repo")]);
    }

    // ── render ──

    #[test]
    fn render_formats_files_with_header_and_preserves_order() {
        let files = vec![
            MemoryFile {
                path: PathBuf::from("/home/.claude/CLAUDE.md"),
                content: "Global rules.".to_owned(),
                label: "user's global instructions",
            },
            MemoryFile {
                path: PathBuf::from("/project/CLAUDE.md"),
                content: "Project rules.".to_owned(),
                label: "project instructions",
            },
        ];
        let out = render(&files);

        assert!(out.starts_with("# User instructions"));
        assert!(out.contains("Be sure to adhere to these instructions."));
        assert!(out.contains("Contents of /home/.claude/CLAUDE.md (user's global instructions):"));
        assert!(out.contains("Contents of /project/CLAUDE.md (project instructions):"));

        let global_pos = out.find("Global rules.").expect("global content missing");
        let project_pos = out.find("Project rules.").expect("project content missing");
        assert!(
            global_pos < project_pos,
            "global should come before project"
        );
    }
}
