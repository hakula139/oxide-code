//! Shell command execution tool with timeout.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{Tool, ToolMetadata, ToolOutput, extract_input_field, title_case};

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(2);
const PIPE_CAPTURE_BYTES: usize = super::MAX_OUTPUT_BYTES;
const PIPE_READ_CHUNK_BYTES: usize = 8 * 1024;

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

    // Put the child in its own process group so a timeout can `killpg` the whole tree, including
    // any `cmd &`-style detached descendants the shell launched.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
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

    let stdout = child.stdout.take().expect("stdout configured as piped");
    let stderr = child.stderr.take().expect("stderr configured as piped");
    let stdout_task = tokio::spawn(read_capped_pipe(stdout));
    let stderr_task = tokio::spawn(read_capped_pipe(stderr));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return ToolOutput {
                content: format!("Failed to execute command: {e}"),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
        Err(_) => {
            // kill_on_drop handles bash; killpg catches detached grandchildren in the same group.
            #[cfg(unix)]
            kill_process_group(pgid);
            stdout_task.abort();
            stderr_task.abort();
            return ToolOutput {
                content: format!("Command timed out after {}ms", timeout.as_millis()),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
    };

    let stdout = match collect_pipe(stdout_task).await {
        Ok(output) => output,
        Err(e) => {
            return ToolOutput {
                content: format!("Failed to read command stdout: {e}"),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
    };
    let stderr = match collect_pipe(stderr_task).await {
        Ok(output) => output,
        Err(e) => {
            return ToolOutput {
                content: format!("Failed to read command stderr: {e}"),
                is_error: true,
                metadata: ToolMetadata::default(),
            };
        }
    };

    let exit_code = status.code();

    let mut content = String::new();
    let stdout = render_pipe(&stdout, true);
    let stderr = render_pipe(&stderr, false);
    if !stdout.is_empty() {
        content.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&stderr);
    }
    if !status.success() {
        let code = exit_code.unwrap_or(-1);
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        _ = write!(content, "(exit code {code})");
    }
    if content.is_empty() {
        content.push_str(NO_OUTPUT_MARKER);
    }

    // Nonzero exit is reported in the body, not as `is_error`: tools the model invokes (`grep`
    // returning 1 on no match, `test`-style probes) are expected to use the exit code as a signal.
    // Reserving `is_error` for spawn / timeout failures keeps the model from treating those probes
    // as hard failures.
    ToolOutput {
        content,
        is_error: false,
        metadata: ToolMetadata::default(),
    }
}

// ── Pipe Capture ──

#[derive(Debug, PartialEq, Eq)]
struct CapturedPipe {
    bytes: Vec<u8>,
    omitted: usize,
}

async fn read_capped_pipe<R>(mut reader: R) -> std::io::Result<CapturedPipe>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut omitted = 0;
    let mut buf = [0; PIPE_READ_CHUNK_BYTES];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let remaining = PIPE_CAPTURE_BYTES.saturating_sub(bytes.len());
        let keep = remaining.min(n);
        bytes.extend_from_slice(&buf[..keep]);
        omitted += n - keep;
    }
    Ok(CapturedPipe { bytes, omitted })
}

async fn collect_pipe(
    task: tokio::task::JoinHandle<std::io::Result<CapturedPipe>>,
) -> std::io::Result<CapturedPipe> {
    task.await.map_err(std::io::Error::other)?
}

fn render_pipe(pipe: &CapturedPipe, trim_start_newlines: bool) -> String {
    let text = String::from_utf8_lossy(&pipe.bytes);
    let mut text = text.trim_end();
    if trim_start_newlines {
        text = text.trim_start_matches('\n');
    } else {
        text = text.trim();
    }

    let mut rendered = text.to_owned();
    if pipe.omitted > 0 {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        _ = write!(
            rendered,
            "(stream truncated: {} bytes omitted)",
            pipe.omitted
        );
    }
    rendered
}

// ── Process Group Cleanup ──

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
    use tokio::io::AsyncWriteExt as _;

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
    async fn run_attaches_description_to_metadata_title() {
        let tool = BashTool;
        let output = tool
            .run(serde_json::json!({
                "command": "echo hello",
                "description": "Print greeting",
            }))
            .await;

        assert!(!output.is_error);
        assert_eq!(output.content, "hello");
        assert_eq!(output.metadata.title.as_deref(), Some("Print greeting"));
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

    #[tokio::test]
    async fn execute_large_output_is_bounded_while_pipe_is_drained() {
        let bytes = PIPE_CAPTURE_BYTES + 1024;
        let output = run_default(&format!("yes x | head -c {bytes}")).await;

        assert!(!output.is_error);
        assert!(
            output.content.len() <= PIPE_CAPTURE_BYTES + 128,
            "output should stay near the pipe cap, got {} bytes",
            output.content.len(),
        );
        assert!(output.content.contains("stream truncated"));
        assert!(output.content.contains("1024 bytes omitted"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_timeout_kills_backgrounded_children() {
        // Without the process-group kill, a detached descendant outlives the timeout and touches
        // the marker file. The marker's absence proves the whole group was killed.
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

    // ── render_pipe ──

    #[tokio::test]
    async fn read_capped_pipe_bounds_retained_bytes_and_counts_omitted() {
        let (mut writer, reader) = tokio::io::duplex(PIPE_CAPTURE_BYTES + 16);
        let input = vec![b'x'; PIPE_CAPTURE_BYTES + 16];
        let writer_task = tokio::spawn(async move {
            writer.write_all(&input).await.unwrap();
        });

        let pipe = read_capped_pipe(reader).await.unwrap();
        writer_task.await.unwrap();

        assert_eq!(pipe.bytes.len(), PIPE_CAPTURE_BYTES);
        assert!(pipe.bytes.iter().all(|&b| b == b'x'));
        assert_eq!(pipe.omitted, 16);
    }

    #[tokio::test]
    async fn collect_pipe_surfaces_join_failure() {
        let task =
            tokio::spawn(async { std::future::pending::<std::io::Result<CapturedPipe>>().await });
        task.abort();

        let err = collect_pipe(task)
            .await
            .expect_err("join error should surface");
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn render_pipe_appends_truncation_marker_after_captured_text() {
        let pipe = CapturedPipe {
            bytes: b"\nhello\n".to_vec(),
            omitted: 7,
        };

        assert_eq!(
            render_pipe(&pipe, true),
            "hello\n(stream truncated: 7 bytes omitted)",
        );
    }

    #[test]
    fn render_pipe_trims_stderr_edges_and_keeps_internal_newlines() {
        let pipe = CapturedPipe {
            bytes: b"\nerr\nmore\n\n".to_vec(),
            omitted: 0,
        };

        assert_eq!(render_pipe(&pipe, false), "err\nmore");
    }

    #[test]
    fn render_pipe_renders_marker_when_every_byte_was_omitted() {
        let pipe = CapturedPipe {
            bytes: Vec::new(),
            omitted: 3,
        };

        assert_eq!(
            render_pipe(&pipe, true),
            "(stream truncated: 3 bytes omitted)"
        );
    }
}
