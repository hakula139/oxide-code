use std::path::{Path, PathBuf};

use tokio::fs;

/// A discovered CLAUDE.md file with its content and a human-readable label.
struct MemoryFile {
    path: PathBuf,
    content: String,
    label: &'static str,
}

/// Discover and load CLAUDE.md files, returning the formatted section for the
/// system prompt.
///
/// Discovery order:
/// 1. User global: `~/.claude/CLAUDE.md`
/// 2. Project root: `CLAUDE.md`
/// 3. Project `.claude/`: `.claude/CLAUDE.md`
///
/// The project root is the git repository root when available, otherwise the
/// current working directory.
///
/// Returns an empty string when no files are found.
pub(super) async fn load(cwd: Option<&Path>, git_root: Option<&Path>) -> String {
    let project_root = git_root.or(cwd);

    let Some(project_root) = project_root else {
        return String::new();
    };

    let candidates = candidate_paths(project_root);
    let files = load_files(candidates).await;

    if files.is_empty() {
        return String::new();
    }

    render(&files)
}

/// Build the list of candidate CLAUDE.md paths to check.
fn candidate_paths(project_root: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut paths = Vec::new();

    if let Some(home) = dirs::home_dir() {
        paths.push((
            home.join(".claude").join("CLAUDE.md"),
            "user's global instructions",
        ));
    }

    paths.push((project_root.join("CLAUDE.md"), "project instructions"));

    paths.push((
        project_root.join(".claude").join("CLAUDE.md"),
        "project instructions (.claude/)",
    ));

    paths
}

/// Load files that exist and have non-empty content.
async fn load_files(candidates: Vec<(PathBuf, &'static str)>) -> Vec<MemoryFile> {
    let mut files = Vec::new();

    for (path, label) in candidates {
        if let Ok(content) = fs::read_to_string(&path).await {
            let content = content.trim().to_owned();
            if !content.is_empty() {
                files.push(MemoryFile {
                    path,
                    content,
                    label,
                });
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

    // ── candidate_paths ──

    #[test]
    fn candidate_paths_includes_project_and_dotclaude() {
        let root = PathBuf::from("/home/user/project");
        let paths = candidate_paths(&root);

        let targets: Vec<_> = paths.iter().map(|(p, _)| p.clone()).collect();
        assert!(targets.contains(&root.join("CLAUDE.md")));
        assert!(targets.contains(&root.join(".claude").join("CLAUDE.md")));
        assert!(paths.len() >= 2);
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
