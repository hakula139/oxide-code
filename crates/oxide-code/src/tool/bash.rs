use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use super::{Tool, ToolOutput};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

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
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let timeout = input.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_millis);

    match tokio::time::timeout(timeout, execute(&input.command)).await {
        Ok(output) => output,
        Err(_) => ToolOutput {
            content: format!("Command timed out after {}ms", timeout.as_millis()),
            is_error: true,
        },
    }
}

async fn execute(command: &str) -> ToolOutput {
    let result = Command::new("bash").arg("-c").arg(command).output().await;

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
        _ = write!(content, "Exit code: {code}");
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

/// Truncate output that exceeds [`MAX_OUTPUT_BYTES`](super::MAX_OUTPUT_BYTES),
/// keeping the first and last halves so the LLM sees both the beginning of the
/// output and the end (where error messages and summaries usually appear).
fn truncate_output(content: &mut String) {
    // The separator line is ~35 bytes; 50 gives headroom for large line counts.
    const TRUNCATION_OVERHEAD: usize = 50;

    if content.len() <= super::MAX_OUTPUT_BYTES {
        return;
    }

    let half = super::MAX_OUTPUT_BYTES / 2;
    let head_end = content.floor_char_boundary(half);
    let tail_start = content.floor_char_boundary(content.len() - half);

    // Only truncate if the omitted region is larger than the separator we
    // would insert — otherwise truncation makes the output longer.
    let omitted = &content[head_end..tail_start];
    if omitted.len() < TRUNCATION_OVERHEAD {
        return;
    }

    let omitted_lines = omitted.lines().count();

    let mut truncated = String::with_capacity(super::MAX_OUTPUT_BYTES + TRUNCATION_OVERHEAD);
    truncated.push_str(&content[..head_end]);
    _ = write!(truncated, "\n... ({omitted_lines} lines truncated) ...\n");
    truncated.push_str(&content[tail_start..]);

    *content = truncated;
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::super::MAX_OUTPUT_BYTES;
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
        assert_eq!(output.content, "Command timed out after 100ms");
    }

    // ── execute ──

    #[tokio::test]
    async fn execute_echo() {
        let output = execute("echo hello").await;
        assert!(!output.is_error);
        assert_eq!(output.content.trim(), "hello");
    }

    #[tokio::test]
    async fn execute_stderr_output() {
        let output = execute("echo err >&2").await;
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            indoc! {"
                STDERR:
                err
            "}
        );
    }

    #[tokio::test]
    async fn execute_combined_stdout_and_stderr() {
        let output = execute("echo out && echo err >&2").await;
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            indoc! {"
                out

                STDERR:
                err
            "}
        );
    }

    #[tokio::test]
    async fn execute_failing_command() {
        let output = execute("false").await;
        assert!(output.is_error);
        assert_eq!(output.content, "Exit code: 1");
    }

    #[tokio::test]
    async fn execute_output_with_nonzero_exit() {
        let output = execute("echo partial; false").await;
        assert!(output.is_error);
        assert_eq!(
            output.content,
            indoc! {"
                partial

                Exit code: 1"}
        );
    }

    #[tokio::test]
    async fn execute_no_output() {
        let output = execute("true").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "(no output)");
    }

    #[tokio::test]
    async fn execute_truncates_large_output() {
        let output = execute("echo HEAD_MARKER && yes | head -c 200000 && echo TAIL_MARKER").await;
        assert!(output.content.contains("lines truncated"));
        assert!(output.content.starts_with("HEAD_MARKER\n"));
        assert!(output.content.ends_with("TAIL_MARKER\n"));
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
        let head = "HEAD_SENTINEL\n";
        let tail = "TAIL_SENTINEL\n";
        let filler_len = MAX_OUTPUT_BYTES * 2 - head.len() - tail.len();
        let filler_lines = filler_len / 2; // "x\n" is 2 bytes each

        let mut content = String::with_capacity(head.len() + filler_len + tail.len());
        content.push_str(head);
        for _ in 0..filler_lines {
            content.push_str("x\n");
        }
        content.push_str(tail);

        truncate_output(&mut content);

        assert!(content.starts_with("HEAD_SENTINEL\n"));
        assert!(content.ends_with("TAIL_SENTINEL\n"));
        assert!(content.contains("lines truncated"));
        assert!(content.len() <= MAX_OUTPUT_BYTES + 100);
        assert!(content.len() >= MAX_OUTPUT_BYTES / 2);
        // Separator sits between head and tail, not at the edges
        let sep_pos = content.find("lines truncated").unwrap();
        assert!(sep_pos > head.len());
        assert!(sep_pos < content.len() - tail.len());
    }

    #[test]
    fn truncate_output_multibyte_at_split_boundary() {
        // Place a 4-byte emoji right at the split point to exercise
        // floor_char_boundary rounding down instead of splitting mid-char.
        let half = MAX_OUTPUT_BYTES / 2;
        let emoji = "🦀"; // 4 bytes
        let prefix_len = half - 2; // emoji straddles the half boundary

        let mut content = String::new();
        content.push_str(&"a".repeat(prefix_len));
        content.push_str(emoji);
        content.push_str(&"b".repeat(MAX_OUTPUT_BYTES * 2)); // enough to trigger truncation

        truncate_output(&mut content);

        assert!(content.contains("lines truncated"));
        assert!(content.starts_with("aaaa"));
        assert!(content.ends_with('b'));
    }

    #[test]
    fn truncate_output_barely_over_limit_unchanged() {
        let original = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let mut content = original.clone();
        truncate_output(&mut content);
        // Head and tail overlap — truncation would make it longer, so skip.
        assert_eq!(content, original);
    }
}
