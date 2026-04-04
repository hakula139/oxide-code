use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

use super::{Tool, ToolOutput};

pub(crate) struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, creating it if it does not exist or overwriting if it does."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
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
    content: String,
}

// ── Execution ──

async fn run(raw: serde_json::Value) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let (result, is_new) = write_file(&input.file_path, &input.content).await;
    let name = file_name(&input.file_path);
    let verb = if is_new { "Created" } else { "Updated" };
    ToolOutput::from_result(result).with_title(format!("{verb} {name}"))
}

fn file_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

async fn write_file(path: &str, content: &str) -> (Result<String, String>, bool) {
    let file_path = std::path::Path::new(path);
    let is_new = matches!(
        tokio::fs::metadata(path).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    );

    if let Some(parent) = file_path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return (Err(format!("Failed to create directory: {e}")), is_new);
    }

    if let Err(e) = tokio::fs::write(path, content).await {
        return (Err(format!("Failed to write file: {e}")), is_new);
    }

    let msg = if is_new {
        format!("Successfully created {path}.")
    } else {
        format!("Successfully updated {path}.")
    };
    (Ok(msg), is_new)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let output = run(serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "content": "hello world"
        }))
        .await;

        assert!(!output.is_error);
        assert!(output.content.contains("created"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn run_missing_required_fields() {
        let output = run(serde_json::json!({"file_path": "/tmp/x"})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    // ── write_file ──

    #[tokio::test]
    async fn write_file_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let (result, is_new) = write_file(path.to_str().unwrap(), "content").await;
        assert!(result.unwrap().contains("created"));
        assert!(is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[tokio::test]
    async fn write_file_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let (result, is_new) = write_file(path.to_str().unwrap(), "new content").await;
        assert!(result.unwrap().contains("updated"));
        assert!(!is_new);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt");

        let (result, _) = write_file(path.to_str().unwrap(), "deep").await;
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep");
    }

    #[tokio::test]
    async fn write_file_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");

        let (result, _) = write_file(path.to_str().unwrap(), "").await;
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[tokio::test]
    async fn write_file_fails_when_parent_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "I am a file").unwrap();

        let path = blocker.join("child.txt");
        let (result, _) = write_file(path.to_str().unwrap(), "content").await;
        assert!(result.unwrap_err().contains("Failed to create directory"));
    }

    #[tokio::test]
    async fn write_file_fails_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (result, _) = write_file(dir.path().to_str().unwrap(), "content").await;
        assert!(result.unwrap_err().contains("Failed to write file"));
    }
}
