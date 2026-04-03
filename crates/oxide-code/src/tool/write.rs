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
    let input: Input = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(e) => {
            return ToolOutput {
                content: format!("Invalid input: {e}"),
                is_error: true,
            };
        }
    };

    match write_file(&input.file_path, &input.content).await {
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

async fn write_file(path: &str, content: &str) -> Result<String, String> {
    let file_path = std::path::Path::new(path);
    let is_new = !file_path.exists();

    // Create parent directories if needed
    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    tokio::fs::write(path, content)
        .await
        .map_err(|e| format!("Failed to write file: {e}"))?;

    if is_new {
        Ok(format!("File created successfully at: {path}"))
    } else {
        Ok(format!("File {path} has been updated successfully."))
    }
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

        let msg = write_file(path.to_str().unwrap(), "content").await.unwrap();
        assert!(msg.contains("created"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[tokio::test]
    async fn write_file_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, "old content").unwrap();

        let msg = write_file(path.to_str().unwrap(), "new content")
            .await
            .unwrap();
        assert!(msg.contains("updated"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.txt");

        write_file(path.to_str().unwrap(), "deep").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deep");
    }

    #[tokio::test]
    async fn write_file_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");

        write_file(path.to_str().unwrap(), "").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
