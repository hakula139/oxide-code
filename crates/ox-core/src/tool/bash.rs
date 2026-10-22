//! Shell command execution tool with timeout.

pub mod background;

use std::collections::VecDeque;
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
use super::spill::{Spill, omission_marker};
use super::view::{ApprovalBody, ApprovalPreview};
use super::{Prepared, Tool, ToolCtx, ToolOutput, ToolSpec, extract_input_field, pascal_case};
use crate::config::auth::secret_env_vars;
use crate::permission::{Access, PermissionClass};

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_TIMEOUT: Duration = Duration::from_mins(10);
/// How long a drain waits for the pipes to close once the shell has exited or its tree is gone.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// The variable each command's environment carries the session ID in, so a script it runs can tell
/// which session started it.
const SESSION_ID_VAR: &str = "OX_SESSION_ID";

/// Editor variables each command's environment points at `true`, so a command that opens an
/// editor, such as `git commit` without `-m` or `git rebase --continue`, keeps its default text and
/// moves on. Nobody can type into an editor started here, and a GUI one such as `code --wait`
/// would open on the user's screen and hold the call until its timeout. `GIT_EDITOR` outranks
/// git's `core.editor`, which outranks `VISUAL` and `EDITOR`.
const EDITOR_VARS: [&str; 3] = ["EDITOR", "GIT_EDITOR", "VISUAL"];

/// Bytes of pipe output one call renders, less the spill paths its omission markers name. The
/// reserve below the dispatcher cap holds the rest of the markers, the stream separator, and the
/// footer, so the dispatcher never re-truncates bash output.
const OUTPUT_BUDGET: usize = super::spill::MAX_OUTPUT_BYTES - 512;
const PIPE_HEAD_BYTES: usize = OUTPUT_BUDGET / 2;
const PIPE_TAIL_BYTES: usize = OUTPUT_BUDGET - PIPE_HEAD_BYTES;
const PIPE_READ_CHUNK_BYTES: usize = 8 * 1024;

const NO_OUTPUT_MARKER: &str = "(no output)";

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
            description: "Execute a shell command and return its output. \
                          For a dev server, a watcher, or a long build, set run_in_background, \
                          without a trailing `&`: the call returns a shell ID at once. \
                          You are not notified when the command finishes, \
                          so check on it with bash_output, and stop it with bash_stop. \
                          A background shell stays open, running or finished, \
                          until bash_stop stops it or the session ends."
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

    fn summarize_call(&self, name: &str, input: &serde_json::Value, _cwd: &Path) -> String {
        extract_input_field(input, "command").map_or_else(|| pascal_case(name), str::to_owned)
    }

    fn approval_preview<'a>(
        &'a self,
        name: &'a str,
        input: &'a serde_json::Value,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = ApprovalPreview> + Send + 'a>> {
        let background = input
            .get("run_in_background")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let label = pascal_case(name);
        Box::pin(std::future::ready(ApprovalPreview {
            title: if background {
                format!("{label} (background)")
            } else {
                label
            },
            body: ApprovalBody::Command(self.summarize_call(name, input, cwd)),
        }))
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
                "description": "Timeout in milliseconds, at most 600000. \
                                Omit or pass 0 for the default of 120000."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Start the command as a background shell \
                                and return its ID without waiting. \
                                The timeout field does not apply."
            }
        },
        "required": ["command"]
    })
}

#[derive(Deserialize)]
struct Input {
    command: String,
    description: Option<String>,
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
}

// ── Execution ──

async fn run(shells: &Arc<BackgroundShells>, raw: serde_json::Value, ctx: &ToolCtx) -> ToolOutput {
    let input: Input = match super::parse_input(raw) {
        Ok(v) => v,
        Err(msg) => return ToolOutput::error(msg),
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

/// Runs `command` in the context's cwd, with [`SESSION_ID_VAR`] set to its session ID, each of the
/// [`EDITOR_VARS`] set to `true`, and none of ox's [`secret_env_vars`].
async fn execute(command: &str, timeout: Duration, ctx: &ToolCtx) -> ToolOutput {
    match execute_until(command, tokio::time::sleep(timeout), ctx).await {
        Ok(output) => output,
        Err(TimedOut(mut content)) => {
            push_footer(
                &mut content,
                &format!("Command timed out after {} ms", timeout.as_millis()),
            );
            ToolOutput::error(content)
        }
    }
}

/// [`execute`], killing the command and returning [`TimedOut`] once `deadline` completes.
async fn execute_until(
    command: &str,
    deadline: impl Future<Output = ()>,
    ctx: &ToolCtx,
) -> Result<ToolOutput, TimedOut> {
    let mut shell = match ShellTree::spawn(command, ctx) {
        Ok(shell) => shell,
        Err(message) => return Ok(ToolOutput::error(message)),
    };

    let (mut stdout, mut stderr) = shell.take_pipes();
    let spills = ctx.spills();
    let mut stdout_capture = PipeCapture::new(spills.file(".stdout.txt"));
    let mut stderr_capture = PipeCapture::new(spills.file(".stderr.txt"));

    // One deadline covers the exit and the pipe drain, so a backgrounded descendant that holds the
    // pipes open cannot stretch the call past the timeout.
    let finished = tokio::select! {
        biased;
        finished = async {
            tokio::join!(
                shell.wait(),
                stdout_capture.drain(&mut stdout),
                stderr_capture.drain(&mut stderr),
            )
        } => Some(finished),
        () = deadline => None,
    };

    let Some((status, stdout_read, stderr_read)) = finished else {
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
        return Err(TimedOut(render_streams(
            &mut stdout_capture,
            &mut stderr_capture,
        )));
    };

    let status = match status {
        Ok(status) => status,
        Err(e) => {
            return Ok(ToolOutput::error(format!("Failed to execute command: {e}")));
        }
    };
    if let Err(e) = stdout_read {
        return Ok(ToolOutput::error(format!(
            "Failed to read command stdout: {e}"
        )));
    }
    if let Err(e) = stderr_read {
        return Ok(ToolOutput::error(format!(
            "Failed to read command stderr: {e}"
        )));
    }
    // Descendants that released the pipes, such as a redirected background server, keep running.
    shell.disarm();

    let mut content = render_streams(&mut stdout_capture, &mut stderr_capture);
    if !status.success() {
        push_footer(&mut content, &exit_footer(status));
    }
    if content.is_empty() {
        content.push_str(NO_OUTPUT_MARKER);
    }

    // A nonzero exit stays a success because probes such as `grep` (1 on no match) and `test`
    // signal through it, so `is_error` marks only spawn, pipe, and timeout failures.
    Ok(ToolOutput::success(content))
}

/// A command [`execute_until`] killed at its deadline, with the output it rendered by then.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
struct TimedOut(String);

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

/// Renders stdout and stderr within [`OUTPUT_BUDGET`], spilling each pipe it cuts.
fn render_streams(stdout: &mut PipeCapture, stderr: &mut PipeCapture) -> String {
    let paths = stdout.spill.path().as_os_str().len() + stderr.spill.path().as_os_str().len();
    let (stdout_kept, stderr_kept) = (stdout.kept_bytes(), stderr.kept_bytes());
    let (stdout_budget, stderr_budget) = split_budget(
        decoded_len(&stdout_kept),
        decoded_len(&stderr_kept),
        OUTPUT_BUDGET - paths,
    );
    let stdout = render_pipe(stdout, &stdout_kept, stdout_budget, "stdout");
    let stdout = stdout.trim_start_matches('\n');
    let stderr = render_pipe(stderr, &stderr_kept, stderr_budget, "stderr");
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

/// Renders the pipe called `name`, whose [`PipeCapture::kept_bytes`] are `kept`, within `budget`
/// bytes, decoding each invalid UTF-8 sequence as U+FFFD, and counts every raw byte it drops in the
/// omission marker. A cut spills the pipe's kept bytes if it has not spilled yet, so its marker can
/// always name a file with the full output.
fn render_pipe(pipe: &mut PipeCapture, kept: &[u8], budget: usize, name: &str) -> String {
    let text = if pipe.omitted == 0 && decoded_len(kept) <= budget {
        String::from_utf8_lossy(kept).into_owned()
    } else {
        let (head, tail) = kept.split_at(pipe.head.len());
        let (head, tail) = ends_within(head, tail, budget);
        if !pipe.spill.started() {
            pipe.spill_kept();
        }

        let omitted = pipe.omitted + kept.len() - head.len() - tail.len();
        let head = String::from_utf8_lossy(head);
        let marker = omission_marker(omitted, &pipe.spill.note(name));
        let tail = String::from_utf8_lossy(tail);
        formatdoc! {"
            {head}
            {marker}
            {tail}"
        }
    };

    text.trim_end().to_owned()
}

/// The start of `head` and the end of `tail` to keep around a cut between them, decoding within
/// `budget` bytes.
fn ends_within<'a>(head: &'a [u8], tail: &'a [u8], budget: usize) -> (&'a [u8], &'a [u8]) {
    // The capture can split a multibyte char between head and tail, and the stray bytes on either
    // side would decode as U+FFFD.
    let split_char = head.utf8_chunks().last().map_or(0, |c| c.invalid().len());
    let continuations = tail
        .iter()
        .take(3)
        .take_while(|&&b| b & 0xC0 == 0x80)
        .count();
    let head = &head[..head.len() - split_char];
    let tail = &tail[continuations..];

    let (head_budget, tail_budget) = split_budget(decoded_len(head), decoded_len(tail), budget);
    (
        prefix_within(head, head_budget),
        suffix_within(tail, tail_budget),
    )
}

/// The length of `bytes` once [`String::from_utf8_lossy`] decodes them.
fn decoded_len(bytes: &[u8]) -> usize {
    char_lens(bytes).map(|(_, decoded)| decoded).sum()
}

/// The longest prefix of `bytes` that decodes within `budget` bytes, cut between chars.
fn prefix_within(bytes: &[u8], budget: usize) -> &[u8] {
    let len = char_lens(bytes)
        .scan(budget, |room, (raw, decoded)| {
            *room = room.checked_sub(decoded)?;
            Some(raw)
        })
        .sum();
    &bytes[..len]
}

/// The longest suffix of `bytes` that decodes within `budget` bytes, cut between chars.
fn suffix_within(bytes: &[u8], budget: usize) -> &[u8] {
    let excess = decoded_len(bytes).saturating_sub(budget);
    let cut = char_lens(bytes)
        .scan(excess, |excess, (raw, decoded)| {
            (*excess > 0).then(|| {
                *excess = excess.saturating_sub(decoded);
                raw
            })
        })
        .sum();
    &bytes[cut..]
}

/// The raw and decoded length of each char in `bytes`, where each invalid UTF-8 sequence counts as
/// one char that decodes as U+FFFD.
fn char_lens(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> {
    bytes.utf8_chunks().flat_map(|chunk| {
        let chars = chunk.valid().chars().map(|c| (c.len_utf8(), c.len_utf8()));
        let invalid = chunk.invalid().len();
        let replacement = char::REPLACEMENT_CHARACTER.len_utf8();
        chars.chain((invalid > 0).then_some((invalid, replacement)))
    })
}

// ── Pipe Capture ──

/// Bounded capture of one pipe: the first [`PIPE_HEAD_BYTES`] and the last [`PIPE_TAIL_BYTES`],
/// with the bytes dropped between them counted in `omitted`. Once it would drop a byte, it writes
/// everything the pipe sends to its spill, so the full output survives while memory stays bounded.
struct PipeCapture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: usize,
    spill: Spill,
}

impl PipeCapture {
    fn new(spill: Spill) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            omitted: 0,
            spill,
        }
    }

    /// Reads `reader` to EOF. Cancel-safe: each chunk is recorded as soon as it is read, so a drain
    /// cut short by the timeout keeps everything that arrived, and a later call resumes it.
    async fn drain<R>(&mut self, reader: &mut R) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        read_chunks(reader, |chunk| self.push(chunk)).await
    }

    fn kept_len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// The head followed by the tail, in one buffer.
    fn kept_bytes(&self) -> Vec<u8> {
        let (front, back) = self.tail.as_slices();
        [self.head.as_slice(), front, back].concat()
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.spill.started() {
            self.spill.write(chunk);
        } else if self.kept_len() + chunk.len() > PIPE_HEAD_BYTES + PIPE_TAIL_BYTES {
            self.spill_kept();
            self.spill.write(chunk);
        }

        let head_room = PIPE_HEAD_BYTES.saturating_sub(self.head.len());
        let (head, rest) = chunk.split_at(head_room.min(chunk.len()));
        self.head.extend_from_slice(head);
        self.tail.extend(rest);

        let overflow = self.tail.len().saturating_sub(PIPE_TAIL_BYTES);
        self.tail.drain(..overflow);
        self.omitted += overflow;
    }

    /// Starts the spill with the kept bytes, which hold everything the pipe sent while nothing was
    /// dropped.
    fn spill_kept(&mut self) {
        let (front, back) = self.tail.as_slices();
        self.spill.write(&self.head);
        self.spill.write(front);
        self.spill.write(back);
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
///
/// On Unix the shell also leads its own session, which has no controlling terminal. A command that
/// prompts on `/dev/tty`, such as `sudo` or `ssh` asking for a password, then fails at once. In
/// ox's session it would print onto the TUI and stop on `SIGTTIN` until killed.
struct ShellTree {
    child: Box<dyn ChildWrapper>,
    armed: bool,
}

impl ShellTree {
    /// Spawns `command` in the context's cwd, failing with the message a tool call reports.
    fn spawn(command: &str, ctx: &ToolCtx) -> Result<Self, String> {
        let failed = |e: io::Error| {
            let cwd = ctx.cwd().display();
            format!("Failed to execute command in {cwd}: {e}")
        };
        let mut cmd = CommandWrap::with_new(bash().map_err(failed)?, |cmd| {
            cmd.arg("-c")
                .arg(command)
                .current_dir(ctx.cwd())
                .env(SESSION_ID_VAR, &ctx.session_id)
                .envs(EDITOR_VARS.map(|var| (var, "true")))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for var in secret_env_vars() {
                cmd.env_remove(var);
            }
        });
        #[cfg(unix)]
        cmd.wrap(process_wrap::tokio::ProcessSession);
        #[cfg(windows)]
        cmd.wrap(process_wrap::tokio::JobObject);
        Ok(Self {
            child: cmd.spawn().map_err(failed)?,
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
        self.kill_blocking_until(Instant::now() + Self::KILL_LIMIT);
    }

    fn kill_blocking_until(&mut self, deadline: Instant) {
        self.armed = false;
        while self.signal(deadline) {
            std::thread::sleep(Self::KILL_RETRY_DELAY);
        }
    }

    /// Signals the group and returns whether to signal it again. `killpg` fails once no member it
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

// ── Testing ──

#[cfg(test)]
mod testing {
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use super::bash;

    /// A command that records the PID `pid`, such as `$!` or `$$`, in `pid_file`. The rename
    /// publishes the PID whole.
    pub(super) fn record_pid(pid: &str, pid_file: &Path) -> String {
        let pid_file = pid_file.display();
        format!("echo {pid} > '{pid_file}.tmp' && mv '{pid_file}.tmp' '{pid_file}'")
    }

    /// Waits for the PID [`record_pid`] writes to `pid_file` and returns it. Spawning bash on a
    /// loaded Windows runner can take several seconds.
    pub(super) async fn until_recorded(pid_file: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !pid_file.exists() {
            assert!(
                Instant::now() < deadline,
                "no PID in {}",
                pid_file.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::read_to_string(pid_file).unwrap().trim().to_owned()
    }

    /// Waits until no process has `pid`, failing if one still does after a deadline.
    pub(super) async fn until_gone(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid).await {
            assert!(Instant::now() < deadline, "process {pid} is still alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Returns whether `pid` names a live process. The tool's own bash checks, since Git Bash on
    /// Windows numbers its processes apart from Windows.
    async fn alive(pid: &str) -> bool {
        tokio::process::Command::new(bash().unwrap())
            .args(["-c", &format!("kill -0 {pid}")])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap()
            .success()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use indoc::indoc;
    #[cfg(unix)]
    use nix::unistd::{Pid, getpgid, getsid};
    use tokio::io::AsyncWriteExt as _;

    use super::testing::{record_pid, until_gone, until_recorded};
    use super::*;
    #[cfg(unix)]
    use crate::tool::local_testing::canonical_tempdir;
    use crate::tool::local_testing::{self, ctx, spill_path, temp_ctx};
    use crate::tool::spill::MAX_OUTPUT_BYTES;
    use crate::tool::testing::bash_preview;

    /// An empty capture of the stdout of call `toolu_test` in `dir`.
    fn empty_capture(dir: &Path) -> PipeCapture {
        PipeCapture::new(ctx(dir).spills().file(".stdout.txt"))
    }

    /// A capture in `dir` holding `head` and `tail`, with `omitted` bytes dropped between them.
    fn capture(dir: &Path, head: &[u8], tail: &[u8], omitted: usize) -> PipeCapture {
        PipeCapture {
            head: head.to_vec(),
            tail: tail.iter().copied().collect(),
            omitted,
            ..empty_capture(dir)
        }
    }

    mod prepare {
        use super::*;

        #[test]
        fn extracts_the_command() {
            let ctx = temp_ctx();
            let prepared = BashTool::new(Arc::default())
                .prepare(serde_json::json!({"command": "ls -la"}), &ctx);
            assert_eq!(
                prepared.access,
                Access::for_command("ls -la", &ctx.workspace)
            );
        }

        #[test]
        fn missing_command_has_no_target() {
            // Only a tool-wide rule matches `None`, so a malformed call cannot slip past a
            // command-specific allow.
            let prepared =
                BashTool::new(Arc::default()).prepare(serde_json::json!({}), &temp_ctx());
            assert!(matches!(prepared.access, Access::None));
        }
    }

    mod approval_preview {
        use super::*;

        #[tokio::test]
        async fn marks_a_background_command_in_the_title() {
            let tool = BashTool::new(Arc::default());
            let cwd = Path::new("/repo");

            let foreground = tool
                .approval_preview("bash", &serde_json::json!({"command": "cargo test"}), cwd)
                .await;
            let background = tool
                .approval_preview(
                    "bash",
                    &serde_json::json!({"command": "cargo watch", "run_in_background": true}),
                    cwd,
                )
                .await;

            assert_eq!(foreground, bash_preview("cargo test"));
            assert_eq!(
                background,
                ApprovalPreview {
                    title: "Bash (background)".to_owned(),
                    body: ApprovalBody::Command("cargo watch".to_owned()),
                }
            );
        }
    }

    mod run {
        use super::*;

        #[tokio::test]
        async fn returns_the_output_of_a_command_that_succeeds() {
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
        async fn missing_command_field_reports_input_error() {
            let output = run(&Arc::default(), serde_json::json!({}), &temp_ctx()).await;

            assert!(output.is_error);
            assert_eq!(output.content, "Invalid input: missing field `command`");
        }

        #[tokio::test]
        async fn attaches_description_to_metadata_title() {
            let input = serde_json::json!({
                "command": "echo hello",
                "description": "Print greeting",
            });

            let output =
                local_testing::run(&BashTool::new(Arc::default()), input, &temp_ctx()).await;

            assert!(!output.is_error);
            assert_eq!(output.content, "hello");
            assert_eq!(output.metadata.title.as_deref(), Some("Print greeting"));
        }

        #[tokio::test]
        async fn times_out_after_the_input_timeout() {
            let output = run(
                &Arc::default(),
                serde_json::json!({"command": "sleep 30", "timeout": 1}),
                &temp_ctx(),
            )
            .await;

            assert!(output.is_error);
            assert_eq!(output.content, "Command timed out after 1 ms");
        }
    }

    mod resolve_timeout {
        use super::*;

        #[test]
        fn defaults_zero_and_caps_the_ceiling() {
            assert_eq!(resolve_timeout(None), DEFAULT_TIMEOUT);
            assert_eq!(resolve_timeout(Some(0)), DEFAULT_TIMEOUT);
            assert_eq!(resolve_timeout(Some(5_000)), Duration::from_secs(5));
            assert_eq!(resolve_timeout(Some(u64::MAX)), MAX_TIMEOUT);
        }
    }

    mod execute {
        use super::*;

        async fn run_default(cmd: &str) -> ToolOutput {
            execute(cmd, DEFAULT_TIMEOUT, &temp_ctx()).await
        }

        #[tokio::test]
        async fn stderr_output_preserves_content() {
            let output = run_default("echo err >&2").await;
            assert!(!output.is_error);
            assert_eq!(output.content, "err");
        }

        #[tokio::test]
        async fn combined_stdout_and_stderr_renders_both_streams() {
            let output = run_default("echo out && echo err >&2").await;
            assert!(!output.is_error);
            assert_eq!(output.content, "out\nerr");
        }

        #[tokio::test]
        async fn nonzero_exit_not_flagged_as_error() {
            let output = run_default("false").await;
            assert!(!output.is_error);
            assert_eq!(output.content, "(exit code 1)");
        }

        #[tokio::test]
        async fn nonzero_exit_preserves_output() {
            let output = run_default("echo partial; false").await;
            assert!(!output.is_error);
            assert_eq!(
                output.content,
                indoc! {"
                    partial

                    (exit code 1)"
                }
            );
        }

        #[tokio::test]
        async fn no_output_renders_the_no_output_marker() {
            let output = run_default("true").await;
            assert!(!output.is_error);
            assert_eq!(output.content, "(no output)");
        }

        #[tokio::test]
        async fn large_output_spills_in_full_and_keeps_head_and_tail() {
            // `cargo build 2>&1` ends with its error summary, so the tail must survive the cap.
            let dir = tempfile::tempdir().unwrap();
            let filler = PIPE_HEAD_BYTES + PIPE_TAIL_BYTES + 1024;
            let command = format!("echo HEAD; yes x | head -c {filler}; echo TAIL");

            let output = execute(&command, DEFAULT_TIMEOUT, &ctx(dir.path())).await;

            assert!(!output.is_error);
            let path = spill_path(dir.path(), ".stdout.txt");
            let full = fs::read_to_string(&path).unwrap();
            assert_eq!(full, format!("HEAD\n{}TAIL\n", "x\n".repeat(filler / 2)));
            let (head, rest) = output.content.split_once("\n... [").unwrap();
            let (marker, tail) = rest.split_once("] ...\n").unwrap();
            let (omitted, note) = marker.split_once(" bytes omitted, ").unwrap();
            assert_eq!(note, format!("full stdout saved to {}", path.display()));
            assert!(full.starts_with(head) && head.starts_with("HEAD\nx\n"));
            assert!(full.ends_with(&format!("{tail}\n")) && tail.ends_with("x\nTAIL"));
            let omitted: usize = omitted.parse().unwrap();
            assert_eq!(head.len() + omitted + tail.len() + "\n".len(), full.len());
            assert!(output.content.len() <= MAX_OUTPUT_BYTES);
        }

        #[tokio::test]
        async fn spills_each_pipe_over_the_cap_to_its_own_file() {
            let dir = tempfile::tempdir().unwrap();
            let bytes = 300 << 10;
            let command = format!(
                "head -c {bytes} /dev/zero | tr '\\0' o; head -c {bytes} /dev/zero | tr '\\0' e >&2"
            );

            let output = execute(&command, DEFAULT_TIMEOUT, &ctx(dir.path())).await;

            assert!(!output.is_error, "{}", output.content);
            for (name, byte) in [("stdout", b'o'), ("stderr", b'e')] {
                let path = spill_path(dir.path(), &format!(".{name}.txt"));
                assert_eq!(fs::read(&path).unwrap(), vec![byte; bytes], "{name}");
                let note = format!(
                    "bytes omitted, full {name} saved to {}] ...",
                    path.display()
                );
                assert!(output.content.contains(&note), "{name}");
            }
            assert!(output.content.len() <= MAX_OUTPUT_BYTES);
        }

        #[tokio::test]
        async fn output_under_the_cap_writes_no_spill_file() {
            // One pipe can use the other's unused share, so this renders whole.
            let dir = tempfile::tempdir().unwrap();
            let bytes = 100 << 10;
            let command = format!("head -c {bytes} /dev/zero | tr '\\0' o");

            let output = execute(&command, DEFAULT_TIMEOUT, &ctx(dir.path())).await;

            assert_eq!(output.content, "o".repeat(bytes));
            assert!(!dir.path().join("session").exists());
        }

        #[tokio::test]
        async fn cancelled_mid_run_kills_backgrounded_children() {
            // The shell either still runs or has exited while a descendant holds its pipes.
            for tail in ["sleep 30", "echo done"] {
                let dir = tempfile::tempdir().unwrap();
                let pid_file = dir.path().join("pid");
                let command = format!("sleep 30 & {}; {tail}", record_pid("$!", &pid_file));
                let ctx = temp_ctx();
                let mut call = Box::pin(execute(&command, DEFAULT_TIMEOUT, &ctx));

                let pid = tokio::select! {
                    output = &mut call => panic!("{tail}: finished before the cancel: {}", output.content),
                    pid = until_recorded(&pid_file) => pid,
                };
                drop(call);

                until_gone(&pid).await;
            }
        }

        #[tokio::test]
        async fn finished_call_keeps_descendants_detached_from_its_pipes() {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("parent");
            let ready = dir.path().join("ready");
            let completed = dir.path().join("completed");
            let release = dir.path().join("release");
            let diagnostics = dir.path().join("diagnostics");
            let parent_command = record_pid("$$", &parent);
            let ready_command = record_pid("$$", &ready);
            let completed_command = record_pid("$$", &completed);
            let scratch_path = dir.path().display();
            let ready_path = ready.display();
            let release_path = release.display();
            let diagnostics_path = diagnostics.display();
            let script = dir.path().join("child.sh");
            let script_path = script.display();
            fs::write(
                &script,
                formatdoc! {"
                    {ready_command} || exit
                    while [ -d '{scratch_path}' ] && [ ! -f '{release_path}' ]; do sleep 0.02; done
                    [ -f '{release_path}' ] && {completed_command}
                "},
            )
            .unwrap();
            let command = formatdoc! {r#"
                {parent_command} || exit
                "$BASH" '{script_path}' > '{diagnostics_path}' 2>&1 &
                child=$!
                while [ ! -f '{ready_path}' ]; do
                    kill -0 $child || {{ wait $child; exit $?; }}
                    sleep 0.02
                done
                echo started
            "#};

            let output = run_default(&command).await;
            fs::write(&release, "").unwrap();
            assert_eq!(
                output.content,
                "started",
                "child diagnostics: {:?}",
                fs::read_to_string(&diagnostics),
            );
            let pid = until_recorded(&ready).await;
            assert_ne!(pid, until_recorded(&parent).await);
            until_gone(&pid).await;

            let completed_pid = fs::read_to_string(&completed).unwrap_or_else(|error| {
                let diagnostics = fs::read_to_string(&diagnostics).unwrap();
                panic!(
                    "descendant {pid} exited without recording completion: {error}. {diagnostics}"
                )
            });
            assert_eq!(completed_pid.trim(), pid);
        }

        #[tokio::test]
        async fn points_every_editor_at_true() {
            let output = run_default(r#"printf %s "$EDITOR $GIT_EDITOR $VISUAL""#).await;
            assert!(!output.is_error, "{}", output.content);
            assert_eq!(output.content, "true true true");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn runs_in_the_ctx_cwd_with_the_session_id_exported() {
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
        async fn in_a_removed_cwd_names_it_in_the_error() {
            let dir = canonical_tempdir();
            let gone = dir.path().join("gone");

            let output = execute("true", DEFAULT_TIMEOUT, &ctx(&gone)).await;

            assert!(output.is_error);
            let cause = io::Error::from_raw_os_error(nix::errno::Errno::ENOENT as i32);
            assert_eq!(
                output.content,
                format!("Failed to execute command in {}: {cause}", gone.display()),
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn signal_death_names_the_signal() {
            let output = run_default("echo before; kill -9 $$").await;
            assert!(!output.is_error);
            assert_eq!(
                output.content,
                indoc! {"
                    before

                    (killed by signal 9)"
                }
            );
        }
    }

    mod execute_until {
        use super::*;

        #[tokio::test]
        async fn timeout_kills_backgrounded_children() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let command = format!("sleep 30 & {}; sleep 30", record_pid("$!", &pid_file));
            let started = async {
                until_recorded(&pid_file).await;
            };

            let timed_out = execute_until(&command, started, &temp_ctx()).await.err();

            assert!(matches!(timed_out, Some(TimedOut(_))), "{timed_out:?}");
            until_gone(&until_recorded(&pid_file).await).await;
        }

        #[tokio::test]
        async fn timeout_covers_background_pipe_holders() {
            // The backgrounded `sleep` holds the pipes open past bash's exit, so only a deadline
            // that covers the drain ends the call before the `sleep` does.
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let command = format!("sleep 30 & echo done; {}", record_pid("$$", &pid_file));
            let exited = async {
                until_gone(&until_recorded(&pid_file).await).await;
            };

            let timed_out = execute_until(&command, exited, &temp_ctx()).await.err();

            assert_eq!(timed_out, Some(TimedOut("done".to_owned())));
        }
    }

    #[cfg(windows)]
    mod git_bash_on {
        use super::*;

        #[test]
        fn finds_bash_under_the_git_root_of_any_git_on_path() {
            let dir = tempfile::tempdir().unwrap();
            // A shim outside any Git root, then the `mingw64\bin` and `cmd` copies a Git Bash
            // session puts on `PATH`.
            let [shims, mingw, cmd, bin] =
                ["shims", r"Git\mingw64\bin", r"Git\cmd", r"Git\bin"].map(|d| dir.path().join(d));
            for d in [&shims, &mingw, &cmd, &bin] {
                fs::create_dir_all(d).unwrap();
            }
            for d in [&shims, &mingw, &cmd] {
                fs::write(d.join("git.exe"), "").unwrap();
            }
            let path = |entries: &[&std::path::PathBuf]| std::env::join_paths(entries).unwrap();
            assert_eq!(git_bash_on(&path(&[&shims, &cmd])), None, "no bash.exe yet");

            let bash = bin.join("bash.exe");
            fs::write(&bash, "").unwrap();
            for git_dir in [&mingw, &cmd] {
                assert_eq!(git_bash_on(&path(&[&shims, git_dir])).as_ref(), Some(&bash));
            }
            assert_eq!(git_bash_on(&path(&[&shims])), None);
        }
    }

    mod render_streams {
        use super::*;

        /// Counts the chars a pipe's rendering kept, each standing for one raw byte, and reads the
        /// byte count out of each of its `... [N bytes omitted, ...] ...` marker lines.
        fn kept_and_omitted(text: &str) -> (usize, Vec<usize>) {
            let (mut kept, mut omitted) = (0, Vec::new());
            for line in text.lines() {
                match line.strip_prefix("... [") {
                    Some(marker) => {
                        omitted.push(marker.split(' ').next().unwrap().parse().unwrap());
                    }
                    None => kept += line.chars().count(),
                }
            }
            (kept, omitted)
        }

        #[test]
        fn fits_overflowing_pipes_under_the_dispatcher_cap() {
            // Each invalid byte, such as `0xFF`, decodes as the three bytes of U+FFFD.
            let cases = [
                (b'a', 10 << 20, 1024, 0),
                (b'a', 300 << 10, 300 << 10, 0),
                (b'a', 300 << 10, 300 << 10, 3),
                (0xFF, 300 << 10, 300 << 10, 0),
            ];
            for (stdout_byte, stdout_len, stderr_len, depth) in cases {
                let dir = tempfile::tempdir().unwrap();
                let cwd =
                    (0..depth).fold(dir.path().to_owned(), |cwd, _| cwd.join("d".repeat(100)));
                let spills = ctx(&cwd).spills();
                let mut stdout = PipeCapture::new(spills.file(".stdout.txt"));
                stdout.push(&vec![stdout_byte; stdout_len]);
                let mut stderr = PipeCapture::new(spills.file(".stderr.txt"));
                stderr.push(&vec![b'z'; stderr_len]);

                let content = render_streams(&mut stdout, &mut stderr);

                let case =
                    format!("{stdout_byte:#04x}: {stdout_len} / {stderr_len} at depth {depth}");
                assert!(content.len() <= MAX_OUTPUT_BYTES, "{case}");
                let (stdout_text, stderr_text) = content.split_once("\nz").unwrap();
                let (stdout_kept, stdout_omitted) = kept_and_omitted(stdout_text);
                assert_eq!(stdout_omitted.len(), 1, "{case}");
                assert_eq!(stdout_kept + stdout_omitted[0], stdout_len, "{case}");
                let (stderr_kept, stderr_omitted) = kept_and_omitted(stderr_text);
                assert_eq!(
                    stderr_kept + 1 + stderr_omitted.iter().sum::<usize>(),
                    stderr_len,
                    "{case}"
                );
            }
        }

        #[test]
        fn splits_the_budget_by_decoded_length() {
            // 32 KiB of `0xFF` decode to 96 KiB, so they fit only in a split by decoded length.
            let dir = tempfile::tempdir().unwrap();
            let mut stdout = capture(dir.path(), &vec![0xFF; 32 << 10], b"", 0);
            let mut stderr = capture(dir.path(), &vec![b'z'; 16 << 10], b"", 0);

            let content = render_streams(&mut stdout, &mut stderr);

            let stdout = char::REPLACEMENT_CHARACTER.to_string().repeat(32 << 10);
            let stderr = "z".repeat(16 << 10);
            assert!(content == format!("{stdout}\n{stderr}"), "a pipe was cut");
            assert!(!dir.path().join("session").exists());
        }

        #[test]
        fn trims_stdout_newlines_and_stderr_whitespace() {
            let dir = tempfile::tempdir().unwrap();
            let mut stdout = capture(dir.path(), b"\n  output\n", b"", 0);
            let mut stderr = capture(dir.path(), b"\n  error\n", b"", 0);
            assert_eq!(render_streams(&mut stdout, &mut stderr), "  output\nerror");
        }
    }

    mod split_budget {
        use super::*;

        #[test]
        fn hands_an_unused_share_to_the_other_part() {
            assert_eq!(split_budget(10, 20, 100), (10, 20));
            assert_eq!(split_budget(10, 1_000, 100), (10, 90));
            assert_eq!(split_budget(1_000, 10, 100), (90, 10));
            assert_eq!(split_budget(1_000, 1_000, 100), (50, 50));
        }
    }

    mod render_pipe {
        use super::*;

        /// The marker for `omitted` bytes of the stdout spilled in `dir`.
        fn marker(dir: &Path, omitted: usize) -> String {
            let path = spill_path(dir, ".stdout.txt");
            format!(
                "... [{omitted} bytes omitted, full stdout saved to {}] ...",
                path.display()
            )
        }

        /// Renders `pipe` as stdout within `budget` bytes.
        fn render(pipe: &mut PipeCapture, budget: usize) -> String {
            let kept = pipe.kept_bytes();
            render_pipe(pipe, &kept, budget, "stdout")
        }

        #[test]
        fn marks_the_omitted_middle_between_head_and_tail() {
            let dir = tempfile::tempdir().unwrap();
            let mut pipe = capture(dir.path(), b"hello\n", b"world\n", 7);

            let rendered = render(&mut pipe, OUTPUT_BUDGET);

            assert_eq!(
                rendered,
                format!("hello\n\n{}\nworld", marker(dir.path(), 7))
            );
        }

        #[test]
        fn folds_chars_split_by_the_omission_into_the_count() {
            let dir = tempfile::tempdir().unwrap();
            let crab = "🦀".as_bytes();
            let mut pipe = capture(
                dir.path(),
                &[b"ab", &crab[..2]].concat(),
                &[&crab[1..], b"cd"].concat(),
                5,
            );

            let rendered = render(&mut pipe, OUTPUT_BUDGET);

            assert_eq!(rendered, format!("ab\n{}\ncd", marker(dir.path(), 10)));
        }

        #[test]
        fn over_budget_fits_the_decoded_text_and_spills_the_raw_pipe() {
            // Each invalid sequence, even the 2-byte `\xE2\x82`, decodes as one 3-byte U+FFFD.
            let dir = tempfile::tempdir().unwrap();
            let mut pipe = capture(dir.path(), b"a\xFF\xFFcd", b"\xFF\xFFw\xE2\x82yz", 0);

            let rendered = render(&mut pipe, 14);

            let r = char::REPLACEMENT_CHARACTER;
            assert_eq!(
                rendered,
                format!("a{r}{r}\n{}\nw{r}yz", marker(dir.path(), 4))
            );
            let spilled = fs::read(spill_path(dir.path(), ".stdout.txt")).unwrap();
            assert_eq!(spilled, b"a\xFF\xFFcd\xFF\xFFw\xE2\x82yz");
        }

        #[test]
        fn decodes_head_and_tail_together_when_nothing_was_omitted() {
            // A multibyte char across the head / tail split must not decode as two replacements.
            let dir = tempfile::tempdir().unwrap();
            let crab = "🦀".as_bytes();
            let mut pipe = capture(dir.path(), &crab[..2], &crab[2..], 0);

            let rendered = render(&mut pipe, OUTPUT_BUDGET);

            assert_eq!(rendered, "🦀");
            assert!(!dir.path().join("session").exists());
        }

        #[test]
        fn trims_the_end_and_keeps_leading_and_internal_newlines() {
            let dir = tempfile::tempdir().unwrap();
            let mut pipe = capture(dir.path(), b"\nerr\nmore\n\n", b"", 0);
            assert_eq!(render(&mut pipe, OUTPUT_BUDGET), "\nerr\nmore");
        }
    }

    mod pipe_capture {
        use super::*;

        mod drain {
            use super::*;

            #[tokio::test]
            async fn cut_short_keeps_what_already_arrived() {
                let dir = tempfile::tempdir().unwrap();
                let (mut writer, mut reader) = tokio::io::duplex(64);
                writer.write_all(b"partial").await.unwrap();

                let mut pipe = empty_capture(dir.path());
                let drained =
                    tokio::time::timeout(Duration::from_millis(50), pipe.drain(&mut reader)).await;

                assert!(
                    drained.is_err(),
                    "writer is still open, so the drain must time out"
                );
                assert_eq!(pipe.head, b"partial");
            }

            #[tokio::test]
            async fn spills_everything_while_buffering_a_bounded_amount() {
                let dir = tempfile::tempdir().unwrap();
                let sent: Vec<u8> = (0..=u8::MAX).cycle().take(4 << 20).collect();

                let mut pipe = empty_capture(dir.path());
                pipe.drain(&mut sent.as_slice()).await.unwrap();

                let spilled = fs::read(spill_path(dir.path(), ".stdout.txt")).unwrap();
                assert!(spilled == sent, "the spill holds every byte sent");
                let buffered = pipe.head.capacity() + pipe.tail.capacity();
                let bound = 2 * (PIPE_HEAD_BYTES + PIPE_TAIL_BYTES + PIPE_READ_CHUNK_BYTES);
                assert!(buffered <= bound, "{buffered} bytes buffered");
            }
        }

        mod push {
            use super::*;

            #[test]
            fn keeps_head_and_tail_across_chunks_and_counts_the_middle() {
                let dir = tempfile::tempdir().unwrap();
                let mut pipe = empty_capture(dir.path());
                pipe.push(&vec![b'h'; PIPE_HEAD_BYTES - 1]);
                pipe.push(b"hm");
                pipe.push(&[b'm'; 99]);
                pipe.push(&vec![b't'; PIPE_TAIL_BYTES]);

                assert_eq!(pipe.head, vec![b'h'; PIPE_HEAD_BYTES]);
                assert_eq!(pipe.tail, vec![b't'; PIPE_TAIL_BYTES]);
                assert_eq!(pipe.omitted, 100);
                let spilled = fs::read(spill_path(dir.path(), ".stdout.txt")).unwrap();
                let sent = [
                    vec![b'h'; PIPE_HEAD_BYTES],
                    vec![b'm'; 100],
                    vec![b't'; PIPE_TAIL_BYTES],
                ];
                assert!(spilled == sent.concat(), "the spill holds every byte sent");
            }
        }
    }

    mod shell_tree {
        use super::*;

        /// A process tree whose first `live` signals reach a member and whose later ones find
        /// none, with a count of the signals it has received.
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

        mod spawn {
            use super::*;

            #[tokio::test]
            async fn withholds_the_secrets_ox_authenticates_with() {
                let names: Vec<_> = secret_env_vars().chain(["OX_TEST_INHERITED"]).collect();
                let vars: Vec<_> = names.iter().map(|&var| (var, Some("secret"))).collect();
                let command = names
                    .iter()
                    .map(|var| format!("echo {var}=${{{var}-unset}}"))
                    .collect::<Vec<_>>()
                    .join("; ");

                let mut tree =
                    temp_env::with_vars(vars, || ShellTree::spawn(&command, &temp_ctx()).unwrap());
                let (mut stdout, _) = tree.take_pipes();
                let mut output = String::new();
                stdout.read_to_string(&mut output).await.unwrap();

                assert_eq!(
                    output,
                    indoc! {"
                        ANTHROPIC_AUTH_TOKEN=unset
                        ANTHROPIC_API_KEY=unset
                        CLAUDE_CODE_OAUTH_TOKEN=unset
                        ANTHROPIC_CUSTOM_HEADERS=unset
                        OX_TEST_INHERITED=secret
                    "},
                );
            }

            #[cfg(unix)]
            #[tokio::test]
            async fn leads_a_new_session_and_process_group() {
                let tree = ShellTree::spawn("sleep 30", &temp_ctx()).unwrap();
                let pid = Pid::from_raw(tree.child.id().unwrap().try_into().unwrap());

                assert_eq!(getsid(Some(pid)), Ok(pid));
                assert_eq!(getpgid(Some(pid)), Ok(pid));
            }
        }

        mod kill {
            use super::*;

            #[cfg(any(target_os = "linux", windows))]
            #[tokio::test]
            async fn signals_once_from_a_call_and_from_drop() {
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
            async fn signals_until_no_member_is_alive() {
                let (mut tree, signals) = fake_tree(2);

                tree.kill().await;
                drop(tree);

                assert_eq!(signals.load(Ordering::Relaxed), 3);
            }

            // In real time, a late wakeup can fire the retry sleep before the zero timeout returns.
            #[cfg(all(unix, not(target_os = "linux")))]
            #[tokio::test(start_paused = true)]
            async fn cut_short_finishes_in_drop() {
                let (mut tree, signals) = fake_tree(5);

                let cut = tokio::time::timeout(Duration::ZERO, tree.kill()).await;
                let before_drop = signals.load(Ordering::Relaxed);
                drop(tree);

                assert!(cut.is_err(), "the kill waits between signals");
                assert_eq!(before_drop, 1);
                assert_eq!(signals.load(Ordering::Relaxed), 6);
            }
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        mod kill_blocking_until {
            use super::*;

            #[test]
            fn gives_up_on_a_tree_alive_past_the_deadline() {
                let (mut tree, signals) = fake_tree(usize::MAX);

                tree.kill_blocking_until(Instant::now());
                drop(tree);

                assert_eq!(signals.load(Ordering::Relaxed), 1);
            }
        }
    }
}
