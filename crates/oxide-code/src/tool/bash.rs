//! Shell command execution tool with timeout.

pub(crate) mod background;

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;
#[cfg(all(unix, not(target_os = "linux")))]
use std::time::Instant;

use indoc::formatdoc;
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStderr, ChildStdout};
#[cfg(all(unix, not(target_os = "linux")))]
use tracing::warn;

use self::background::{BackgroundShells, ShellState};
use super::{
    ApprovalBody, ApprovalPreview, Prepared, Tool, ToolCtx, ToolOutput, ToolSpec,
    extract_input_field, pascal_case,
};
use crate::permission::{Access, PermissionClass};

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_TIMEOUT: Duration = Duration::from_mins(10);
/// How long a drain waits for the pipes to close once the shell has exited or its tree is gone.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// The variable each command's environment carries the session ID in, so a script it runs can tell
/// which session started it.
const SESSION_ID_VAR: &str = "OX_SESSION_ID";

/// Bytes of pipe output one call renders. The reserve below the dispatcher cap holds the omission
/// markers, the stream separator, and the footer, so the dispatcher never re-truncates bash output.
const OUTPUT_BUDGET: usize = super::MAX_OUTPUT_BYTES - 256;
const PIPE_HEAD_BYTES: usize = OUTPUT_BUDGET / 2;
const PIPE_TAIL_BYTES: usize = OUTPUT_BUDGET - PIPE_HEAD_BYTES;
const PIPE_READ_CHUNK_BYTES: usize = 8 * 1024;

const NO_OUTPUT_MARKER: &str = "(no output)";

#[cfg_attr(test, derive(Default))]
pub(crate) struct BashTool {
    shells: Arc<BackgroundShells>,
}

impl BashTool {
    pub(crate) fn new(shells: Arc<BackgroundShells>) -> Self {
        Self { shells }
    }
}

impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_owned(),
            description: "Execute a shell command and return its output. For a dev server, a \
                 watcher, or a long build, set run_in_background, without a trailing `&`: the \
                 call returns a shell ID at once. You are not notified when the command \
                 finishes, so check on it with bash_output, and stop it with bash_stop. A \
                 background shell stays open, running or finished, until bash_stop stops it or \
                 the session ends."
                .to_owned(),
            input_schema: input_schema(),
            class: PermissionClass::Exec,
            parallel_safe: false,
        }
    }

    fn prepare(&self, input: serde_json::Value, ctx: &ToolCtx) -> Prepared {
        let access = extract_input_field(&input, "command")
            .map(|command| Access::for_command(command, &ctx.workspace))
            .unwrap_or_default();
        Prepared { input, access }
    }

    fn icon(&self) -> &'static str {
        "$"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "command")
    }

    fn summarize_call(&self, name: &str, input: &serde_json::Value, _cwd: &Path) -> String {
        extract_input_field(input, "command").map_or_else(|| pascal_case(name), str::to_owned)
    }

    fn approval_preview(
        &self,
        name: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> ApprovalPreview {
        let background = input
            .get("run_in_background")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let label = pascal_case(name);
        ApprovalPreview {
            title: if background {
                format!("{label} (background)")
            } else {
                label
            },
            body: ApprovalBody::Command(self.summarize_call(name, input, cwd)),
        }
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(run(&self.shells, prepared.input, ctx))
    }
}

// ── Input ──

fn input_schema() -> serde_json::Value {
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
                "description": "Timeout in milliseconds, at most 600000. Omit or pass 0 for the default of 120000."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Start the command as a background shell and return its ID without waiting. The timeout field does not apply."
            }
        },
        "required": ["command"]
    })
}

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
}

// ── Execution ──

async fn run(shells: &Arc<BackgroundShells>, raw: serde_json::Value, ctx: &ToolCtx) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let mut output = if input.run_in_background {
        background::start(shells, &input.command, ctx)
    } else {
        execute(&input.command, resolve_timeout(input.timeout), ctx).await
    };

    if let Some(desc) = input.description {
        output.metadata.title = Some(desc);
    }

    output
}

fn resolve_timeout(millis: Option<u64>) -> Duration {
    match millis {
        None | Some(0) => DEFAULT_TIMEOUT,
        Some(ms) => Duration::from_millis(ms).min(MAX_TIMEOUT),
    }
}

/// Runs `command` in the context's cwd, with [`SESSION_ID_VAR`] set to its session ID.
async fn execute(command: &str, timeout: Duration, ctx: &ToolCtx) -> ToolOutput {
    let mut shell = match ShellTree::spawn(command, ctx) {
        Ok(shell) => shell,
        Err(e) => {
            return ToolOutput::error(format!(
                "Failed to execute command in {}: {e}",
                ctx.cwd().display()
            ));
        }
    };

    let (mut stdout, mut stderr) = shell.take_pipes();
    let mut stdout_capture = PipeCapture::default();
    let mut stderr_capture = PipeCapture::default();

    // One deadline covers the exit and the pipe drain, so a backgrounded descendant that holds the
    // pipes open cannot stretch the call past the timeout.
    let finished = tokio::time::timeout(timeout, async {
        tokio::join!(
            shell.wait(),
            stdout_capture.drain(&mut stdout),
            stderr_capture.drain(&mut stderr),
        )
    })
    .await;

    let Ok((status, stdout_read, stderr_read)) = finished else {
        // Killing the tree closes its pipe ends, so the grace drain only waits on a holder that
        // escaped it.
        shell.kill().await;
        _ = tokio::time::timeout(DRAIN_GRACE, async {
            tokio::join!(
                stdout_capture.drain(&mut stdout),
                stderr_capture.drain(&mut stderr),
            )
        })
        .await;
        return timeout_output(timeout, &stdout_capture, &stderr_capture);
    };

    let status = match status {
        Ok(status) => status,
        Err(e) => {
            return ToolOutput::error(format!("Failed to execute command: {e}"));
        }
    };
    if let Err(e) = stdout_read {
        return ToolOutput::error(format!("Failed to read command stdout: {e}"));
    }
    if let Err(e) = stderr_read {
        return ToolOutput::error(format!("Failed to read command stderr: {e}"));
    }
    // Descendants that released the pipes, such as a redirected background server, keep running.
    shell.disarm();

    let mut content = render_streams(&stdout_capture, &stderr_capture);
    if !status.success() {
        push_footer(&mut content, &exit_footer(status));
    }
    if content.is_empty() {
        content.push_str(NO_OUTPUT_MARKER);
    }

    // A nonzero exit stays in the body because commands like `grep` (1 on no match) and `test`
    // probes signal through their exit code. Reserving `is_error` for spawn, pipe, and timeout
    // failures keeps the model from treating those probes as hard failures.
    ToolOutput::success(content)
}

fn timeout_output(timeout: Duration, stdout: &PipeCapture, stderr: &PipeCapture) -> ToolOutput {
    let mut content = render_streams(stdout, stderr);
    push_footer(
        &mut content,
        &format!("Command timed out after {} ms", timeout.as_millis()),
    );
    ToolOutput::error(content)
}

fn exit_footer(status: ExitStatus) -> String {
    format!("({})", ShellState::of(status).describe())
}

// ── Shell Resolution ──

#[cfg(not(windows))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "shares the Windows signature, where Git Bash can be missing"
)]
fn bash() -> io::Result<&'static Path> {
    Ok(Path::new("bash"))
}

/// Git for Windows' bash, looked up once. `Command::new("bash")` searches System32 before `PATH`
/// there and finds the WSL launcher, so a missing Git Bash is an error.
#[cfg(windows)]
fn bash() -> io::Result<&'static Path> {
    static GIT_BASH: std::sync::LazyLock<Option<std::path::PathBuf>> =
        std::sync::LazyLock::new(|| git_bash_on(&std::env::var_os("PATH")?));
    GIT_BASH.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Git Bash not found. Install Git for Windows and put git.exe on PATH.",
        )
    })
}

/// Finds `bin\bash.exe` under the Git for Windows root of a `git.exe` on `path`, which sits in the
/// root's `cmd\` or `bin\`, or in `mingw64\bin\` inside a Git Bash session.
#[cfg(windows)]
fn git_bash_on(path: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.join("git.exe").is_file())
        .find_map(|dir| {
            dir.ancestors()
                .skip(1)
                .take(2)
                .map(|root| root.join(r"bin\bash.exe"))
                .find(|bash| bash.is_file())
        })
}

// ── Output Rendering ──

fn render_streams(stdout: &PipeCapture, stderr: &PipeCapture) -> String {
    let (stdout_budget, stderr_budget) = split_budget(stdout.kept(), stderr.kept(), OUTPUT_BUDGET);
    let stdout = render_pipe(stdout, stdout_budget);
    let stdout = stdout.trim_start_matches('\n');
    let stderr = render_pipe(stderr, stderr_budget);
    let stderr = stderr.trim_start();
    match (stdout.is_empty(), stderr.is_empty()) {
        (_, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn push_footer(content: &mut String, footer: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(footer);
}

/// Splits `budget` between parts wanting `first` and `second` bytes. Each gets up to half, and a
/// part needing less hands its unused share to the other.
fn split_budget(first: usize, second: usize, budget: usize) -> (usize, usize) {
    let second = second.min(budget - first.min(budget / 2));
    (first.min(budget - second), second)
}

/// Renders at most `budget` bytes of `pipe`, folding everything it drops into the omission count.
fn render_pipe(pipe: &PipeCapture, budget: usize) -> String {
    let tail: Vec<u8> = pipe.tail.iter().copied().collect();
    let (head_len, tail_len) = split_budget(pipe.head.len(), tail.len(), budget);
    let mut head = &pipe.head[..head_len];
    let mut tail = &tail[tail.len() - tail_len..];
    let mut omitted = pipe.omitted + (pipe.head.len() - head_len) + (pipe.tail.len() - tail_len);

    let text = if omitted == 0 {
        String::from_utf8_lossy(&[head, tail].concat()).into_owned()
    } else {
        // Either cut can split a multibyte char, and its stray bytes would decode as U+FFFD.
        let split_char = head.utf8_chunks().last().map_or(0, |c| c.invalid().len());
        let continuations = tail
            .iter()
            .take(3)
            .take_while(|&&b| b & 0xC0 == 0x80)
            .count();
        head = &head[..head.len() - split_char];
        tail = &tail[continuations..];
        omitted += split_char + continuations;
        formatdoc! {"
            {}
            ... [{omitted} bytes omitted] ...
            {}",
            String::from_utf8_lossy(head),
            String::from_utf8_lossy(tail),
        }
    };

    text.trim_end().to_owned()
}

// ── Pipe Capture ──

/// Bounded capture of one pipe: the first [`PIPE_HEAD_BYTES`] and the last [`PIPE_TAIL_BYTES`],
/// with the bytes dropped between them counted in `omitted`.
#[derive(Debug, Default)]
struct PipeCapture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: usize,
}

impl PipeCapture {
    /// Reads `reader` to EOF. Cancel-safe: each chunk is recorded as soon as it is read, so a drain
    /// cut short by the timeout keeps everything that arrived, and a later call resumes it.
    async fn drain<R>(&mut self, reader: &mut R) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        read_chunks(reader, |chunk| self.push(chunk)).await
    }

    fn kept(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    fn push(&mut self, chunk: &[u8]) {
        let head_room = PIPE_HEAD_BYTES.saturating_sub(self.head.len());
        let (head, rest) = chunk.split_at(head_room.min(chunk.len()));
        self.head.extend_from_slice(head);
        self.tail.extend(rest);

        let overflow = self.tail.len().saturating_sub(PIPE_TAIL_BYTES);
        self.tail.drain(..overflow);
        self.omitted += overflow;
    }
}

async fn read_chunks<R>(reader: &mut R, mut record: impl FnMut(&[u8])) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0; PIPE_READ_CHUNK_BYTES];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        record(&buf[..n]);
    }
}

// ── Shell Process Tree ──

/// The shell and every descendant it starts, held in a process group on Unix and a Job Object on
/// Windows. Dropping it while armed kills the whole tree, so a timeout, failure, or cancelled call
/// leaves no descendant running. A finished call disarms it.
struct ShellTree {
    child: Box<dyn ChildWrapper>,
    armed: bool,
}

impl ShellTree {
    fn spawn(command: &str, ctx: &ToolCtx) -> io::Result<Self> {
        let mut cmd = CommandWrap::with_new(bash()?, |cmd| {
            cmd.arg("-c")
                .arg(command)
                .current_dir(ctx.cwd())
                .env(SESSION_ID_VAR, &ctx.session_id)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        #[cfg(unix)]
        cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        cmd.wrap(process_wrap::tokio::JobObject);
        Ok(Self {
            child: cmd.spawn()?,
            armed: true,
        })
    }

    fn take_pipes(&mut self) -> (ChildStdout, ChildStderr) {
        let stdout = self.child.stdout().take();
        let stderr = self.child.stderr().take();
        (
            stdout.expect("stdout configured as piped"),
            stderr.expect("stderr configured as piped"),
        )
    }

    /// Waits for the shell alone, since the tree wrapper's own wait also outlasts detached
    /// descendants on Windows.
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.inner_mut().wait().await
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(any(target_os = "linux", windows))]
impl ShellTree {
    /// One call reaches every member. Linux checks for a fatal signal and attaches a forked child
    /// to its group under the lock a group signal holds, and terminating a Job Object ends all of
    /// its processes.
    #[expect(
        clippy::unused_async,
        reason = "shares the signature of the kill that retries on other Unix systems"
    )]
    async fn kill(&mut self) {
        self.kill_blocking();
    }

    fn kill_blocking(&mut self) {
        self.armed = false;
        _ = self.child.start_kill();
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
impl ShellTree {
    /// How long a kill keeps signaling members that `SIGKILL` cannot end yet, such as one in an
    /// uninterruptible read.
    const KILL_LIMIT: Duration = Duration::from_secs(1);
    const KILL_RETRY_DELAY: Duration = Duration::from_millis(1);

    /// Signals the group until no member it may signal is alive, since kernels other than Linux,
    /// such as XNU, can miss a child forked during a `killpg`. On XNU, such a child joins the group
    /// early but cannot receive signals until its fork finishes. A member still forking counts as
    /// alive, so no child can join after the last signal. The tree stays armed until then, so
    /// dropping the call mid-kill finishes the kill in `Drop`.
    async fn kill(&mut self) {
        let deadline = Instant::now() + Self::KILL_LIMIT;
        while self.signal(deadline) {
            tokio::time::sleep(Self::KILL_RETRY_DELAY).await;
        }
        self.armed = false;
    }

    /// Kills the tree as [`Self::kill`] does, blocking the thread between signals, for `Drop`.
    fn kill_blocking(&mut self) {
        self.armed = false;
        let deadline = Instant::now() + Self::KILL_LIMIT;
        while self.signal(deadline) {
            std::thread::sleep(Self::KILL_RETRY_DELAY);
        }
    }

    /// Signals the group and says whether to signal it again. `killpg` fails once no member it
    /// may signal is alive: `EPERM` when only zombies or members it cannot signal remain, `ESRCH`
    /// once the group is gone.
    fn signal(&mut self, deadline: Instant) -> bool {
        if self.child.start_kill().is_err() {
            return false;
        }
        if Instant::now() < deadline {
            return true;
        }
        let limit = Self::KILL_LIMIT.as_secs();
        warn!("stopped killing a bash process tree still alive after {limit} s of SIGKILL");
        false
    }
}

impl Drop for ShellTree {
    fn drop(&mut self) {
        if self.armed {
            self.kill_blocking();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use indoc::indoc;
    use tokio::io::AsyncWriteExt as _;

    use super::*;
    use crate::tool::MAX_OUTPUT_BYTES;
    use crate::tool::testing::{self, temp_ctx};
    #[cfg(unix)]
    use crate::tool::testing::{canonical_tempdir, ctx};

    // ── prepare ──

    #[test]
    fn prepare_extracts_the_command() {
        let ctx = temp_ctx();
        let prepared = BashTool::default().prepare(serde_json::json!({"command": "ls -la"}), &ctx);
        assert_eq!(
            prepared.access,
            Access::for_command("ls -la", &ctx.workspace)
        );
    }

    #[test]
    fn prepare_missing_command_is_none() {
        // A call with no `command` field falls to `None`, which only a tool-wide rule matches, so a
        // malformed bash call cannot slip past a command-specific allow.
        let prepared = BashTool::default().prepare(serde_json::json!({}), &temp_ctx());
        assert!(matches!(prepared.access, Access::None));
    }

    // ── approval_preview ──

    #[test]
    fn approval_preview_marks_a_background_command_in_the_title() {
        let tool = BashTool::default();
        let cwd = Path::new("/repo");

        let foreground =
            tool.approval_preview("bash", &serde_json::json!({"command": "cargo test"}), cwd);
        let background = tool.approval_preview(
            "bash",
            &serde_json::json!({"command": "cargo watch", "run_in_background": true}),
            cwd,
        );

        assert_eq!(foreground.title, "Bash");
        assert_eq!(
            foreground.body,
            ApprovalBody::Command("cargo test".to_owned())
        );
        assert_eq!(background.title, "Bash (background)");
        assert_eq!(
            background.body,
            ApprovalBody::Command("cargo watch".to_owned())
        );
    }

    // ── run ──

    #[tokio::test]
    async fn run_valid_command_executes_successfully() {
        let output = run(
            &Arc::default(),
            serde_json::json!({"command": "echo hello"}),
            &temp_ctx(),
        )
        .await;
        assert!(!output.is_error);
        assert_eq!(output.content, "hello");
    }

    #[tokio::test]
    async fn run_missing_command_field_reports_input_error() {
        let output = run(&Arc::default(), serde_json::json!({}), &temp_ctx()).await;
        assert!(output.is_error);
        assert!(output.content.contains("Invalid input"));
    }

    #[tokio::test]
    async fn run_attaches_description_to_metadata_title() {
        let input = serde_json::json!({
            "command": "echo hello",
            "description": "Print greeting",
        });

        let output = testing::run(&BashTool::default(), input, &temp_ctx()).await;

        assert!(!output.is_error);
        assert_eq!(output.content, "hello");
        assert_eq!(output.metadata.title.as_deref(), Some("Print greeting"));
    }

    #[tokio::test]
    async fn run_timeout_keeps_partial_output() {
        // The timeout leaves bash ample time to print before the kill, even under parallel load.
        let output = run(
            &Arc::default(),
            serde_json::json!({
                "command": "echo started; sleep 10",
                "timeout": 1_000
            }),
            &temp_ctx(),
        )
        .await;
        assert!(output.is_error);
        assert_eq!(
            output.content,
            indoc! {"
                started

                Command timed out after 1000 ms"}
        );
    }

    // ── resolve_timeout ──

    #[test]
    fn resolve_timeout_defaults_zero_and_caps_the_ceiling() {
        assert_eq!(resolve_timeout(None), DEFAULT_TIMEOUT);
        assert_eq!(resolve_timeout(Some(0)), DEFAULT_TIMEOUT);
        assert_eq!(resolve_timeout(Some(5_000)), Duration::from_secs(5));
        assert_eq!(resolve_timeout(Some(u64::MAX)), MAX_TIMEOUT);
    }

    // ── execute ──

    async fn run_default(cmd: &str) -> ToolOutput {
        execute(cmd, DEFAULT_TIMEOUT, &temp_ctx()).await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_runs_in_the_ctx_cwd_with_the_session_id_exported() {
        let dir = canonical_tempdir();
        assert_ne!(std::env::current_dir().unwrap(), dir.path());

        let output = execute(
            r#"pwd -P; printf %s "$OX_SESSION_ID""#,
            DEFAULT_TIMEOUT,
            &ctx(dir.path()),
        )
        .await;

        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            output.content,
            format!("{}\ntest-session", dir.path().display()),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_in_a_removed_cwd_names_it_in_the_error() {
        let dir = canonical_tempdir();
        let gone = dir.path().join("gone");

        let output = execute("true", DEFAULT_TIMEOUT, &ctx(&gone)).await;

        assert!(output.is_error);
        assert_eq!(
            output.content,
            format!(
                "Failed to execute command in {}: No such file or directory (os error 2)",
                gone.display()
            ),
        );
    }

    #[tokio::test]
    async fn execute_stderr_output_preserves_content() {
        let output = run_default("echo err >&2").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "err");
    }

    #[tokio::test]
    async fn execute_combined_stdout_and_stderr_renders_both_streams() {
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
    async fn execute_nonzero_exit_preserves_output() {
        let output = run_default("echo partial; false").await;
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            indoc! {"
                partial

                (exit code 1)"}
        );
    }

    #[tokio::test]
    async fn execute_no_output_renders_the_no_output_marker() {
        let output = run_default("true").await;
        assert!(!output.is_error);
        assert_eq!(output.content, "(no output)");
    }

    #[tokio::test]
    async fn execute_large_output_keeps_head_and_tail() {
        // `cargo build 2>&1` puts its error summary last, so the tail must survive the pipe cap.
        let filler = PIPE_HEAD_BYTES + PIPE_TAIL_BYTES + 1024;
        let output = run_default(&format!("echo HEAD; yes x | head -c {filler}; echo TAIL")).await;

        assert!(!output.is_error);
        assert!(output.content.starts_with("HEAD\nx\n"), "head kept");
        assert!(output.content.ends_with("x\nTAIL"), "tail kept");
        let omitted = "HEAD\n".len() + filler + "TAIL\n".len() - PIPE_HEAD_BYTES - PIPE_TAIL_BYTES;
        assert!(
            output
                .content
                .contains(&format!("\n... [{omitted} bytes omitted] ...\n")),
            "middle marker counts the dropped bytes",
        );
        assert!(output.content.len() <= PIPE_HEAD_BYTES + PIPE_TAIL_BYTES + 64);
    }

    #[tokio::test]
    async fn execute_timeout_kills_backgrounded_children() {
        // Without the tree kill, a detached descendant outlives the timeout and touches the marker
        // file. The marker's absence proves the whole tree was killed.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("leaked");
        let marker_str = marker.to_str().unwrap();

        let command = format!("(sleep 1 && touch '{marker_str}') & sleep 5");
        let start = std::time::Instant::now();
        let output = execute(&command, Duration::from_millis(100), &temp_ctx()).await;
        assert!(output.is_error, "expected timeout");
        assert_eq!(output.content, "Command timed out after 100 ms");
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

    #[tokio::test]
    async fn execute_cancelled_mid_run_kills_backgrounded_children() {
        // The shell either still runs or has exited while a descendant holds its pipes.
        for tail in ["sleep 30", "echo done"] {
            let dir = tempfile::tempdir().unwrap();
            let started = dir.path().join("started");
            let leaked = dir.path().join("leaked");
            let command = format!(
                "(touch '{started}'; sleep 1 && touch '{leaked}') & {tail}",
                started = started.display(),
                leaked = leaked.display(),
            );

            let ctx = temp_ctx();
            let mut call = Box::pin(execute(&command, DEFAULT_TIMEOUT, &ctx));
            // Spawning bash on a loaded Windows runner can take several seconds.
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !started.exists() && std::time::Instant::now() < deadline {
                let still_running = tokio::time::timeout(Duration::from_millis(20), call.as_mut())
                    .await
                    .is_err();
                assert!(still_running, "{tail}: command finished before the cancel");
            }
            assert!(started.exists(), "{tail}: command never started");
            drop(call);

            tokio::time::sleep(Duration::from_millis(1_500)).await;
            assert!(
                !leaked.exists(),
                "{tail}: backgrounded descendant outlived the cancel",
            );
        }
    }

    #[tokio::test]
    async fn execute_finished_call_keeps_descendants_detached_from_its_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("kept");
        let marker_str = marker.to_str().unwrap();

        let command = format!("(sleep 1 && touch '{marker_str}') >/dev/null 2>&1 & echo started");
        let output = run_default(&command).await;
        assert_eq!(output.content, "started");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(marker.exists(), "detached descendant was killed");
    }

    #[tokio::test]
    async fn execute_timeout_covers_background_pipe_holders() {
        // The backgrounded `sleep` holds the pipes open past bash's exit. The timeout leaves bash
        // ample time to print first, even under parallel load.
        let start = std::time::Instant::now();
        let output = execute(
            "sleep 5 & echo done",
            Duration::from_millis(1_000),
            &temp_ctx(),
        )
        .await;

        assert!(output.is_error, "expected timeout");
        assert_eq!(
            output.content,
            indoc! {"
                done

                Command timed out after 1000 ms"}
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timeout did not cover pipe collection",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_signal_death_names_the_signal() {
        let output = run_default("echo before; kill -9 $$").await;
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            indoc! {"
                before

                (killed by signal 9)"}
        );
    }

    // ── git_bash_on ──

    #[cfg(windows)]
    #[test]
    fn git_bash_on_finds_bash_under_the_git_root_of_any_git_on_path() {
        let dir = tempfile::tempdir().unwrap();
        // A shim outside any Git root, then the `mingw64\bin` and `cmd` copies a Git Bash session
        // puts on `PATH`.
        let [shims, mingw, cmd, bin] =
            [r"shims", r"Git\mingw64\bin", r"Git\cmd", r"Git\bin"].map(|d| dir.path().join(d));
        for d in [&shims, &mingw, &cmd, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        for d in [&shims, &mingw, &cmd] {
            std::fs::write(d.join("git.exe"), "").unwrap();
        }
        let path = |entries: &[&std::path::PathBuf]| std::env::join_paths(entries).unwrap();
        assert_eq!(git_bash_on(&path(&[&shims, &cmd])), None, "no bash.exe yet");

        let bash = bin.join("bash.exe");
        std::fs::write(&bash, "").unwrap();
        for git_dir in [&mingw, &cmd] {
            assert_eq!(git_bash_on(&path(&[&shims, git_dir])).as_ref(), Some(&bash));
        }
        assert_eq!(git_bash_on(&path(&[&shims])), None);
    }

    // ── render_streams ──

    /// Reads the byte count out of each `[N bytes omitted]` marker in `content`.
    fn omitted_counts(content: &str) -> Vec<usize> {
        content
            .split("... [")
            .skip(1)
            .map(|rest| rest[..rest.find(' ').unwrap()].parse().unwrap())
            .collect()
    }

    #[test]
    fn render_streams_fits_overflowing_pipes_under_the_dispatcher_cap() {
        // `a` and `z` appear nowhere in the marker text, so counting them counts kept bytes.
        for (stdout_len, stderr_len) in [(10 << 20, 1024), (300 << 10, 300 << 10)] {
            let mut stdout = PipeCapture::default();
            stdout.push(&vec![b'a'; stdout_len]);
            let mut stderr = PipeCapture::default();
            stderr.push(&vec![b'z'; stderr_len]);

            let content = render_streams(&stdout, &stderr);

            assert!(
                content.len() <= MAX_OUTPUT_BYTES,
                "{stdout_len} / {stderr_len}"
            );
            let (stdout_text, stderr_text) = content.split_once("\nz").unwrap();
            let stdout_omitted = omitted_counts(stdout_text);
            assert_eq!(stdout_omitted.len(), 1);
            assert_eq!(
                stdout_text.matches('a').count() + stdout_omitted[0],
                stdout_len
            );
            let stderr_omitted: usize = omitted_counts(stderr_text).iter().sum();
            assert_eq!(
                stderr_text.matches('z').count() + 1 + stderr_omitted,
                stderr_len
            );
        }
    }

    #[test]
    fn render_streams_trims_stdout_newlines_and_stderr_whitespace() {
        let stdout = capture(b"\n  output\n", b"", 0);
        let stderr = capture(b"\n  error\n", b"", 0);
        assert_eq!(render_streams(&stdout, &stderr), "  output\nerror");
    }

    // ── split_budget ──

    #[test]
    fn split_budget_hands_an_unused_share_to_the_other_part() {
        assert_eq!(split_budget(10, 20, 100), (10, 20));
        assert_eq!(split_budget(10, 1_000, 100), (10, 90));
        assert_eq!(split_budget(1_000, 10, 100), (90, 10));
        assert_eq!(split_budget(1_000, 1_000, 100), (50, 50));
    }

    // ── render_pipe ──

    fn capture(head: &[u8], tail: &[u8], omitted: usize) -> PipeCapture {
        PipeCapture {
            head: head.to_vec(),
            tail: tail.iter().copied().collect(),
            omitted,
        }
    }

    #[test]
    fn render_pipe_marks_the_omitted_middle_between_head_and_tail() {
        let pipe = capture(b"hello\n", b"world\n", 7);
        assert_eq!(
            render_pipe(&pipe, OUTPUT_BUDGET),
            indoc! {"
                hello

                ... [7 bytes omitted] ...
                world"},
        );
    }

    #[test]
    fn render_pipe_folds_chars_split_by_the_omission_into_the_count() {
        let crab = "🦀".as_bytes();
        let pipe = capture(
            &[b"ab", &crab[..2]].concat(),
            &[&crab[1..], b"cd"].concat(),
            5,
        );
        assert_eq!(
            render_pipe(&pipe, OUTPUT_BUDGET),
            indoc! {"
                ab
                ... [10 bytes omitted] ...
                cd"},
        );
    }

    #[test]
    fn render_pipe_over_budget_drops_the_middle_and_counts_it() {
        let pipe = capture(b"0123456789", b"abcdef4321", 0);
        assert_eq!(
            render_pipe(&pipe, 8),
            indoc! {"
                0123
                ... [12 bytes omitted] ...
                4321"},
        );
    }

    #[test]
    fn render_pipe_decodes_head_and_tail_together_when_nothing_was_omitted() {
        // A multibyte char straddling the head / tail split must not decode as two replacements.
        let crab = "🦀".as_bytes();
        let pipe = capture(&crab[..2], &crab[2..], 0);
        assert_eq!(render_pipe(&pipe, OUTPUT_BUDGET), "🦀");
    }

    #[test]
    fn render_pipe_trims_the_end_and_keeps_leading_and_internal_newlines() {
        let pipe = capture(b"\nerr\nmore\n\n", b"", 0);
        assert_eq!(render_pipe(&pipe, OUTPUT_BUDGET), "\nerr\nmore");
    }

    // ── PipeCapture::drain ──

    #[tokio::test]
    async fn drain_cut_short_keeps_what_already_arrived() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"partial").await.unwrap();

        let mut pipe = PipeCapture::default();
        let drained =
            tokio::time::timeout(Duration::from_millis(50), pipe.drain(&mut reader)).await;

        assert!(
            drained.is_err(),
            "writer is still open, so the drain must time out"
        );
        assert_eq!(pipe.head, b"partial");
    }

    // ── PipeCapture::push ──

    #[test]
    fn push_keeps_head_and_tail_across_chunks_and_counts_the_middle() {
        let mut pipe = PipeCapture::default();
        pipe.push(&vec![b'h'; PIPE_HEAD_BYTES - 1]);
        pipe.push(b"hm");
        pipe.push(&[b'm'; 99]);
        pipe.push(&vec![b't'; PIPE_TAIL_BYTES]);

        assert_eq!(pipe.head, vec![b'h'; PIPE_HEAD_BYTES]);
        assert_eq!(pipe.tail, vec![b't'; PIPE_TAIL_BYTES]);
        assert_eq!(pipe.omitted, 100);
    }

    // ── ShellTree::kill ──

    /// A process tree whose first `live` signals reach a member and whose later ones find none,
    /// with a count of the signals it has received.
    fn fake_tree(live: usize) -> (ShellTree, Arc<AtomicUsize>) {
        let signals = Arc::new(AtomicUsize::new(0));
        let child = FakeChild {
            live,
            signals: Arc::clone(&signals),
        };
        let tree = ShellTree {
            child: Box::new(child),
            armed: true,
        };
        (tree, signals)
    }

    #[derive(Debug)]
    struct FakeChild {
        live: usize,
        signals: Arc<AtomicUsize>,
    }

    impl ChildWrapper for FakeChild {
        fn inner(&self) -> &dyn ChildWrapper {
            unreachable!("a fake child wraps no process")
        }

        fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
            unreachable!("a fake child wraps no process")
        }

        fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
            self
        }

        fn start_kill(&mut self) -> io::Result<()> {
            if self.signals.fetch_add(1, Ordering::Relaxed) < self.live {
                Ok(())
            } else {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn kill_signals_once_from_a_call_and_from_drop() {
        let (mut killed, killed_signals) = fake_tree(usize::MAX);
        let (dropped, dropped_signals) = fake_tree(usize::MAX);

        killed.kill().await;
        drop(killed);
        drop(dropped);

        assert_eq!(killed_signals.load(Ordering::Relaxed), 1);
        assert_eq!(dropped_signals.load(Ordering::Relaxed), 1);
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn kill_signals_until_no_member_is_alive() {
        let (mut tree, signals) = fake_tree(2);

        tree.kill().await;
        drop(tree);

        assert_eq!(signals.load(Ordering::Relaxed), 3);
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn kill_cut_short_finishes_in_drop() {
        let (mut tree, signals) = fake_tree(5);

        let cut = tokio::time::timeout(Duration::ZERO, tree.kill()).await;
        let before_drop = signals.load(Ordering::Relaxed);
        drop(tree);

        assert!(cut.is_err(), "the kill waits between signals");
        assert_eq!(before_drop, 1);
        assert_eq!(signals.load(Ordering::Relaxed), 6);
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn kill_from_drop_gives_up_on_a_tree_that_outlives_the_limit() {
        let (tree, signals) = fake_tree(usize::MAX);

        let began = Instant::now();
        drop(tree);

        assert!(began.elapsed() >= ShellTree::KILL_LIMIT);
        assert!(signals.load(Ordering::Relaxed) > 1);
    }
}
