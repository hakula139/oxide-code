use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolOutput};

pub(crate) struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Perform exact string replacement in a file. \
         The old_string must be unique in the file unless replace_all is true."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace (must be unique unless replace_all is true)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must differ from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
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
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
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

    match edit_file(
        &input.file_path,
        &input.old_string,
        &input.new_string,
        input.replace_all,
    )
    .await
    {
        Ok(msg) => ToolOutput {
            content: msg,
            is_error: false,
        },
        Err(msg) => ToolOutput {
            content: msg,
            is_error: true,
        },
    }
}

async fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty.".into());
    }

    if old_string == new_string {
        return Err("old_string and new_string are identical. No changes to make.".into());
    }

    let raw_content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("Error reading {path}: {e}"))?;

    // Normalize CRLF → LF for matching, but keep original bytes for write-back
    // so we don't silently convert a file's line endings.
    let normalized = raw_content.replace("\r\n", "\n");

    let match_count = normalized.matches(old_string).count();

    if match_count == 0 {
        return Err(format!(
            "old_string not found in {path}. Make sure the string matches exactly, \
             including whitespace and indentation."
        ));
    }

    if match_count > 1 && !replace_all {
        return Err(format!(
            "Found {match_count} occurrences of old_string in {path}. \
             Set replace_all to true to replace all, or provide more context \
             to make old_string unique."
        ));
    }

    // Apply the replacement on normalized content, then restore original
    // line endings if the file used CRLF.
    let updated = if replace_all {
        normalized.replace(old_string, new_string)
    } else {
        normalized.replacen(old_string, new_string, 1)
    };

    let has_crlf = raw_content.contains("\r\n");
    let final_content = if has_crlf {
        updated.replace('\n', "\r\n")
    } else {
        updated
    };

    tokio::fs::write(path, &final_content)
        .await
        .map_err(|e| format!("Failed to write {path}: {e}"))?;

    if replace_all && match_count > 1 {
        Ok(format!("Replaced {match_count} occurrences in {path}."))
    } else {
        Ok(format!("Successfully edited {path}."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_valid_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let output = run(serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "hello",
            "new_string": "goodbye"
        }))
        .await;

        assert!(!output.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn run_missing_required_fields() {
        let output = run(serde_json::json!({
            "file_path": "/tmp/x",
            "old_string": "a"
        }))
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    // ── edit_file ──

    #[tokio::test]
    async fn edit_file_replaces_unique_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "fn foo() {}\nfn bar() {}\n").unwrap();

        edit_file(
            path.to_str().unwrap(),
            "fn foo() {}",
            "fn foo() -> i32 { 42 }",
            false,
        )
        .await
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "fn foo() -> i32 { 42 }\nfn bar() {}\n");
    }

    #[tokio::test]
    async fn edit_file_rejects_empty_old_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(path.to_str().unwrap(), "", "x", false)
            .await
            .unwrap_err();
        assert!(err.contains("must not be empty"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn edit_file_rejects_identical_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = edit_file(path.to_str().unwrap(), "hello", "hello", false)
            .await
            .unwrap_err();
        assert!(err.contains("identical"));
    }

    #[tokio::test]
    async fn edit_file_rejects_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let err = edit_file(path.to_str().unwrap(), "nonexistent", "replacement", false)
            .await
            .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_rejects_nonexistent_file() {
        let err = edit_file("/nonexistent/file.txt", "a", "b", false)
            .await
            .unwrap_err();
        assert!(err.contains("Error reading"));
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let err = edit_file(path.to_str().unwrap(), "aaa", "ccc", false)
            .await
            .unwrap_err();
        assert!(err.contains("2 occurrences"));
    }

    #[tokio::test]
    async fn edit_file_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let msg = edit_file(path.to_str().unwrap(), "aaa", "ccc", true)
            .await
            .unwrap();

        assert!(msg.contains("2 occurrences"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ccc bbb ccc");
    }

    #[tokio::test]
    async fn edit_file_crlf_matching_preserves_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "line1\r\nline2\r\n").unwrap();

        edit_file(path.to_str().unwrap(), "line1\nline2", "a\nb", false)
            .await
            .unwrap();

        // CRLF line endings should be preserved in the output
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, b"a\r\nb\r\n");
    }

    #[tokio::test]
    async fn edit_file_replace_all_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let msg = edit_file(path.to_str().unwrap(), "hello", "goodbye", true)
            .await
            .unwrap();

        assert!(msg.contains("Successfully edited"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }
}
