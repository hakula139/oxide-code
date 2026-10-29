//! Background shells: `bash` commands that keep running after their call, and the `bash_output`
//! and `bash_stop` tools that read and end them.

use std::io;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;
use tokio::process::ChildStdout;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tracing::error;

use super::{
    Bash, DRAIN_GRACE, MAX_TIMEOUT, PipeCapture, ShellTree, push_footer, read_chunks,
    render_output, spawn_failure,
};
use crate::permission::{Access, PermissionClass};
use crate::protocol::{Event, EventSink};
use crate::tool::spill::Spills;
use crate::tool::{
    Prepared, Tool, ToolCtx, ToolDefinition, ToolOutput, ToolSpec, extract_input_field,
};
use crate::util::sync::lock_or_recover;
use crate::util::text::ELLIPSIS;

/// Background shells a session keeps open at once, running or finished.
const MAX_SHELLS: usize = 16;

const NO_NEW_OUTPUT_MARKER: &str = "(no new output)";

// ── Shell State ──

/// A background shell as the session lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundShell {
    pub id: String,
    pub(crate) command: String,
    pub state: ShellState,
}

impl BackgroundShell {
    /// The command's first line, followed by [`ELLIPSIS`] when more lines follow.
    pub fn command_preview(&self) -> String {
        let mut lines = self.command.lines();
        let first = lines.next().unwrap_or_default();
        if lines.next().is_some() {
            format!("{first} {ELLIPSIS}")
        } else {
            first.to_owned()
        }
    }
}

/// Whether a shell is still running, and how it ended if not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ShellState {
    Running,
    /// Exited with this code.
    Exited(i32),
    /// A signal ended the shell, on Unix.
    Killed(i32),
}

impl ShellState {
    pub(super) fn of(status: ExitStatus) -> Self {
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Self::Killed(signal);
        }
        // Without a signal, `wait` always reports an exit code, so the fallback never applies.
        Self::Exited(status.code().unwrap_or(-1))
    }

    /// The state as a status line reads it, such as `exit code 0`.
    pub fn describe(self) -> String {
        match self {
            Self::Running => "running".to_owned(),
            Self::Exited(code) => format!("exit code {code}"),
            Self::Killed(signal) => format!("killed by signal {signal}"),
        }
    }
}

// ── Registry ──

/// The session's background shells. Each stays open, running or finished, until `bash_stop` stops
/// it or the session ends.
#[derive(Default)]
pub(crate) struct BackgroundShells {
    registry: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    shells: Vec<Shell>,
    started: u64,
    sink: Option<Arc<dyn EventSink>>,
}

struct Shell {
    id: String,
    command: String,
    output: Arc<ShellOutput>,
    /// Dropping it asks the supervisor to end the tree.
    stop: oneshot::Sender<()>,
    /// Finishes once the tree has ended or been released and the last output is in.
    supervisor: JoinHandle<()>,
}

impl BackgroundShells {
    /// Reports each change to the shell list through `sink` from now on.
    pub(crate) fn attach(&self, sink: Arc<dyn EventSink>) {
        self.lock().sink = Some(sink);
    }

    /// Starts `command` in the context's cwd and returns the ID its shell goes by.
    async fn start(self: &Arc<Self>, command: &str, ctx: &ToolCtx) -> Result<String, String> {
        let failed = |e| spawn_failure(ctx, &e);
        let bash = Bash::ready().await.map_err(failed)?;

        let mut registry = self.lock();
        if registry.shells.len() >= MAX_SHELLS {
            return Err(registry.full());
        }

        let (tree, pipe) = ShellTree::spawn(bash, command, ctx).map_err(failed)?;
        registry.started += 1;
        let id = format!("shell-{}", registry.started);
        let output = Arc::new(ShellOutput::new(ctx.spills()));
        let (stop, stopped) = oneshot::channel();
        let supervisor = tokio::spawn(supervise(
            tree,
            pipe,
            Arc::clone(&output),
            stopped,
            Arc::downgrade(self),
        ));

        registry.shells.push(Shell {
            id: id.clone(),
            command: command.to_owned(),
            output,
            stop,
            supervisor,
        });
        registry.publish();
        Ok(id)
    }

    fn output(&self, id: &str) -> Result<Arc<ShellOutput>, String> {
        let registry = self.lock();
        registry
            .shells
            .iter()
            .find(|shell| shell.id == id)
            .map(|shell| Arc::clone(&shell.output))
            .ok_or_else(|| registry.unknown(id))
    }

    fn remove(&self, id: &str) -> Result<Shell, String> {
        let mut registry = self.lock();
        let Some(index) = registry.shells.iter().position(|shell| shell.id == id) else {
            return Err(registry.unknown(id));
        };
        let shell = registry.shells.remove(index);
        registry.publish();
        Ok(shell)
    }

    /// Ends every shell along with the processes it started, as the session they belong to ends,
    /// and returns once each tree is gone. Reports no list, since frontends drop theirs when the
    /// session rolls or resumes.
    pub(crate) async fn stop_all(&self) {
        // Keeping only the handles drops every stop sender first, so the trees all end at once.
        let supervisors: Vec<_> = std::mem::take(&mut self.lock().shells)
            .into_iter()
            .map(|shell| shell.supervisor)
            .collect();
        for supervisor in supervisors {
            if let Err(e) = supervisor.await {
                error!(error = %e, "background shell supervisor failed");
            }
        }
    }

    /// The open shells, oldest first.
    pub(crate) fn list(&self) -> Vec<BackgroundShell> {
        self.lock().list()
    }

    fn publish(&self) {
        self.lock().publish();
    }

    fn lock(&self) -> MutexGuard<'_, Registry> {
        lock_or_recover(&self.registry, "background shells")
    }
}

impl Registry {
    fn list(&self) -> Vec<BackgroundShell> {
        self.shells
            .iter()
            .map(|shell| BackgroundShell {
                id: shell.id.clone(),
                command: shell.command.clone(),
                state: shell.output.state(),
            })
            .collect()
    }

    /// Reports the list through the attached sink. Callers hold the lock, so reports arrive in the
    /// order the list changed.
    fn publish(&self) {
        if let Some(sink) = &self.sink {
            sink.emit(Event::BackgroundShellsUpdated(self.list()));
        }
    }

    /// Why `id` names no open shell, with the shells that are open.
    fn unknown(&self, id: &str) -> String {
        if self.shells.is_empty() {
            return format!(
                "No background shell {id} is open, and none are. \
                 A shell closes when bash_stop stops it, when the session ends, and when ox exits."
            );
        }
        format!(
            "No background shell {id} is open. Open shells: {}.",
            self.open_shells()
        )
    }

    /// Why no shell can start, with the shells that fill the session.
    fn full(&self) -> String {
        format!(
            "A session keeps at most {MAX_SHELLS} background shells open, running or finished, \
             and these are open: {}. \
             Stop one you no longer need with bash_stop.",
            self.open_shells()
        )
    }

    /// Each open shell with its state, as in `shell-1 (running), shell-2 (exit code 0)`.
    fn open_shells(&self) -> String {
        self.list()
            .iter()
            .map(|shell| format!("{} ({})", shell.id, shell.state.describe()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ── Shell Output ──

/// What a shell printed since its last read, and its state, which its supervisor publishes.
struct ShellOutput {
    unread: Mutex<Unread>,
    state: watch::Sender<ShellState>,
    /// The spill files of the call that started the shell, one for each read that omits bytes.
    spills: Spills,
}

/// The output of the shell's `read`th read, counted from 1, which numbers its spill file.
struct Unread {
    read: usize,
    capture: PipeCapture,
}

impl Unread {
    fn new(spills: &Spills, read: usize) -> Self {
        let spill = spills.file(&format!(".{read}.output.txt"));
        Self {
            read,
            capture: PipeCapture::new(spill),
        }
    }
}

impl ShellOutput {
    fn new(spills: Spills) -> Self {
        Self {
            unread: Mutex::new(Unread::new(&spills, 1)),
            state: watch::Sender::new(ShellState::Running),
            spills,
        }
    }

    fn state(&self) -> ShellState {
        *self.state.borrow()
    }

    /// Waits up to `limit` for the shell to finish.
    async fn wait(&self, limit: Duration) {
        let mut state = self.state.subscribe();
        _ = tokio::time::timeout(limit, state.wait_for(|state| *state != ShellState::Running))
            .await;
    }

    /// Renders what the shell printed since the last read, bounded like a foreground call's output,
    /// and starts the next read empty. The render waits only for a spill write already in flight,
    /// so its markers name complete files.
    fn take(&self) -> String {
        let mut unread = {
            let mut unread = self.lock();
            let next = Unread::new(&self.spills, unread.read + 1);
            std::mem::replace(&mut *unread, next)
        };
        render_output(&mut unread.capture)
    }

    /// Runs `f` on the current read's capture.
    fn with_capture(&self, f: impl FnOnce(&mut PipeCapture)) {
        f(&mut self.lock().capture);
    }

    fn lock(&self) -> MutexGuard<'_, Unread> {
        lock_or_recover(&self.unread, "background shell output")
    }
}

// ── Supervisor ──

/// Runs one shell: pumps its `pipe` into `output` and publishes its state once the shell exits. A
/// stop kills the whole tree while the shell runs or a descendant still holds the pipe. Once the
/// shell has exited and the pipe has closed, the tree is released as a foreground call's is, so a
/// descendant that redirected its output keeps running.
async fn supervise(
    mut tree: ShellTree,
    pipe: ChildStdout,
    output: Arc<ShellOutput>,
    mut stop: oneshot::Receiver<()>,
    shells: Weak<BackgroundShells>,
) {
    let mut pumped = tokio::spawn({
        let output = Arc::clone(&output);
        async move { pump(pipe, &output).await }
    });

    let exited = tokio::select! {
        status = tree.wait() => Some(status),
        _ = &mut stop => None,
    };
    if let Some(status) = exited {
        // A read after the exit should see the last output, so the pipe gets to catch up.
        let drained = tokio::time::timeout(DRAIN_GRACE, &mut pumped).await.is_ok();
        output.state.send_replace(exit_state(status, &output));
        if let Some(shells) = shells.upgrade() {
            shells.publish();
        }
        let released = drained
            || tokio::select! {
                _ = &mut pumped => true,
                _ = &mut stop => false,
            };
        if released {
            tree.disarm();
            return;
        }
    }

    tree.kill().await;
    _ = tokio::time::timeout(DRAIN_GRACE, &mut pumped).await;
    pumped.abort();
}

fn exit_state(status: io::Result<ExitStatus>, output: &ShellOutput) -> ShellState {
    match status {
        Ok(status) => ShellState::of(status),
        Err(e) => {
            output.with_capture(|capture| {
                capture.push_note(&format!("Failed to wait for the command: {e}"));
            });
            ShellState::Exited(-1)
        }
    }
}

/// Copies `reader` into the current read's capture until the pipe closes. A read error lands in
/// the capture too, where the next read reports it.
async fn pump<R>(mut reader: R, output: &ShellOutput)
where
    R: AsyncRead + Unpin,
{
    let read = read_chunks(&mut reader, |chunk| {
        output.with_capture(|capture| capture.push(chunk));
    })
    .await;
    if let Err(e) = read {
        output.with_capture(|capture| {
            capture.push_note(&format!("Failed to read command output: {e}"));
        });
    }
}

// ── Starting ──

/// Starts `command` as a background shell for the `bash` tool.
pub(super) async fn start(
    shells: &Arc<BackgroundShells>,
    command: &str,
    ctx: &ToolCtx,
) -> ToolOutput {
    match shells.start(command, ctx).await {
        Ok(id) => ToolOutput::success(format!(
            "Started background shell {id}. \
             You are not notified when it finishes, \
             so check its output and state with bash_output, and stop it with bash_stop."
        )),
        Err(message) => ToolOutput::error(message),
    }
}

// ── Bash Output Tool ──

/// Reads what a background shell printed since the last read.
pub(crate) struct BashOutputTool {
    shells: Arc<BackgroundShells>,
}

impl BashOutputTool {
    pub(crate) fn new(shells: Arc<BackgroundShells>) -> Self {
        Self { shells }
    }
}

impl Tool for BashOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: "bash_output".to_owned(),
                description: "Read what a background shell printed since the last read, \
                              and whether it is still running. \
                              Pass a timeout to wait for the command to finish first, \
                              so one call can wait out a build. \
                              The wait ends when the command exits or the timeout passes, \
                              so for a command that keeps running, \
                              such as a server, it lasts the whole timeout. \
                              An unknown ID lists the open shells."
                    .to_owned(),
                input_schema: output_input_schema(),
            },
            // It reads only a shell the session started, so it never needs approval.
            class: PermissionClass::Read,
            // A read empties the shell's unread output.
            parallel_safe: false,
        }
    }

    fn prepare(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<Prepared, String> {
        Prepared::parse(input, |_: &OutputInput| Ok(Access::None))
    }

    fn icon(&self) -> &'static str {
        "»"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "shell_id")
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(read(&self.shells, prepared.into_input()))
    }
}

// ── Bash Stop Tool ──

/// Ends a background shell and every process it started.
pub(crate) struct BashStopTool {
    shells: Arc<BackgroundShells>,
}

impl BashStopTool {
    pub(crate) fn new(shells: Arc<BackgroundShells>) -> Self {
        Self { shells }
    }
}

impl Tool for BashStopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            definition: ToolDefinition {
                name: "bash_stop".to_owned(),
                description: "Stop a background shell and every process it started, \
                              and return what it printed since the last read. \
                              Stop a finished shell too once you no longer need its output, \
                              since a session keeps only a limited number of shells open."
                    .to_owned(),
                input_schema: stop_input_schema(),
            },
            // It ends only a shell the session started, so it never needs approval.
            class: PermissionClass::Read,
            parallel_safe: false,
        }
    }

    fn prepare(&self, input: serde_json::Value, _ctx: &ToolCtx) -> Result<Prepared, String> {
        Prepared::parse(input, |_: &StopInput| Ok(Access::None))
    }

    fn icon(&self) -> &'static str {
        "■"
    }

    fn summarize_input<'a>(&self, input: &'a serde_json::Value) -> Option<&'a str> {
        extract_input_field(input, "shell_id")
    }

    fn run<'a>(
        &'a self,
        prepared: Prepared,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(stop(&self.shells, prepared.into_input()))
    }
}

// ── Input ──

fn output_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "shell_id": shell_id_property(),
            "timeout": {
                "type": "integer",
                "description": "Milliseconds to wait for the command to finish \
                                before reading, capped at 600000. \
                                Omit or pass 0 to read at once."
            }
        },
        "required": ["shell_id"],
        "additionalProperties": false
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputInput {
    shell_id: String,
    timeout: Option<u64>,
}

fn stop_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "shell_id": shell_id_property()
        },
        "required": ["shell_id"],
        "additionalProperties": false
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopInput {
    shell_id: String,
}

fn shell_id_property() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "The ID bash returned when it started the shell, such as \"shell-1\""
    })
}

// ── Execution ──

async fn read(shells: &BackgroundShells, input: OutputInput) -> ToolOutput {
    let output = match shells.output(&input.shell_id) {
        Ok(output) => output,
        Err(message) => return ToolOutput::error(message),
    };
    if let Some(limit) = wait_limit(input.timeout) {
        output.wait(limit).await;
    }

    let footer = match output.state() {
        ShellState::Running => "(still running)".to_owned(),
        ended => format!("({})", ended.describe()),
    };
    ToolOutput::success(with_footer(output.take(), &footer))
}

fn wait_limit(millis: Option<u64>) -> Option<Duration> {
    millis
        .filter(|&ms| ms > 0)
        .map(|ms| Duration::from_millis(ms).min(MAX_TIMEOUT))
}

async fn stop(shells: &BackgroundShells, input: StopInput) -> ToolOutput {
    let Shell {
        output,
        stop,
        supervisor,
        ..
    } = match shells.remove(&input.shell_id) {
        Ok(shell) => shell,
        Err(message) => return ToolOutput::error(message),
    };
    drop(stop);
    if let Err(e) = supervisor.await {
        return ToolOutput::error(format!(
            "Failed to stop background shell {}: {e}",
            input.shell_id
        ));
    }

    let footer = match output.state() {
        ShellState::Running => "(stopped)".to_owned(),
        ShellState::Exited(code) => format!("(stopped after it exited with code {code})"),
        ShellState::Killed(signal) => format!("(stopped after signal {signal} killed it)"),
    };
    ToolOutput::success(with_footer(output.take(), &footer))
}

fn with_footer(mut content: String, footer: &str) -> String {
    if content.is_empty() {
        content.push_str(NO_NEW_OUTPUT_MARKER);
    }
    push_footer(&mut content, footer);
    content
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::sync::Arc;

    use super::BackgroundShells;
    use crate::tool::local_testing::temp_ctx;

    /// Starts `command` as a background shell of `shells` in the temp dir, returning its ID.
    pub(crate) async fn start_shell(shells: &Arc<BackgroundShells>, command: &str) -> String {
        shells
            .start(command, &temp_ctx())
            .await
            .expect("the shell starts")
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::{BackgroundShell, ShellState};

    /// A listed shell `id` running `command` at `state`.
    pub fn shell(id: &str, command: &str, state: ShellState) -> BackgroundShell {
        BackgroundShell {
            id: id.to_owned(),
            command: command.to_owned(),
            state,
        }
    }

    pub fn shells_in_each_state() -> Vec<BackgroundShell> {
        vec![
            shell("shell-1", "cargo watch", ShellState::Running),
            shell("shell-2", "make test", ShellState::Exited(2)),
            shell("shell-3", "sleep 60", ShellState::Killed(15)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::task::{Context, Poll};
    use std::time::Instant;

    use tokio::io::ReadBuf;

    use super::testing::shell;
    use super::*;
    use crate::protocol::local_testing::CapturingSink;
    use crate::tool::bash::BashTool;
    use crate::tool::bash::testing::{record_pid, until_gone, until_recorded};
    use crate::tool::local_testing::{self, ctx, spill_path, temp_ctx};
    use crate::tool::spill::MAX_OUTPUT_BYTES;

    /// A registry that reports its list to the returned sink.
    fn shells() -> (Arc<BackgroundShells>, CapturingSink) {
        let shells = Arc::new(BackgroundShells::default());
        let sink = CapturingSink::default();
        shells.attach(Arc::new(sink.clone()));
        (shells, sink)
    }

    async fn start(shells: &Arc<BackgroundShells>, command: &str) -> ToolOutput {
        start_in(shells, command, &temp_ctx()).await
    }

    /// Starts `command` with `ctx`, whose session directory takes the shell's spill files.
    async fn start_in(shells: &Arc<BackgroundShells>, command: &str, ctx: &ToolCtx) -> ToolOutput {
        let input = serde_json::json!({"command": command, "run_in_background": true});
        local_testing::run(&BashTool::new(Arc::clone(shells)), input, ctx).await
    }

    async fn output(shells: &Arc<BackgroundShells>, id: &str, timeout: u64) -> String {
        let input = serde_json::json!({"shell_id": id, "timeout": timeout});
        local_testing::run(&BashOutputTool::new(Arc::clone(shells)), input, &temp_ctx())
            .await
            .content
    }

    /// A command that prints `hi`, then keeps running beside a background `sleep` whose PID it
    /// records in `pid_file`.
    fn leaking(pid_file: &Path) -> String {
        format!(
            "echo hi; sleep 30 & {}; sleep 30",
            record_pid("$!", pid_file)
        )
    }

    mod background_shell {
        use super::*;

        mod command_preview {
            use super::*;

            #[test]
            fn keeps_the_first_line_and_marks_the_rest() {
                let preview =
                    |command| shell("shell-1", command, ShellState::Running).command_preview();

                assert_eq!(preview("cargo watch\n"), "cargo watch");
                assert_eq!(
                    preview("cat <<'EOF' > notes\nhello\nEOF"),
                    "cat <<'EOF' > notes ..."
                );
            }
        }
    }

    mod background_shells {
        use super::*;

        mod start {
            use super::*;

            #[tokio::test]
            async fn returns_the_shell_id_and_lists_the_running_shell() {
                let (shells, sink) = shells();

                let started = start(&shells, "sleep 30").await;

                assert!(!started.is_error, "{}", started.content);
                assert_eq!(
                    started.content,
                    "Started background shell shell-1. \
                     You are not notified when it finishes, \
                     so check its output and state with bash_output, and stop it with bash_stop.",
                );
                assert_eq!(
                    sink.events(),
                    [Event::BackgroundShellsUpdated(vec![shell(
                        "shell-1",
                        "sleep 30",
                        ShellState::Running,
                    )])],
                );
                shells.stop_all().await;
            }

            #[tokio::test]
            async fn refuses_a_shell_past_the_cap_and_names_the_open_ones() {
                let (shells, _sink) = shells();
                for _ in 0..MAX_SHELLS {
                    assert!(!start(&shells, "sleep 30").await.is_error);
                }

                let refused = start(&shells, "sleep 30").await;

                assert!(refused.is_error);
                let open: Vec<_> = (1..=MAX_SHELLS)
                    .map(|n| format!("shell-{n} (running)"))
                    .collect();
                assert_eq!(
                    refused.content,
                    format!(
                        "A session keeps at most 16 background shells open, running or finished, \
                         and these are open: {}. \
                         Stop one you no longer need with bash_stop.",
                        open.join(", "),
                    ),
                );
                shells.stop_all().await;
            }
        }
    }

    mod stop_all {
        use super::*;

        #[tokio::test]
        async fn ends_every_shell_and_its_descendants_without_a_report() {
            let dir = tempfile::tempdir().unwrap();
            let (shells, sink) = shells();
            let pid_files: Vec<_> = (0..2)
                .map(|n| dir.path().join(format!("pid-{n}")))
                .collect();
            let mut descendants = Vec::new();
            for pid_file in &pid_files {
                start(&shells, &leaking(pid_file)).await;
                descendants.push(until_recorded(pid_file).await);
            }
            let reported = sink.events().len();

            shells.stop_all().await;

            assert_eq!(shells.list(), []);
            assert_eq!(sink.events().len(), reported);
            for pid in &descendants {
                until_gone(pid).await;
            }
        }
    }

    mod exit_state {
        use super::*;

        #[test]
        fn reports_a_failed_wait_after_the_output_so_far() {
            let output = ShellOutput::new(temp_ctx().spills());
            output.with_capture(|capture| capture.push(b"warning: slow"));

            let state = exit_state(Err(io::Error::other("no child")), &output);

            assert_eq!(state, ShellState::Exited(-1));
            assert_eq!(
                output.take(),
                "warning: slow\nFailed to wait for the command: no child"
            );
        }
    }

    mod pump {
        use super::*;

        /// A pipe whose every read fails.
        struct BrokenPipe;

        impl AsyncRead for BrokenPipe {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Poll::Ready(Err(io::Error::other("pipe broke")))
            }
        }

        #[tokio::test]
        async fn reports_a_failed_read_in_the_capture() {
            let output = ShellOutput::new(temp_ctx().spills());

            pump(BrokenPipe, &output).await;

            assert_eq!(output.take(), "Failed to read command output: pipe broke");
        }
    }

    mod read {
        use super::*;

        #[tokio::test]
        async fn reads_only_what_arrived_since_the_last_read() {
            let dir = tempfile::tempdir().unwrap();
            let go = dir.path().join("go");
            let (shells, sink) = shells();
            let command = format!(
                "echo one; while [ ! -e '{}' ]; do sleep 0.02; done; echo two",
                go.display(),
            );
            start(&shells, &command).await;

            let mut first = output(&shells, "shell-1", 0).await;
            let deadline = Instant::now() + Duration::from_secs(5);
            while first == "(no new output)\n\n(still running)" && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
                first = output(&shells, "shell-1", 0).await;
            }
            fs::write(&go, "").unwrap();
            let second = output(&shells, "shell-1", 5_000).await;
            let third = output(&shells, "shell-1", 0).await;

            assert_eq!(first, "one\n\n(still running)");
            assert_eq!(second, "two\n\n(exit code 0)");
            assert_eq!(third, "(no new output)\n\n(exit code 0)");
            assert_eq!(
                sink.events().last(),
                Some(&Event::BackgroundShellsUpdated(vec![shell(
                    "shell-1",
                    &command,
                    ShellState::Exited(0),
                )])),
            );
            shells.stop_all().await;
        }

        #[tokio::test]
        async fn ends_its_wait_at_the_timeout_while_the_command_runs() {
            let (shells, _sink) = shells();
            start(&shells, "sleep 30").await;

            let running = output(&shells, "shell-1", 1).await;

            assert_eq!(running, "(no new output)\n\n(still running)");
            shells.stop_all().await;
        }

        #[tokio::test]
        async fn spills_each_read_that_omits_bytes_to_its_own_file() {
            let dir = tempfile::tempdir().unwrap();
            let (ready, go) = (dir.path().join("ready"), dir.path().join("go"));
            let (shells, _sink) = shells();
            let command = format!(
                "head -c 400000 /dev/zero | tr '\\0' a; {}; \
                 while [ ! -e '{}' ]; do sleep 0.02; done; head -c 400000 /dev/zero | tr '\\0' b",
                record_pid("$$", &ready),
                go.display(),
            );
            start_in(&shells, &command, &ctx(dir.path())).await;
            until_recorded(&ready).await;

            let first = output(&shells, "shell-1", 0).await;
            fs::write(&go, "").unwrap();
            let second = output(&shells, "shell-1", 5_000).await;

            let spills = [1, 2].map(|read| spill_path(dir.path(), &format!(".{read}.output.txt")));
            for (read, spill) in [&first, &second].into_iter().zip(&spills) {
                let note = format!(
                    "bytes omitted, full output saved to {}] ...",
                    spill.display()
                );
                assert!(read.contains(&note), "{}", &read[..read.len().min(200)]);
                assert!(read.len() <= MAX_OUTPUT_BYTES);
            }
            assert!(second.ends_with("\n\n(exit code 0)"));
            let spilled = spills.map(|spill| fs::read(spill).unwrap()).concat();
            let sent = [vec![b'a'; 400_000], vec![b'b'; 400_000]].concat();
            assert!(spilled == sent, "the spills hold every byte sent");
            shells.stop_all().await;
        }

        #[tokio::test]
        async fn names_the_open_shells_for_an_unknown_id() {
            let (shells, _sink) = shells();
            let none_open = output(&shells, "shell-9", 0).await;

            start(&shells, "sleep 30").await;
            start(&shells, "exit 3").await;
            output(&shells, "shell-2", 5_000).await;
            let some_open = output(&shells, "shell-9", 0).await;

            assert_eq!(
                none_open,
                "No background shell shell-9 is open, and none are. \
                 A shell closes when bash_stop stops it, when the session ends, \
                 and when ox exits.",
            );
            assert_eq!(
                some_open,
                "No background shell shell-9 is open. \
                 Open shells: shell-1 (running), shell-2 (exit code 3).",
            );
            shells.stop_all().await;
        }
    }

    mod wait_limit {
        use super::*;

        #[test]
        fn skips_zero_and_caps_the_ceiling() {
            assert_eq!(wait_limit(None), None);
            assert_eq!(wait_limit(Some(0)), None);
            assert_eq!(wait_limit(Some(5_000)), Some(Duration::from_secs(5)));
            assert_eq!(wait_limit(Some(u64::MAX)), Some(MAX_TIMEOUT));
        }
    }

    mod stop {
        use super::*;

        async fn stop(shells: &Arc<BackgroundShells>, id: &str) -> ToolOutput {
            let input = serde_json::json!({"shell_id": id});
            local_testing::run(&BashStopTool::new(Arc::clone(shells)), input, &temp_ctx()).await
        }

        #[tokio::test]
        async fn ends_the_tree_and_returns_the_unread_output() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let (shells, sink) = shells();
            start(&shells, &leaking(&pid_file)).await;
            let pid = until_recorded(&pid_file).await;

            let stopped = stop(&shells, "shell-1").await;

            assert!(!stopped.is_error, "{}", stopped.content);
            assert_eq!(stopped.content, "hi\n\n(stopped)");
            assert_eq!(
                sink.events().last(),
                Some(&Event::BackgroundShellsUpdated(Vec::new()))
            );
            let again = stop(&shells, "shell-1").await;
            assert!(again.is_error);
            assert_eq!(
                again.content,
                "No background shell shell-1 is open, and none are. \
                 A shell closes when bash_stop stops it, when the session ends, \
                 and when ox exits.",
            );
            until_gone(&pid).await;
        }

        #[tokio::test]
        async fn ends_a_descendant_still_holding_the_pipe_of_a_finished_shell() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let (shells, _sink) = shells();
            let command = format!("sleep 30 & {}; exit 3", record_pid("$!", &pid_file));
            start(&shells, &command).await;

            let read = output(&shells, "shell-1", 5_000).await;
            let stopped = stop(&shells, "shell-1").await;

            assert_eq!(read, "(no new output)\n\n(exit code 3)");
            assert!(!stopped.is_error, "{}", stopped.content);
            assert_eq!(
                stopped.content,
                "(no new output)\n\n(stopped after it exited with code 3)"
            );
            until_gone(&until_recorded(&pid_file).await).await;
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn reports_the_signal_that_ended_the_shell() {
            let (shells, _sink) = shells();
            start(&shells, "kill -9 $$").await;

            let read = output(&shells, "shell-1", 5_000).await;
            let stopped = stop(&shells, "shell-1").await;

            assert_eq!(read, "(no new output)\n\n(killed by signal 9)");
            assert_eq!(
                stopped.content,
                "(no new output)\n\n(stopped after signal 9 killed it)"
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn leaves_a_descendant_that_released_the_pipe_as_a_foreground_call_does() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let (shells, _sink) = shells();
            let command = format!(
                "sleep 30 >/dev/null 2>&1 & echo $! > '{}'",
                pid_file.display()
            );
            start(&shells, &command).await;
            output(&shells, "shell-1", 5_000).await;

            let stopped = stop(&shells, "shell-1").await;

            let pid = fs::read_to_string(&pid_file).unwrap();
            let signal = |args: &[&str]| {
                std::process::Command::new("kill")
                    .args(args)
                    .arg(pid.trim())
                    .status()
                    .unwrap()
                    .success()
            };
            let alive = signal(&["-0"]);
            signal(&["-9"]);
            assert!(alive, "the stop killed a descendant that released the pipe");
            assert_eq!(
                stopped.content,
                "(no new output)\n\n(stopped after it exited with code 0)"
            );
        }
    }
}
