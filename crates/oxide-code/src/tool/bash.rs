use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use super::{Tool, ToolMetadata, ToolOutput, extract_input_field, title_case};

/// Default per-command timeout — two minutes covers typical
/// compile / test cycles without letting a runaway command hold
/// the agent loop indefinitely.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(2);

/// Stand-in content when a command produced no stdout / stderr.
const NO_OUTPUT_MARKER: &str = "(no output)";

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
                "description": {
                    "type": "string",
                    "description": "A concise (5-10 word) description of what this command does"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                }
            },
            "required": ["command"]
        })
    }

    fn icon(&self) -> &'static str {
        "$"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "command")
    }

    /// Bash uses `$ <command>` as its visual identity — the dollar
    /// icon already reads as a shell prompt, so wrapping the command
    /// in `Bash(...)` would be redundant. When the `command` field is
    /// absent (malformed input — schema validation should catch this
    /// upstream) fall back to the default shape (`Bash`) so the UI
    /// still prints a readable label rather than a bare `$ `.
    fn summarize_call(&self, input: &serde_json::Value) -> String {
        extract_input_field(input, "command").map_or_else(|| title_case(self.name()), str::to_owned)
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
    description: Option<String>,
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

    let mut output = execute(&input.command, timeout).await;

    if let Some(desc) = input.description {
        output.metadata.title = Some(desc);
    }

    output
}

async fn execute(command: &str, timeout: Duration) -> ToolOutput {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    // Own process group so timeout can kill the whole tree, not just bash —
    // otherwise `(sleep 3600; ...) &` outlives the direct child.
    #[cfg(unix)]
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ToolOutput {
                content: format!("Failed to execute command: {e}"),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
    };

    #[cfg(unix)]
    let pgid = child.id();

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return ToolOutput {
                content: format!("Failed to execute command: {e}"),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
        Err(_) => {
            // kill_on_drop handles bash; killpg catches any backgrounded
            // grandchildren still in the same process group.
            #[cfg(unix)]
            kill_process_group(pgid);
            return ToolOutput {
                content: format!("Command timed out after {}ms", timeout.as_millis()),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
    };

    let exit_code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut content = String::new();
    if !stdout.is_empty() {
        let trimmed = stdout.trim_start_matches('\n').trim_end();
        content.push_str(trimmed);
    }
    if !stderr.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(stderr.trim());
    }
    if !output.status.success() {
        let code = exit_code.unwrap_or(-1);
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        _ = write!(content, "(exit code {code})");
    }
    if content.is_empty() {
        content.push_str(NO_OUTPUT_MARKER);
    }

    // Only flag execution failures (timeout, spawn error) as is_error.
    // Nonzero exit codes are informational — many commands use them normally
    // (grep returns 1 for no matches, diff returns 1 for differences, etc.).
    // The model can determine severity from the output content itself.
    ToolOutput {
        content,
        is_error: false,
        metadata: ToolMetadata {
            exit_code,
            ..ToolMetadata::default()
        },
    }
}

// ── Process Group Cleanup ──

/// Best-effort SIGKILL of an entire process group on Unix via the safe
/// `nix` wrapper around `killpg(2)`. Errors are ignored (`ESRCH` just
/// means the group already exited).
#[cfg(unix)]
fn kill_process_group(pgid: Option<u32>) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let Some(pgid) = pgid else { return };
    let Ok(pgid_signed) = i32::try_from(pgid) else {
        return;
    };
    _ = killpg(Pid::from_raw(pgid_signed), Signal::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── run ──

    #[tokio::test]
    async fn run_valid_command() {
        let output = run(serde_json::json!({"command": "echo hello"})).await;
        assert!(!output.is_error);
        assert_eq!(output.content, "hello");
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

    async fn run_default(cmd: &str) -> ToolOutput {
        execute(cmd, DEFAULT_TIMEOUT).await
    }

    #[tokio::test]
    async fn execute_echo() {
        let output = run_default("echo hello").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "hello");
    }

    #[tokio::test]
    async fn execute_stderr_output() {
        let output = run_default("echo err >&2").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "err");
    }

    #[tokio::test]
    async fn execute_combined_stdout_and_stderr() {
        let output = run_default("echo out && echo err >&2").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "out\nerr");
    }

    #[tokio::test]
    async fn execute_nonzero_exit_not_flagged_as_error() {
        let output = run_default("false").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "(exit code 1)");
    }

    #[tokio::test]
    async fn execute_output_with_nonzero_exit() {
        let output = run_default("echo partial; false").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "partial\n\n(exit code 1)");
    }

    #[tokio::test]
    async fn execute_no_output() {
        let output = run_default("true").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "(no output)");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_timeout_kills_backgrounded_children() {
        // A real shell command spawns a long-lived descendant and detaches.
        // Before the process-group fix, the descendant would outlive the
        // timeout and leak as an orphan. The test writes to a marker file
        // after 1 second; if the group is killed first, the marker is absent.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("leaked");
        let marker_str = marker.to_str().unwrap();

        let command = format!("(sleep 1 && touch {marker_str}) & sleep 5");
        let start = std::time::Instant::now();
        let output = execute(&command, Duration::from_millis(100)).await;
        assert!(output.is_error, "expected timeout");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "timeout did not return promptly",
        );

        // Give any leaked background process enough wallclock to touch the file.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            !marker.exists(),
            "backgrounded descendant was not killed: marker file was created",
        );
    }
}
