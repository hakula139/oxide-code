use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

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
    let input: Input = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(e) => {
            return ToolOutput {
                content: format!("Invalid input: {e}"),
                is_error: true,
            };
        }
    };

    let Input { pattern, path } = input;

    // glob crate is sync — run in a blocking task
    match tokio::task::spawn_blocking(move || glob_files(&pattern, path.as_deref())).await {
        Ok(Ok(content)) => ToolOutput {
            content,
            is_error: false,
        },
        Ok(Err(msg)) => ToolOutput {
            content: msg,
            is_error: true,
        },
        Err(e) => ToolOutput {
            content: format!("Internal error: {e}"),
            is_error: true,
        },
    }
}

fn glob_files(pattern: &str, search_dir: Option<&str>) -> Result<String, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("Failed to get working directory: {e}"))?;
    let base = search_dir.map_or_else(|| cwd.clone(), std::path::PathBuf::from);

    if !base.is_dir() {
        return Err(format!("Directory does not exist: {}", base.display()));
    }

    // Build the full glob pattern by joining base dir with the relative pattern
    let full_pattern = if std::path::Path::new(pattern).is_absolute() {
        pattern.to_owned()
    } else {
        format!("{}/{pattern}", base.display())
    };

    let entries = glob::glob(&full_pattern).map_err(|e| format!("Invalid glob pattern: {e}"))?;

    // Collect matching paths with their modification times
    let mut matches: Vec<(String, SystemTime)> = Vec::new();
    for entry in entries {
        let Ok(path) = entry else { continue };

        if !path.is_file() {
            continue;
        }

        let mtime = path
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let display_path = path.to_string_lossy().into_owned();
        matches.push((display_path, mtime));
    }

    // Sort by mtime descending (newest first)
    matches.sort_by(|a, b| b.1.cmp(&a.1));

    if matches.is_empty() {
        return Ok("No files found".into());
    }

    let truncated = matches.len() > MAX_RESULTS;
    let total = matches.len();
    matches.truncate(MAX_RESULTS);

    let mut output: String = matches
        .into_iter()
        .map(|(p, _)| p)
        .collect::<Vec<_>>()
        .join("\n");

    if truncated {
        let _ = write!(
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
    fn glob_files_sorted_by_mtime() {
        let dir = tempfile::tempdir().unwrap();

        // Create files with slight time differences
        std::fs::write(dir.path().join("old.txt"), "old").unwrap();
        // Touch to ensure mtime differs
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.path().join("new.txt"), "new").unwrap();

        let result = glob_files("*.txt", Some(dir.path().to_str().unwrap())).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2);
        // Newest file should be first
        assert!(lines[0].contains("new.txt"));
        assert!(lines[1].contains("old.txt"));
    }
}
