use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use super::{Tool, ToolOutput};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

pub(crate) struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command and return its output."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                }
            },
            "required": ["command"]
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
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
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

    let timeout = input.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_millis);

    match tokio::time::timeout(timeout, execute(&input.command)).await {
        Ok(output) => output,
        Err(_) => ToolOutput {
            content: format!("Command timed out after {}s", timeout.as_secs()),
            is_error: true,
        },
    }
}

async fn execute(command: &str) -> ToolOutput {
    let result = Command::new("sh").arg("-c").arg(command).output().await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return ToolOutput {
                content: format!("Failed to execute command: {e}"),
                is_error: true,
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut content = String::new();

    if !stdout.is_empty() {
        content.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str("STDERR:\n");
        content.push_str(&stderr);
    }
    if !output.status.success() {
        if !content.is_empty() {
            content.push('\n');
        }
        let code = output.status.code().unwrap_or(-1);
        let _ = write!(content, "Exit code: {code}");
    }
    if content.is_empty() {
        content.push_str("(no output)");
    }

    truncate_output(&mut content);

    ToolOutput {
        content,
        is_error: !output.status.success(),
    }
}

/// Truncate output that exceeds [`MAX_OUTPUT_BYTES`], keeping the first and
/// last halves so the LLM sees both the beginning of the output and the end
/// (where error messages and summaries usually appear).
fn truncate_output(content: &mut String) {
    if content.len() <= MAX_OUTPUT_BYTES {
        return;
    }

    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = content.floor_char_boundary(half);
    let tail_start = content.floor_char_boundary(content.len() - half);

    // The separator line is ~35 bytes. Only truncate if the omitted region
    // is large enough that removing it actually saves space.
    let omitted = &content[head_end..tail_start];
    if omitted.len() < 50 {
        return;
    }

    let omitted_lines = omitted.lines().count();

    let mut truncated = String::with_capacity(MAX_OUTPUT_BYTES + 64);
    truncated.push_str(&content[..head_end]);
    let _ = write!(truncated, "\n... ({omitted_lines} lines truncated) ...\n");
    truncated.push_str(&content[tail_start..]);

    *content = truncated;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_valid_command() {
        let output = run(serde_json::json!({"command": "echo hello"})).await;
        assert!(!output.is_error);
        assert_eq!(output.content.trim(), "hello");
    }

    #[tokio::test]
    async fn run_missing_command_field() {
        let output = run(serde_json::json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_timeout() {
        let output = run(serde_json::json!({
            "command": "sleep 10",
            "timeout": 100
        }))
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("timed out"));
    }

    // ── execute ──

    #[tokio::test]
    async fn execute_echo() {
        let output = execute("echo hello").await;
        assert!(!output.is_error);
        assert_eq!(output.content.trim(), "hello");
    }

    #[tokio::test]
    async fn execute_failing_command() {
        let output = execute("false").await;
        assert!(output.is_error);
        assert!(output.content.contains("Exit code: 1"));
    }

    #[tokio::test]
    async fn execute_stderr_output() {
        let output = execute("echo err >&2").await;
        assert!(!output.is_error);
        assert!(output.content.contains("STDERR:"));
        assert!(output.content.contains("err"));
    }

    #[tokio::test]
    async fn execute_combined_stdout_and_stderr() {
        let output = execute("echo out && echo err >&2").await;
        assert!(!output.is_error);
        assert!(output.content.contains("out"));
        assert!(output.content.contains("STDERR:"));
        assert!(output.content.contains("err"));
    }

    #[tokio::test]
    async fn execute_output_with_nonzero_exit() {
        let output = execute("echo partial; false").await;
        assert!(output.is_error);
        assert!(output.content.contains("partial"));
        assert!(output.content.contains("Exit code: 1"));
    }

    #[tokio::test]
    async fn execute_no_output() {
        let output = execute("true").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "(no output)");
    }

    #[tokio::test]
    async fn execute_truncates_large_output() {
        let output = execute("yes | head -c 200000").await;
        assert!(output.content.contains("lines truncated"));
        // Head+tail truncation: keeps beginning and end
        assert!(output.content.starts_with("y\n"));
        assert!(output.content.ends_with("y\n"));
    }

    // ── truncate_output ──

    #[test]
    fn truncate_output_short_content_unchanged() {
        let mut content = "hello".to_owned();
        truncate_output(&mut content);
        assert_eq!(content, "hello");
    }

    #[test]
    fn truncate_output_keeps_head_and_tail() {
        let mut content = "x\n".repeat(100_000);
        truncate_output(&mut content);
        assert!(content.len() <= MAX_OUTPUT_BYTES + 100);
        assert!(content.starts_with("x\n"));
        assert!(content.ends_with("x\n"));
        assert!(content.contains("lines truncated"));
    }

    #[test]
    fn truncate_output_barely_over_limit_unchanged() {
        let mut content = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let original_len = content.len();
        truncate_output(&mut content);
        // Head and tail overlap — truncation would make it longer, so skip.
        assert_eq!(content.len(), original_len);
    }
}
