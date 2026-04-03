use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolOutput};

const MAX_RESULTS: usize = 100;

pub(crate) struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern. Returns paths sorted by modification time (newest first)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match (e.g. \"**/*.rs\", \"src/**/*.ts\")"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in (default: current working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn run(
        &self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(run(input))
    }
}

// ── Input ──

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

// ── Execution ──

async fn run(raw: serde_json::Value) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let Input { pattern, path } = input;

    match tokio::task::spawn_blocking(move || glob_files(&pattern, path.as_deref())).await {
        Ok(result) => ToolOutput::from_result(result),
        Err(e) => ToolOutput {
            content: format!("Internal error: {e}"),
            is_error: true,
        },
    }
}

fn glob_files(pattern: &str, search_path: Option<&str>) -> Result<String, String> {
    let base = super::resolve_base_dir(search_path)?;

    if !base.is_dir() {
        return Err(format!("Directory does not exist: {}", base.display()));
    }

    let glob = globset::Glob::new(pattern)
        .map_err(|e| format!("Invalid glob pattern: {e}"))?
        .compile_matcher();

    let mut matches: Vec<(String, std::time::SystemTime)> = super::walk_files(&base)
        .filter(|entry| {
            let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            glob.is_match(rel)
        })
        .map(|entry| {
            let mtime = super::entry_mtime(&entry);
            (entry.path().to_string_lossy().into_owned(), mtime)
        })
        .collect();

    matches.sort_by(|a, b| b.1.cmp(&a.1));

    if matches.is_empty() {
        return Ok("No files found".into());
    }

    let truncated = matches.len() > MAX_RESULTS;
    let total = matches.len();
    matches.truncate(MAX_RESULTS);

    let mut output = String::new();
    for (i, (p, _)) in matches.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        output.push_str(p);
    }

    if truncated {
        _ = write!(
            output,
            "\n\n(Showing {MAX_RESULTS} of {total} matches. Use a more specific pattern.)"
        );
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();

        let output = run(serde_json::json!({
            "pattern": "*.txt",
            "path": dir.path().to_str().unwrap()
        }))
        .await;

        assert!(!output.is_error);
        assert!(output.content.contains("a.txt"));
        assert!(!output.content.contains("b.rs"));
    }

    #[tokio::test]
    async fn run_missing_pattern() {
        let output = run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    // ── glob_files ──

    #[test]
    fn glob_files_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.txt"), "").unwrap();
        std::fs::write(dir.path().join("bar.txt"), "").unwrap();
        std::fs::write(dir.path().join("baz.rs"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("foo.txt"));
        assert!(result.contains("bar.txt"));
        assert!(!result.contains("baz.rs"));
    }

    #[test]
    fn glob_files_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(dir.path().join("top.rs"), "").unwrap();
        std::fs::write(sub.join("nested.rs"), "").unwrap();

        let result = glob_files("**/*.rs", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("top.rs"));
        assert!(result.contains("nested.rs"));
    }

    #[test]
    fn glob_files_sorted_by_mtime() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(dir.path().join("old.txt"), "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("new.txt"), "new").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("new.txt"));
        assert!(lines[1].contains("old.txt"));
    }

    #[test]
    fn glob_files_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let result = glob_files("*.rs", Some(dir.path().to_str().unwrap())).unwrap();
        assert_eq!(result, "No files found");
    }

    #[test]
    fn glob_files_invalid_directory() {
        let err = glob_files("*.txt", Some("/nonexistent/dir")).unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn glob_files_truncated_at_max_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_RESULTS + 10 {
            std::fs::write(dir.path().join(format!("{i:04}.txt")), "").unwrap();
        }

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        let file_count = result.lines().filter(|l| l.contains(".txt")).count();
        assert_eq!(file_count, MAX_RESULTS);
        assert!(result.contains(&format!("Showing {MAX_RESULTS} of {}", MAX_RESULTS + 10)));
    }

    #[test]
    fn glob_files_skips_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.txt"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("visible.txt"));
        assert!(!result.contains(".hidden"));
    }

    #[test]
    fn glob_files_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        assert!(result.contains("tracked.txt"));
        assert!(!result.contains("ignored.txt"));
    }
}
