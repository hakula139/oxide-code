//! Stdio output and the op loop shared by the `--no-tui` REPL and `--print` headless modes.

mod wire;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::pin::pin;

use anyhow::Result;
use clap::ValueEnum;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::debug;

use ox_core::permission::{Basis, SessionGrant};
use ox_core::protocol::{
    ApprovalDecision, Event, Op, ToolCall, ToolResult, TurnEnd, UsageSnapshot, notice,
};
use ox_core::session::display::{UNTITLED_MARKER, archive_notice, id_prefix};
use ox_core::session::store::listing::ArchiveState;
use ox_core::tool::tasks::{Progress, Task};
use ox_core::tool::view::ApprovalPreview;
use ox_core::util::text::{escape_controls, escape_line};
use ox_tui::glyphs::{INTERRUPTED_MARKER, task_marker};

use super::Shutdown;

// ── Op Loop ──

/// How [`run_op`] returned.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OpEnd {
    /// The op ended this way.
    Ended(TurnEnd),
    /// The engine stopped before the op ended.
    EngineStopped,
    /// A signal stopped the run.
    Signal(Shutdown),
}

/// Sends `op`, then renders `events` to `output` until `op` ends, the engine stops, or a `signal`
/// stops the run. Approval requests go to `approver`, and its answer goes back on `ops`. Events
/// that arrive while `approver` decides wait until it answers, unless one ends `op`, such as the
/// cancellation an interrupt causes, which drops the question. An engine for a run nobody attends
/// requests no approval, so such a run passes no `approver`.
///
/// SIGINT in a run with an `approver` interrupts `op`, as it does in a shell, while any other
/// signal stops the run. Once stdout's reader goes away, `op` is interrupted, since nothing reads
/// the rest. Each `signal` call must be cancel safe, as [`Signals::recv`](super::Signals::recv) is,
/// since the loop starts a new one for every event.
pub(super) async fn run_op<O: Write, E: Write, A: Approver>(
    events: &mut mpsc::UnboundedReceiver<Event>,
    ops: &mpsc::Sender<Op>,
    output: &mut Output<O, E>,
    signal: &mut impl AsyncFnMut() -> Shutdown,
    op: Op,
    mut approver: Option<&mut A>,
) -> OpEnd {
    let attended = approver.is_some();
    // A send fails only once the engine has stopped, which the next `recv` reports.
    _ = ops.send(op.clone()).await;
    let mut held = VecDeque::new();
    let mut interrupted = false;
    loop {
        let event = if let Some(event) = held.pop_front() {
            event
        } else {
            tokio::select! {
                biased;

                shutdown = signal() => {
                    if let Some(end) = on_signal(output, ops, attended, shutdown).await {
                        return end;
                    }
                    continue;
                }
                event = events.recv() => match event {
                    Some(event) => event,
                    None => return OpEnd::EngineStopped,
                },
            }
        };
        let Event::ApprovalRequested {
            id,
            preview,
            basis,
            grant,
        } = event
        else {
            let end = TurnEnd::of(&op, &event);
            output.render(event);
            if let Some(end) = end {
                return OpEnd::Ended(end);
            }
            if output.stdout_lost() && !interrupted {
                interrupted = true;
                _ = ops.send(Op::Interrupt).await;
            }
            continue;
        };

        let approver = approver
            .as_deref_mut()
            .expect("an engine requests approval only in an attended run");
        let mut decision = pin!(approver.decide(&preview, &basis, grant.as_deref()));
        let decision = loop {
            tokio::select! {
                biased;

                decision = &mut decision => break Some(decision),
                event = events.recv() => {
                    let Some(event) = event else {
                        return OpEnd::EngineStopped;
                    };
                    let ends = TurnEnd::of(&op, &event).is_some();
                    held.push_back(event);
                    if ends {
                        break None;
                    }
                }
                shutdown = signal() => {
                    if let Some(end) = on_signal(output, ops, attended, shutdown).await {
                        return end;
                    }
                    break None;
                }
            }
        };
        if let Some(decision) = decision {
            let decision = decision.unwrap_or_else(|error| {
                output.record(Err(error));
                ApprovalDecision::Abort
            });
            _ = ops.send(Op::Resolve { id, decision }).await;
        }
    }
}

/// Moves past the `^C` that `shutdown` echoed, then returns how the run ends, or `None` once SIGINT
/// in an attended run has interrupted the op, which keeps rendering until the op ends.
async fn on_signal<O: Write, E: Write>(
    output: &mut Output<O, E>,
    ops: &mpsc::Sender<Op>,
    attended: bool,
    shutdown: Shutdown,
) -> Option<OpEnd> {
    output.break_line();
    if attended && shutdown == Shutdown::Interrupt {
        _ = ops.send(Op::Interrupt).await;
        return None;
    }
    Some(OpEnd::Signal(shutdown))
}

/// How a stdio frontend answers an approval request.
pub(super) trait Approver {
    /// Decides the call `preview` shows, which `basis` made ask and `grant` may allow for the rest
    /// of the session. An error aborts the call and is retained until the run ends.
    fn decide(
        &mut self,
        preview: &ApprovalPreview,
        basis: &Basis,
        grant: Option<&SessionGrant>,
    ) -> impl Future<Output = Result<ApprovalDecision>>;
}

/// Uninhabited approval type for unattended runs, which pass no approver.
pub(super) enum Unattended {}

impl Approver for Unattended {
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "an uninhabited type has no value to await on"
    )]
    async fn decide(
        &mut self,
        _: &ApprovalPreview,
        _: &Basis,
        _: Option<&SessionGrant>,
    ) -> Result<ApprovalDecision> {
        match *self {}
    }
}

// ── Output ──

/// What a stdio run prints on stdout. The REPL always prints text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum OutputFormat {
    /// The reply as plain text.
    #[default]
    Text,
    /// One JSON result once the run ends.
    Json,
    /// Each event as a JSON line, then the result.
    StreamJson,
}

/// A stdio run's output in its [`OutputFormat`]. Progress, tool calls, and errors go to stderr as
/// text in every format, so stdout carries only the reply or the JSON. A failed write leaves the
/// rest of the run going and is retained until [`Self::finish`].
pub(super) struct Output<O, E> {
    stdout: Watched<O>,
    stderr: E,
    format: OutputFormat,
    text: TextState,
    /// What the result of a JSON run reports.
    summary: RunSummary,
    failure: Option<anyhow::Error>,
}

/// What the result line of a JSON run reports besides how it ended.
#[derive(Default)]
struct RunSummary {
    error: Option<String>,
    session_write_error: Option<String>,
    /// Reply text since the last tool call, which the result reports as the final message.
    message: String,
    usage: Option<UsageSnapshot>,
    /// The session a fork moved the run to, which the result reports in place of the one it
    /// started in.
    forked_to: Option<String>,
}

impl<O: Write, E: Write> Output<O, E> {
    pub(super) fn new(format: OutputFormat, show_thinking: bool, stdout: O, stderr: E) -> Self {
        Self {
            stdout: Watched::new(stdout),
            stderr,
            format,
            // Thinking renders on stdout, so the JSON formats leave it off, which also keeps the
            // progress notes it would replace on stderr.
            text: TextState::new(show_thinking && format == OutputFormat::Text),
            summary: RunSummary::default(),
            failure: None,
        }
    }

    /// Prints `event`.
    pub(super) fn render(&mut self, event: Event) {
        let Self {
            stdout,
            stderr,
            format,
            text,
            summary,
            ..
        } = self;
        if *format != OutputFormat::Text {
            summary.observe(&event);
        }
        let result = match format {
            OutputFormat::Text => text.render(event, stdout, stderr),
            OutputFormat::Json => text.render(event, &mut io::sink(), stderr),
            OutputFormat::StreamJson => {
                let written =
                    wire::wire(&event).map_or(Ok(()), |line| write_json_line(stdout, &line));
                text.render(event, &mut io::sink(), stderr).and(written)
            }
        };
        self.record(result);
    }

    /// Prints the events already queued, such as the engine's startup warnings.
    pub(super) fn render_queued(&mut self, events: &mut mpsc::UnboundedReceiver<Event>) {
        while let Ok(event) = events.try_recv() {
            self.render(event);
        }
    }

    /// Prints that the session could not be saved, which the result of a JSON run also reports.
    pub(super) fn session_write_error(&mut self, failure: &str) {
        self.summary.session_write_error = Some(failure.to_owned());
        self.render(Event::Error(notice::session_write_failed(failure)));
    }

    /// Moves to a new line after a signal, past the `^C` a terminal echoes.
    pub(super) fn break_line(&mut self) {
        if self.format != OutputFormat::Text {
            // The JSON formats render replies into a sink, so their open line is not on stdout.
            self.text.line_open = false;
        }
        let result = self.text.break_line(&mut self.stdout, &mut self.stderr);
        self.record(result.map_err(Into::into));
    }

    /// Prints the result that ends a JSON run, or ends the reply line a text run left open.
    /// `session_id` names the session the run started in.
    pub(super) fn print_result(&mut self, stop_reason: TurnEnd, session_id: &str) {
        let result = if self.format == OutputFormat::Text {
            self.text.end_line(&mut self.stdout).map_err(Into::into)
        } else {
            let RunSummary {
                error,
                session_write_error,
                message,
                usage,
                forked_to,
            } = &self.summary;
            let line = wire::HeadlessEvent::Result(wire::RunResult {
                stop_reason,
                error: error.as_deref(),
                session_write_error: session_write_error.as_deref(),
                message: (stop_reason == TurnEnd::Completed).then_some(message.as_str()),
                usage: usage.as_ref(),
                session_id: forked_to.as_deref().unwrap_or(session_id),
            });
            write_json_line(&mut self.stdout, &line)
        };
        self.record(result);
    }

    /// Whether stdout's reader went away, so nothing reads the rest of the run.
    pub(super) fn stdout_lost(&self) -> bool {
        self.stdout.lost
    }

    /// Retains the error of `result` unless an earlier one is retained.
    pub(super) fn record(&mut self, result: Result<()>) {
        if let Err(error) = result {
            debug!(error = %format_args!("{error:#}"), "stdio output failed");
            self.failure.get_or_insert(error);
        }
    }

    /// Returns the first failure retained, which it clears.
    pub(super) fn finish(&mut self) -> Result<()> {
        self.failure.take().map_or(Ok(()), Err)
    }

    /// Reports that output failed on the stream every other line uses.
    pub(super) fn report_failure(&mut self, error: &anyhow::Error) {
        _ = writeln!(self.stderr, "Error: failed to print output: {error:#}");
    }
}

impl RunSummary {
    fn observe(&mut self, event: &Event) {
        match event {
            Event::StreamToken(text) => self.message.push_str(text),
            Event::ToolCallStart(_) => self.message.clear(),
            Event::TurnFailed(error) => self.error = Some(error.clone()),
            Event::UsageUpdated(usage) => self.usage = Some(*usage),
            Event::SessionForked { id, .. } => self.forked_to = Some(id.clone()),
            _ => {}
        }
    }
}

/// Notes when a write or flush to the wrapped stream fails because its reader went away.
struct Watched<W> {
    inner: W,
    lost: bool,
}

impl<W: Write> Watched<W> {
    fn new(inner: W) -> Self {
        Self { inner, lost: false }
    }

    fn watch<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        result.inspect_err(|error| self.lost |= error.kind() == io::ErrorKind::BrokenPipe)
    }
}

impl<W: Write> Write for Watched<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = self.inner.write(buf);
        self.watch(result)
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.inner.flush();
        self.watch(result)
    }
}

fn write_json_line(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    Ok(())
}

// ── Text ──

/// Starts dim text, the style used for thinking.
const DIM: &str = "\x1b[2m";
/// Restores normal intensity.
const NORMAL_INTENSITY: &str = "\x1b[22m";

/// Renders events as text: assistant tokens stream to stdout and everything else to stderr, so
/// piped output stays clean. Model and tool text passes through [`escape_controls`], so it cannot
/// redraw the terminal around a REPL approval prompt. An open reply line on stdout ends before a
/// tool call starts, before the reply that follows shown thinking, and when the operation ends.
struct TextState {
    show_thinking: bool,
    /// Whether the last text written to stdout left the current line open.
    line_open: bool,
    /// Whether the last text written to stdout was thinking.
    thinking: bool,
    calls: StartedCalls,
}

/// The started calls awaiting a result, so a result printed after another call's header can repeat
/// its own.
#[derive(Default)]
struct StartedCalls {
    headers: HashMap<String, String>,
    last: Option<String>,
}

impl TextState {
    fn new(show_thinking: bool) -> Self {
        Self {
            show_thinking,
            line_open: false,
            thinking: false,
            calls: StartedCalls::default(),
        }
    }

    /// Writes `event` to `stdout` or `stderr`.
    fn render(
        &mut self,
        event: Event,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> Result<()> {
        match event {
            Event::StreamToken(text) => {
                if std::mem::take(&mut self.thinking) {
                    self.end_line(stdout)?;
                }
                self.write_reply(stdout, &escape_controls(&text), "", "")?;
            }
            Event::ThinkingToken(text) => {
                if self.show_thinking {
                    let text = escape_controls(&text);
                    self.write_reply(stdout, &text, DIM, NORMAL_INTENSITY)?;
                    self.thinking = true;
                }
            }
            Event::ProgressUpdate(text) => {
                if !self.show_thinking {
                    writeln!(stderr, "{}", escape_controls(text.trim()))?;
                }
            }
            Event::ToolCallStart(ToolCall {
                id, icon, label, ..
            }) => {
                let header = format!("{icon} {}", escape_controls(&label));
                self.end_reply_then(stdout, stderr, &header)?;
                self.calls.start(id, header);
            }
            Event::ToolCallEnd(result) => self.render_result(result, stderr)?,
            Event::TasksUpdated(tasks) => write!(stderr, "{}", task_lines(&tasks))?,
            Event::AutoCompactionStarted => {
                self.end_reply_then(stdout, stderr, notice::AUTO_COMPACTING)?;
            }
            Event::SessionCompacted {
                pre_count,
                instructions,
                ..
            } => {
                let notice = notice::compacted(pre_count, instructions.as_deref());
                writeln!(stderr, "{}", escape_controls(&notice))?;
            }
            Event::SessionTitleUpdated { title, .. } => {
                let title = escape_line(&title);
                writeln!(stderr, "Renamed the session to {title}")?;
            }
            Event::SessionRolled { id, .. } => {
                writeln!(stderr, "Started session {}", id_prefix(&id))?;
            }
            Event::SessionResumed {
                id, title, prior, ..
            } => {
                let notice = resumed_notice(&id, title.as_deref(), prior);
                writeln!(stderr, "{notice}")?;
            }
            Event::SessionForked { id, from } => {
                self.end_reply_then(stdout, stderr, &notice::session_forked(&id, &from))?;
            }
            Event::ConfigChanged { notice, .. } => writeln!(stderr, "{notice}")?,
            Event::ModeChanged(mode) => writeln!(stderr, "{}", notice::mode_changed(mode))?,
            // TUI-only events and the `SessionReady` marker have no stdio surface to update.
            Event::PromptDrained(_)
            | Event::TurnRolledBack
            | Event::PromptUnsent
            | Event::UsageUpdated(_)
            | Event::RateLimitsUpdated(_)
            | Event::AutoCompactionPaused { .. }
            | Event::AutoCompactionResumed
            | Event::SummaryToken(_)
            | Event::SummaryDiscarded
            | Event::LoginStarted { .. }
            | Event::SignedIn { .. }
            | Event::LoginRestored { .. }
            | Event::SignedOut { .. }
            | Event::ApprovalRequested { .. }
            | Event::ArchiveUpdated { .. }
            | Event::SavedFound(_)
            | Event::SavedDeleted { .. }
            | Event::SessionRulesUpdated(_)
            | Event::BackgroundShellsUpdated(_)
            | Event::SessionReady => {}
            Event::TurnComplete => self.end_line(stdout)?,
            Event::Cancelled => self.end_reply_then(stdout, stderr, INTERRUPTED_MARKER)?,
            Event::Retrying(msg) | Event::Notice(msg) => {
                writeln!(stderr, "{}", escape_controls(&msg))?;
            }
            Event::TurnFailed(msg) => {
                let line = format!("Error: {}", escape_controls(&msg));
                self.end_reply_then(stdout, stderr, &line)?;
            }
            Event::Error(msg) | Event::RenameFailed(msg) => {
                writeln!(stderr, "Error: {}", escape_controls(&msg))?;
            }
        }
        Ok(())
    }

    /// Moves to a new line after a signal, past the `^C` a terminal echoes, by ending the open
    /// reply line on `stdout` or else writing a newline to `stderr`.
    fn break_line(&mut self, stdout: &mut impl Write, stderr: &mut impl Write) -> io::Result<()> {
        if self.line_open {
            return self.end_line(stdout);
        }
        writeln!(stderr)?;
        stderr.flush()
    }

    /// Ends the reply line left open on `stdout`, if any.
    fn end_line(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        if !std::mem::take(&mut self.line_open) {
            return Ok(());
        }
        writeln!(stdout)?;
        stdout.flush()
    }

    /// Writes reply or thinking `text` to `stdout` between `start` and `end` styles, noting whether
    /// it leaves the line open.
    fn write_reply(
        &mut self,
        stdout: &mut impl Write,
        text: &str,
        start: &str,
        end: &str,
    ) -> io::Result<()> {
        write!(stdout, "{start}{text}{end}")?;
        stdout.flush()?;
        if !text.is_empty() {
            self.line_open = !text.ends_with('\n');
        }
        Ok(())
    }

    /// Ends the open reply line, then writes `line` to `stderr`, which it does even when ending the
    /// reply line failed.
    fn end_reply_then(
        &mut self,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
        line: &str,
    ) -> io::Result<()> {
        let ended = self.end_line(stdout);
        writeln!(stderr, "{line}")?;
        ended
    }

    /// Writes a call's result under its title, after its call's header again when another call's
    /// header came after it.
    fn render_result(&mut self, result: ToolResult, stderr: &mut impl Write) -> Result<()> {
        let ToolResult {
            id, content, title, ..
        } = result;
        if let Some(header) = self.calls.end(&id) {
            writeln!(stderr, "{header}")?;
        }
        if let Some(title) = title {
            writeln!(stderr, "  {}", escape_controls(&title))?;
        }
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            writeln!(stderr, "{}", escape_controls(trimmed))?;
        }
        writeln!(stderr)?;
        Ok(())
    }
}

impl StartedCalls {
    fn start(&mut self, id: String, header: String) {
        self.headers.insert(id.clone(), header);
        self.last = Some(id);
    }

    /// The header to repeat above the result of the call `id`, when another call's header was
    /// printed after its own.
    fn end(&mut self, id: &str) -> Option<String> {
        let repeat = self.last.take().as_deref() != Some(id);
        self.headers.remove(id).filter(|_| repeat)
    }
}

/// A progress header, one marked line per task, and a closing blank line, or nothing for a cleared
/// list.
fn task_lines(tasks: &[Task]) -> String {
    let Some(progress) = Progress::of(tasks) else {
        return String::new();
    };
    let mut lines = format!("Tasks: {progress}\n");
    for task in tasks {
        let content = escape_controls(&task.content);
        _ = writeln!(lines, "  {} {content}", task_marker(task.status));
    }
    lines.push('\n');
    lines
}

/// The session an [`Event::SessionResumed`] continues, then a note when resuming it unarchived it.
fn resumed_notice(id: &str, title: Option<&str>, prior: ArchiveState) -> String {
    let title = title.map_or_else(|| UNTITLED_MARKER.to_owned(), escape_line);
    let resumed = format!("Resumed session {}: {title}", id_prefix(id));
    if prior == ArchiveState::Archived {
        return format!("{resumed}\n{}", archive_notice(id, ArchiveState::Active));
    }
    resumed
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use indoc::indoc;
    use wiremock::MockServer;

    use ox_core::client::{RateLimitWindow, RateLimits};
    use ox_core::config::compaction::AutoCompactionConfig;
    use ox_core::engine::testing::{engine, with_pinned_home};
    use ox_core::model::{Effort, ModelSpec};
    use ox_core::permission::Mode;
    use ox_core::permission::testing::exec_grant;
    use ox_core::protocol::testing::{ls_call, text_result};
    use ox_core::protocol::{CompactTrigger, ConfigPick, SavedTarget};
    use ox_core::session::saved::{SavedAction, SavedError};
    use ox_core::session::store::testing::test_store;
    use ox_core::tool::bash::background::testing::shells_in_each_state;
    use ox_core::tool::tasks::TaskStatus;
    use ox_core::tool::tasks::testing::{plan, task};
    use ox_core::tool::testing::bash_preview;
    use ox_core::util::log::testing::LogCapture;

    use super::*;
    use crate::driver::testing::{
        ClosedPipe, FailingWriter, Failure, Screen, no_signal, output, result_data,
    };

    mod run_op {
        use super::*;

        /// Answers every approval with its decision, recording what each request offered.
        struct Fixed {
            decision: ApprovalDecision,
            offered: Vec<(String, Option<String>)>,
        }

        impl Approver for Fixed {
            fn decide(
                &mut self,
                preview: &ApprovalPreview,
                _: &Basis,
                grant: Option<&SessionGrant>,
            ) -> impl Future<Output = Result<ApprovalDecision>> {
                let rule = grant.map(|grant| grant.suggested().pattern().to_owned());
                self.offered.push((preview.title.clone(), rule));
                std::future::ready(Ok(self.decision.clone()))
            }
        }

        /// Never answers, as a person who has not typed an answer yet.
        struct Undecided;

        impl Approver for Undecided {
            fn decide(
                &mut self,
                _: &ApprovalPreview,
                _: &Basis,
                _: Option<&SessionGrant>,
            ) -> impl Future<Output = Result<ApprovalDecision>> {
                std::future::pending()
            }
        }

        fn fixed(decision: ApprovalDecision) -> Fixed {
            Fixed {
                decision,
                offered: Vec::new(),
            }
        }

        fn prompt() -> Op {
            Op::prompt("hi")
        }

        fn approval(grant: Option<SessionGrant>) -> Event {
            Event::ApprovalRequested {
                id: "t1".to_owned(),
                preview: bash_preview("ls"),
                basis: Basis::Mode(Mode::Manual),
                grant: grant.map(Box::new),
            }
        }

        /// A channel of `events`, its sender kept open.
        fn queued(
            events: impl IntoIterator<Item = Event>,
        ) -> (mpsc::UnboundedSender<Event>, mpsc::UnboundedReceiver<Event>) {
            let (tx, rx) = mpsc::unbounded_channel();
            for event in events {
                tx.send(event).unwrap();
            }
            (tx, rx)
        }

        /// Every op sent on `ops` so far.
        fn sent(ops: &mut mpsc::Receiver<Op>) -> Vec<Op> {
            std::iter::from_fn(|| ops.try_recv().ok()).collect()
        }

        #[tokio::test]
        async fn answers_each_approval_on_ops() {
            let (_tx, mut events) = queued([
                approval(Some(exec_grant("ls"))),
                Event::Notice("Done.".to_owned()),
                Event::TurnComplete,
            ]);
            let (ops_tx, mut ops) = mpsc::channel(4);
            let (mut output, _, stderr) = output(OutputFormat::Text);
            let mut approver = fixed(ApprovalDecision::Deny);

            let end = run_op(
                &mut events,
                &ops_tx,
                &mut output,
                &mut no_signal,
                prompt(),
                Some(&mut approver),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Completed));
            assert_eq!(
                approver.offered,
                [("Bash".to_owned(), Some("ls".to_owned()))],
            );
            assert_eq!(
                sent(&mut ops),
                [
                    prompt(),
                    Op::Resolve {
                        id: "t1".to_owned(),
                        decision: ApprovalDecision::Deny,
                    },
                ],
            );
            assert_eq!(stderr.text(), "Done.\n");
        }

        #[tokio::test]
        async fn drops_an_unanswered_approval_once_the_op_ends() {
            let (_tx, mut events) = queued([
                approval(None),
                Event::ModeChanged(Mode::Plan),
                Event::Cancelled,
            ]);
            let (ops_tx, mut ops) = mpsc::channel(4);
            let (mut output, _, stderr) = output(OutputFormat::Text);

            let end = run_op(
                &mut events,
                &ops_tx,
                &mut output,
                &mut no_signal,
                prompt(),
                Some(&mut Undecided),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Cancelled));
            assert_eq!(
                stderr.text(),
                format!("Switched to plan mode\n{INTERRUPTED_MARKER}\n"),
            );
            assert_eq!(sent(&mut ops), [prompt()]);
        }

        #[tokio::test]
        async fn holds_events_that_arrive_during_an_approval_until_it_is_answered() {
            /// Answers once the events sent with the request have arrived, recording what had
            /// rendered by then.
            struct Watching {
                screen: Screen,
                rendered: Option<String>,
            }

            impl Approver for Watching {
                async fn decide(
                    &mut self,
                    _: &ApprovalPreview,
                    _: &Basis,
                    _: Option<&SessionGrant>,
                ) -> Result<ApprovalDecision> {
                    tokio::task::yield_now().await;
                    self.rendered = Some(self.screen.text());
                    Ok(ApprovalDecision::Deny)
                }
            }

            let (_tx, mut events) = queued([
                approval(None),
                Event::ModeChanged(Mode::Plan),
                Event::TurnComplete,
            ]);
            let (ops, _ops_rx) = mpsc::channel(4);
            let (mut output, _, stderr) = output(OutputFormat::Text);
            let mut approver = Watching {
                screen: stderr.clone(),
                rendered: None,
            };

            let end = run_op(
                &mut events,
                &ops,
                &mut output,
                &mut no_signal,
                prompt(),
                Some(&mut approver),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Completed));
            assert_eq!(approver.rendered.as_deref(), Some(""));
            assert_eq!(stderr.text(), "Switched to plan mode\n");
        }

        #[tokio::test]
        async fn stops_at_the_terminal_event_even_when_rendering_fails() {
            let (_tx, mut events) = queued([
                Event::StreamToken("reply".to_owned()),
                Event::Cancelled,
                Event::TurnComplete,
            ]);
            let (ops, _ops_rx) = mpsc::channel(4);
            let failure = Failure::Write("reply");
            let mut output = Output::new(
                OutputFormat::Text,
                false,
                FailingWriter::new(failure),
                io::sink(),
            );
            let mut approver = fixed(ApprovalDecision::Deny);

            let end = run_op(
                &mut events,
                &ops,
                &mut output,
                &mut no_signal,
                prompt(),
                Some(&mut approver),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Cancelled));
            assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
            assert_eq!(output.finish().unwrap_err().to_string(), failure.message());
        }

        #[tokio::test]
        async fn aborts_an_approval_with_failed_output_and_drains_its_terminal_event() {
            struct LostPrompt;

            impl Approver for LostPrompt {
                fn decide(
                    &mut self,
                    _: &ApprovalPreview,
                    _: &Basis,
                    _: Option<&SessionGrant>,
                ) -> impl Future<Output = Result<ApprovalDecision>> {
                    std::future::ready(Err(anyhow::anyhow!("lost approval prompt")))
                }
            }

            let (_tx, mut events) = queued([
                approval(Some(exec_grant("ls"))),
                Event::StreamToken("later output".to_owned()),
                Event::Cancelled,
                Event::TurnComplete,
            ]);
            let (ops_tx, mut ops) = mpsc::channel(4);
            let (mut output, stdout, stderr) = output(OutputFormat::Text);

            let end = run_op(
                &mut events,
                &ops_tx,
                &mut output,
                &mut no_signal,
                prompt(),
                Some(&mut LostPrompt),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Cancelled));
            assert_eq!(
                sent(&mut ops),
                [
                    prompt(),
                    Op::Resolve {
                        id: "t1".to_owned(),
                        decision: ApprovalDecision::Abort,
                    },
                ],
            );
            assert_eq!(stdout.text(), "later output\n");
            assert_eq!(stderr.text(), format!("{INTERRUPTED_MARKER}\n"));
            assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
            assert_eq!(
                output.finish().unwrap_err().to_string(),
                "lost approval prompt"
            );
        }

        #[tokio::test]
        async fn waits_for_the_terminal_event_of_its_own_op() {
            let (tx, mut events) = queued([
                Event::SessionRolled {
                    id: "next-session".to_owned(),
                    old_session: ArchiveState::Active,
                },
                Event::Error("Previous session write failed: disk full".to_owned()),
                Event::SessionReady,
                Event::TurnComplete,
            ]);
            drop(tx);
            let (ops, _ops_rx) = mpsc::channel(4);
            let (mut output, _, stderr) = output(OutputFormat::Text);
            let mut approver = fixed(ApprovalDecision::Deny);

            let end = run_op(
                &mut events,
                &ops,
                &mut output,
                &mut no_signal,
                Op::Clear,
                Some(&mut approver),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Completed));
            assert_eq!(
                stderr.text(),
                indoc! {"
                    Started session next-ses
                    Error: Previous session write failed: disk full
                "},
            );
            assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
        }

        #[tokio::test]
        async fn renders_what_the_engine_reports_after_a_clear_before_returning() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let instructions = dir.path().join("CLAUDE.md");
                std::fs::write(&instructions, b"\xff\xfe").unwrap();
                let mut handle = engine.spawn();
                let (mut output, _, stderr) = output(OutputFormat::Text);
                let mut approver = fixed(ApprovalDecision::Deny);

                let end = run_op(
                    &mut handle.events,
                    &handle.ops,
                    &mut output,
                    &mut no_signal,
                    Op::Clear,
                    Some(&mut approver),
                )
                .await;

                assert_eq!(end, OpEnd::Ended(TurnEnd::Completed));
                let stderr = stderr.text();
                let lines: Vec<&str> = stderr.lines().collect();
                let [rolled, warning] = lines[..] else {
                    panic!("the roll and the warning: {stderr}");
                };
                assert!(rolled.starts_with("Started session "), "{stderr}");
                assert_eq!(
                    warning,
                    format!(
                        "Skipped instruction file {}: stream did not contain valid UTF-8",
                        instructions.display(),
                    ),
                );
                handle.shutdown().await.unwrap();
            })
            .await;
        }

        #[tokio::test]
        async fn reports_a_stopped_engine() {
            for held in [false, true] {
                let pending = if held {
                    approval(None)
                } else {
                    Event::StreamToken("partial".to_owned())
                };
                let (tx, mut events) = queued([pending]);
                drop(tx);
                let (ops, _ops_rx) = mpsc::channel(4);
                let (mut output, stdout, _) = output(OutputFormat::Text);

                let end = run_op(
                    &mut events,
                    &ops,
                    &mut output,
                    &mut no_signal,
                    prompt(),
                    Some(&mut Undecided),
                )
                .await;

                assert_eq!(end, OpEnd::EngineStopped, "held: {held}");
                let expected = if held { "" } else { "partial" };
                assert_eq!(stdout.text(), expected, "held: {held}");
            }
        }

        #[tokio::test]
        async fn interrupts_once_only_when_stdout_loses_its_reader() {
            type Stream = Box<dyn Write>;

            let cases: [(&str, Stream, Stream, bool); 3] = [
                (
                    "closed stdout",
                    Box::new(ClosedPipe),
                    Box::new(io::sink()),
                    true,
                ),
                (
                    "failed write",
                    Box::new(FailingWriter::new(Failure::Write("first"))),
                    Box::new(io::sink()),
                    false,
                ),
                (
                    "closed stderr",
                    Box::new(io::sink()),
                    Box::new(ClosedPipe),
                    false,
                ),
            ];
            for (case, stdout, stderr, interrupts) in cases {
                let (_tx, mut events) = queued([
                    Event::StreamToken("first".to_owned()),
                    Event::Notice("Reading.".to_owned()),
                    Event::StreamToken("second".to_owned()),
                    Event::TurnComplete,
                ]);
                let (ops_tx, mut ops) = mpsc::channel(4);
                let mut output = Output::new(OutputFormat::StreamJson, false, stdout, stderr);

                let end = run_op(
                    &mut events,
                    &ops_tx,
                    &mut output,
                    &mut no_signal,
                    prompt(),
                    None::<&mut Unattended>,
                )
                .await;

                assert_eq!(end, OpEnd::Ended(TurnEnd::Completed), "{case}");
                let expected = if interrupts {
                    vec![prompt(), Op::Interrupt]
                } else {
                    vec![prompt()]
                };
                assert_eq!(sent(&mut ops), expected, "{case}");
                assert!(output.finish().is_err(), "{case}");
            }
        }

        #[tokio::test]
        async fn sigint_interrupts_an_attended_op_and_other_signals_stop_the_run() {
            for (attended, signal, ends) in [
                (true, Shutdown::Interrupt, OpEnd::Ended(TurnEnd::Cancelled)),
                (
                    false,
                    Shutdown::Interrupt,
                    OpEnd::Signal(Shutdown::Interrupt),
                ),
                #[cfg(unix)]
                (
                    true,
                    Shutdown::Terminate,
                    OpEnd::Signal(Shutdown::Terminate),
                ),
            ] {
                let (tx, mut events) = queued([Event::StreamToken("partial".to_owned())]);
                let (ops_tx, mut ops) = mpsc::channel(4);
                let (mut output, stdout, stderr) = output(OutputFormat::Text);
                let mut fired = false;
                let mut once = async || {
                    if fired {
                        return std::future::pending().await;
                    }
                    tokio::task::yield_now().await;
                    fired = true;
                    tx.send(Event::Cancelled).unwrap();
                    signal
                };
                let mut approver = fixed(ApprovalDecision::Deny);

                let end = run_op(
                    &mut events,
                    &ops_tx,
                    &mut output,
                    &mut once,
                    prompt(),
                    attended.then_some(&mut approver),
                )
                .await;

                assert_eq!(end, ends, "{attended}, {signal:?}");
                let interrupts = matches!(end, OpEnd::Ended(_));
                let expected = if interrupts {
                    vec![prompt(), Op::Interrupt]
                } else {
                    vec![prompt()]
                };
                assert_eq!(sent(&mut ops), expected, "{attended}, {signal:?}");
                assert_eq!(stdout.text(), "partial\n", "{attended}, {signal:?}");
                let marker = if interrupts {
                    format!("{INTERRUPTED_MARKER}\n")
                } else {
                    String::new()
                };
                assert_eq!(stderr.text(), marker, "{attended}, {signal:?}");
            }
        }

        #[tokio::test]
        async fn sigint_during_an_approval_drops_the_question() {
            let (tx, mut events) = queued([approval(None)]);
            let (ops_tx, mut ops) = mpsc::channel(4);
            let (mut output, _, stderr) = output(OutputFormat::Text);
            let mut fired = false;
            let mut once = async || {
                if fired {
                    return std::future::pending().await;
                }
                tokio::task::yield_now().await;
                fired = true;
                tx.send(Event::Cancelled).unwrap();
                Shutdown::Interrupt
            };

            let end = run_op(
                &mut events,
                &ops_tx,
                &mut output,
                &mut once,
                prompt(),
                Some(&mut Undecided),
            )
            .await;

            assert_eq!(end, OpEnd::Ended(TurnEnd::Cancelled));
            assert_eq!(sent(&mut ops), [prompt(), Op::Interrupt]);
            assert_eq!(stderr.text(), format!("\n{INTERRUPTED_MARKER}\n"));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn sigterm_during_an_approval_stops_the_run() {
            let (_tx, mut events) = queued([approval(None)]);
            let (ops_tx, mut ops) = mpsc::channel(4);
            let (mut output, _, stderr) = output(OutputFormat::Text);
            let mut signal = async || {
                tokio::task::yield_now().await;
                Shutdown::Terminate
            };

            let end = run_op(
                &mut events,
                &ops_tx,
                &mut output,
                &mut signal,
                prompt(),
                Some(&mut Undecided),
            )
            .await;

            assert_eq!(end, OpEnd::Signal(Shutdown::Terminate));
            assert_eq!(sent(&mut ops), [prompt()]);
            assert_eq!(stderr.text(), "\n");
        }
    }

    mod output {
        use super::*;

        #[test]
        fn retains_the_first_failure_until_finish() {
            let (mut output, _, _) = output(OutputFormat::Text);

            output.record(Err(anyhow::anyhow!("first failure")));
            output.record(Err(anyhow::anyhow!("second failure")));
            output.record(Ok(()));

            assert_eq!(output.finish().unwrap_err().to_string(), "first failure");
            output.finish().unwrap();
        }

        mod render {
            use super::*;

            #[test]
            fn retains_early_write_and_flush_failures_after_later_output_succeeds() {
                for format in [OutputFormat::Text, OutputFormat::StreamJson] {
                    for failure in [Failure::Write("lost"), Failure::Flush] {
                        let stdout = FailingWriter::new(failure);
                        let screen = stdout.screen.clone();
                        let stderr = Screen::default();
                        let mut output = Output::new(format, false, stdout, stderr.clone());

                        for event in [
                            Event::StreamToken("lost".to_owned()),
                            Event::StreamToken("reply".to_owned()),
                            Event::Error("boom".to_owned()),
                            Event::TurnComplete,
                        ] {
                            output.render(event);
                        }
                        output.print_result(TurnEnd::Completed, "session-1");

                        assert_eq!(
                            output.finish().unwrap_err().to_string(),
                            failure.message(),
                            "{format:?}, {failure:?}",
                        );
                        if format == OutputFormat::Text {
                            let expected = match failure {
                                Failure::Write(_) => "reply\n",
                                Failure::Flush => "lostreply\n",
                            };
                            assert_eq!(screen.text(), expected, "{failure:?}");
                        } else {
                            let result = result_data(&screen.text());
                            assert_eq!(result["stop_reason"], "completed", "{failure:?}");
                            assert_eq!(result["message"], "lostreply", "{failure:?}");
                        }
                        assert_eq!(stderr.text(), "Error: boom\n", "{format:?}, {failure:?}");
                    }
                }
            }

            #[test]
            fn keeps_progress_notes_on_stderr_when_json_runs_show_thinking() {
                let (stdout, stderr) = (Screen::default(), Screen::default());
                let mut output =
                    Output::new(OutputFormat::Json, true, stdout.clone(), stderr.clone());

                output.render(Event::ThinkingToken("Hmm.".to_owned()));
                output.render(Event::ProgressUpdate("Checking the loader.".to_owned()));

                assert_eq!(
                    (stdout.text(), stderr.text()),
                    (String::new(), "Checking the loader.\n".to_owned()),
                );
            }

            #[test]
            fn renders_stderr_and_notes_the_lost_stdout_once_its_reader_is_gone() {
                let stderr = Screen::default();
                let mut output =
                    Output::new(OutputFormat::StreamJson, false, ClosedPipe, stderr.clone());
                assert!(!output.stdout_lost());

                output.render(Event::Error("boom".to_owned()));

                assert!(output.stdout_lost());
                assert_eq!(stderr.text(), "Error: boom\n");
            }
        }

        mod session_write_error {
            use super::*;

            #[test]
            fn prints_the_failure_and_reports_it_beside_the_completed_turn() {
                let (mut output, stdout, stderr) = output(OutputFormat::Json);
                output.render(Event::StreamToken("reply".to_owned()));

                output.session_write_error("disk full");
                output.print_result(TurnEnd::Completed, "session-1");

                output.finish().unwrap();
                assert_eq!(stderr.text(), "Error: Session write failed: disk full\n");
                let result = result_data(&stdout.text());
                assert_eq!(result["stop_reason"], "completed");
                assert_eq!(result["message"], "reply");
                assert_eq!(result["session_write_error"], "disk full");
            }
        }

        mod break_line {
            use super::*;

            #[test]
            fn ends_an_open_reply_line_once() {
                let (mut output, stdout, stderr) = output(OutputFormat::Text);
                output.render(Event::StreamToken("partial".to_owned()));

                output.break_line();
                output.print_result(TurnEnd::Shutdown, "session-1");

                output.finish().unwrap();
                assert_eq!(
                    (stdout.text(), stderr.text()),
                    ("partial\n".to_owned(), String::new()),
                );
            }

            #[test]
            fn keeps_the_line_break_off_stdout_in_the_json_formats() {
                for format in [OutputFormat::Json, OutputFormat::StreamJson] {
                    let (mut output, stdout, stderr) = output(format);
                    output.render(Event::StreamToken("partial".to_owned()));
                    let streamed = stdout.text();

                    output.break_line();

                    output.finish().unwrap();
                    assert_eq!(stdout.text(), streamed, "{format:?}");
                    assert_eq!(stderr.text(), "\n", "{format:?}");
                }
            }

            #[test]
            fn retains_a_failed_line_break() {
                let failure = Failure::Write("\n");
                let mut output = Output::new(
                    OutputFormat::Text,
                    false,
                    io::sink(),
                    FailingWriter::new(failure),
                );

                output.break_line();

                assert_eq!(output.finish().unwrap_err().to_string(), failure.message());
            }
        }

        mod print_result {
            use super::*;

            #[test]
            fn reports_the_reply_after_the_last_tool_call_and_the_latest_usage() {
                let usage = |tokens| UsageSnapshot {
                    context_tokens: Some(tokens),
                    context_window: Some(200_000),
                    estimated_cost_usd: Some(0.5),
                };
                let (mut output, stdout, _) = output(OutputFormat::Json);
                for event in [
                    Event::StreamToken("Let me ".to_owned()),
                    Event::StreamToken("look.".to_owned()),
                    Event::ToolCallStart(ls_call()),
                    Event::UsageUpdated(usage(1_000)),
                    Event::StreamToken("Found ".to_owned()),
                    Event::StreamToken("it.".to_owned()),
                    Event::UsageUpdated(usage(2_000)),
                    Event::TurnComplete,
                ] {
                    output.render(event);
                }

                output.print_result(TurnEnd::Completed, "session-1");

                output.finish().unwrap();
                let stdout = stdout.text();
                assert_eq!(stdout.lines().count(), 1, "{stdout}");
                assert_eq!(
                    result_data(&stdout),
                    serde_json::json!({
                        "stop_reason": "completed",
                        "error": null,
                        "session_write_error": null,
                        "message": "Found it.",
                        "usage": {
                            "context_tokens": 2_000,
                            "context_window": 200_000,
                            "estimated_cost_usd": 0.5,
                        },
                        "session_id": "session-1",
                    }),
                );
            }

            #[test]
            fn reports_the_fork_the_run_continued_in() {
                let (mut output, stdout, _) = output(OutputFormat::Json);
                output.render(Event::SessionForked {
                    id: "fork-session-id".to_owned(),
                    from: "session-1".to_owned(),
                });

                output.print_result(TurnEnd::Completed, "session-1");

                output.finish().unwrap();
                let stdout = stdout.text();
                assert_eq!(
                    stdout.lines().count(),
                    1,
                    "the notice stays off the wire: {stdout}"
                );
                assert_eq!(result_data(&stdout)["session_id"], "fork-session-id");
            }

            #[test]
            fn ends_the_open_reply_line_once_in_the_text_format() {
                let (mut output, stdout, stderr) = output(OutputFormat::Text);
                output.render(Event::StreamToken("partial reply".to_owned()));

                output.print_result(TurnEnd::Shutdown, "session-1");
                output.print_result(TurnEnd::Shutdown, "session-1");

                output.finish().unwrap();
                assert_eq!(
                    (stdout.text(), stderr.text()),
                    ("partial reply\n".to_owned(), String::new()),
                );
            }
        }

        mod record {
            use super::*;

            #[test]
            fn logs_the_full_failure_chain() {
                let logs = LogCapture::default();
                let (mut output, _, _) = output(OutputFormat::Text);
                let failure = anyhow::anyhow!("pipe closed").context("failed to render event");

                tracing::dispatcher::with_default(&logs.dispatch(), || {
                    output.record(Err(failure));
                });

                assert_eq!(
                    logs.events(),
                    [BTreeMap::from([
                        (
                            "error".to_owned(),
                            "failed to render event: pipe closed".to_owned()
                        ),
                        ("message".to_owned(), "stdio output failed".to_owned()),
                    ])]
                );
            }
        }

        mod report_failure {
            use super::*;

            #[test]
            fn names_the_failure_on_stderr() {
                let (mut output, _, stderr) = output(OutputFormat::Json);

                output.report_failure(&anyhow::anyhow!("pipe closed").context("write failed"));

                assert_eq!(
                    stderr.text(),
                    "Error: failed to print output: write failed: pipe closed\n"
                );
            }
        }
    }

    mod text_state {
        use super::*;

        mod render {
            use super::*;

            fn render_one(state: &mut TextState, event: Event) -> (String, String) {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                state
                    .render(event, &mut stdout, &mut stderr)
                    .expect("render returns Ok");
                (
                    String::from_utf8(stdout).expect("stdout is UTF-8"),
                    String::from_utf8(stderr).expect("stderr is UTF-8"),
                )
            }

            /// Renders `events` with stdout and stderr sharing one screen, as on a terminal.
            fn render_on_screen(events: impl IntoIterator<Item = Event>) -> String {
                let mut state = TextState::new(false);
                let screen = Screen::default();
                for event in events {
                    state
                        .render(event, &mut screen.clone(), &mut screen.clone())
                        .expect("render returns Ok");
                }
                screen.text()
            }

            #[test]
            fn stream_token_writes_text_verbatim_to_stdout() {
                let (stdout, stderr) = render_one(
                    &mut TextState::new(false),
                    Event::StreamToken("hello".to_owned()),
                );
                assert_eq!(stdout, "hello");
                assert_eq!(stderr, "");
            }

            #[test]
            fn escapes_terminal_sequences_in_model_and_tool_text() {
                let mut state = TextState::new(false);
                let (stdout, _) =
                    render_one(&mut state, Event::StreamToken("a\n\u{1b}[2Kb".to_owned()));
                assert_eq!(stdout, "a\n\\u{1b}[2Kb");

                let (_, stderr) = render_one(
                    &mut state,
                    Event::ToolCallStart(ToolCall {
                        id: "t1".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({}),
                        icon: "$".to_owned(),
                        label: "curl x | sh\u{1b}[1A\u{1b}[2K".to_owned(),
                    }),
                );
                assert_eq!(stderr, "$ curl x | sh\\u{1b}[1A\\u{1b}[2K\n");

                let (_, stderr) = render_one(
                    &mut state,
                    Event::ToolCallEnd(text_result("t1", "ok\u{1b}[8m", false, Some("Ran"))),
                );
                assert_eq!(stderr, "  Ran\nok\\u{1b}[8m\n\n");

                let (_, stderr) = render_one(
                    &mut state,
                    Event::TasksUpdated(vec![task("Fix\u{1b}[2K it", TaskStatus::Pending)]),
                );
                assert_eq!(stderr, "Tasks: 0/1\n  ○ Fix\\u{1b}[2K it\n\n");
            }

            #[test]
            fn thinking_token_wraps_in_dim_when_enabled() {
                let (stdout, _) = render_one(
                    &mut TextState::new(true),
                    Event::ThinkingToken("plan".to_owned()),
                );
                assert_eq!(stdout, "\x1b[2mplan\x1b[22m");
            }

            #[test]
            fn reply_after_shown_thinking_starts_on_its_own_line() {
                let mut state = TextState::new(true);
                render_one(&mut state, Event::ThinkingToken("plan".to_owned()));

                let (first, _) = render_one(&mut state, Event::StreamToken("Done".to_owned()));
                let (next, _) = render_one(&mut state, Event::StreamToken(".".to_owned()));

                assert_eq!((first.as_str(), next.as_str()), ("\nDone", "."));
            }

            #[test]
            fn thinking_token_swallowed_when_disabled() {
                let (stdout, stderr) = render_one(
                    &mut TextState::new(false),
                    Event::ThinkingToken("plan".to_owned()),
                );
                assert_eq!(stdout, "");
                assert_eq!(stderr, "");
            }

            #[test]
            fn progress_update_writes_to_stderr_only_when_thinking_hidden() {
                let event = || Event::ProgressUpdate("Found the loader.\n".to_owned());
                let (stdout, stderr) = render_one(&mut TextState::new(false), event());
                assert_eq!(stdout, "");
                assert_eq!(stderr, "Found the loader.\n");

                // The same text already streamed as dim thinking on stdout.
                assert_eq!(
                    render_one(&mut TextState::new(true), event()),
                    (String::new(), String::new()),
                );
            }

            #[test]
            fn tool_call_start_writes_icon_label_to_stderr() {
                let (stdout, stderr) =
                    render_one(&mut TextState::new(false), Event::ToolCallStart(ls_call()));
                assert_eq!(stdout, "");
                assert_eq!(stderr, "$ ls\n");
            }

            #[test]
            fn tool_call_start_ends_the_reply_line_before_its_header() {
                let screen = render_on_screen([
                    Event::StreamToken("Let me look.".to_owned()),
                    Event::ToolCallStart(ls_call()),
                ]);

                assert_eq!(screen, "Let me look.\n$ ls\n");
            }

            #[test]
            fn tool_call_end_with_title_writes_title_then_content() {
                let (_, stderr) = render_one(
                    &mut TextState::new(false),
                    Event::ToolCallEnd(text_result("t1", "file1\nfile2\n", false, Some("Listed"))),
                );
                assert_eq!(
                    stderr,
                    indoc! {"
                          Listed
                        file1
                        file2

                    "}
                );
            }

            #[test]
            fn tool_call_end_after_another_call_started_repeats_its_header() {
                let mut state = TextState::new(false);
                let start = |id: &str| {
                    Event::ToolCallStart(ToolCall {
                        id: id.to_owned(),
                        name: "grep".to_owned(),
                        input: serde_json::json!({}),
                        icon: "*".to_owned(),
                        label: format!("Grep({id})"),
                    })
                };

                let rendered = [
                    start("a"),
                    start("b"),
                    Event::ToolCallEnd(text_result("a", "a hits", false, None)),
                    Event::ToolCallEnd(text_result("b", "b hits", false, None)),
                    start("c"),
                    Event::ToolCallEnd(text_result("c", "c hits", false, None)),
                ]
                .map(|event| render_one(&mut state, event).1);

                assert_eq!(
                    rendered,
                    [
                        "* Grep(a)\n",
                        "* Grep(b)\n",
                        "* Grep(a)\na hits\n\n",
                        "* Grep(b)\nb hits\n\n",
                        "* Grep(c)\n",
                        "c hits\n\n",
                    ],
                );
            }

            #[test]
            fn tool_call_end_without_title_skips_header_and_whitespace_content() {
                let (_, stderr) = render_one(
                    &mut TextState::new(false),
                    Event::ToolCallEnd(text_result("t2", "   \n", true, None)),
                );
                assert_eq!(stderr, "\n");
            }

            #[test]
            fn tasks_updated_marks_each_task_under_its_progress_and_skips_a_cleared_list() {
                let mut state = TextState::new(false);

                let (stdout, stderr) = render_one(&mut state, Event::TasksUpdated(plan()));
                assert_eq!(stdout, "");
                assert_eq!(
                    stderr,
                    [
                        "Tasks: 1/3\n",
                        "  ✓ Read the config loader\n",
                        "  ● Add the entry type\n",
                        "  ○ Write the tests\n",
                        "\n",
                    ]
                    .concat(),
                );

                let (stdout, stderr) = render_one(&mut state, Event::TasksUpdated(Vec::new()));
                assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));
            }

            #[test]
            fn auto_compaction_started_explains_the_pause() {
                let (stdout, stderr) =
                    render_one(&mut TextState::new(false), Event::AutoCompactionStarted);

                assert_eq!(stdout, "");
                assert_eq!(stderr, "Compacting the conversation to free up context\n");
            }

            #[test]
            fn session_compacted_writes_the_boundary_with_its_focus_escaped() {
                let (stdout, stderr) = render_one(
                    &mut TextState::new(false),
                    Event::SessionCompacted {
                        summary: "summary".to_owned(),
                        pre_count: 4,
                        instructions: Some("login\u{1b}[2J".to_owned()),
                        trigger: CompactTrigger::Manual,
                    },
                );

                assert_eq!(stdout, "");
                assert_eq!(
                    stderr,
                    "Compacted 4 messages into a summary (focus: login\\u{1b}[2J)\n"
                );
            }

            #[test]
            fn session_forked_ends_the_reply_line_before_its_notice() {
                let screen = render_on_screen([
                    Event::StreamToken("Working".to_owned()),
                    Event::SessionForked {
                        id: "fork-session-id".to_owned(),
                        from: "source-session-id".to_owned(),
                    },
                ]);

                assert_eq!(
                    screen,
                    "Working\nAnother ox process wrote to session source-s, so this conversation \
                     continues as session fork-ses\n"
                );
            }

            #[test]
            fn session_confirmations_write_to_stderr() {
                let cases = [
                    (
                        Event::SessionTitleUpdated {
                            id: "sid".to_owned(),
                            title: "Fix login\u{1b}[2J".to_owned(),
                        },
                        "Renamed the session to Fix login\\u{1b}[2J\n",
                    ),
                    (
                        Event::SessionRolled {
                            id: "0123456789abcdef".to_owned(),
                            old_session: ArchiveState::Active,
                        },
                        "Started session 01234567\n",
                    ),
                    (
                        Event::SessionResumed {
                            id: "89abcdef01234567".to_owned(),
                            title: Some("Fix login".to_owned()),
                            prior: ArchiveState::Active,
                            history: Vec::new(),
                        },
                        "Resumed session 89abcdef: Fix login\n",
                    ),
                    (
                        Event::SessionResumed {
                            id: "89abcdef01234567".to_owned(),
                            title: None,
                            prior: ArchiveState::Archived,
                            history: Vec::new(),
                        },
                        "Resumed session 89abcdef: (untitled)\nUnarchived session 89abcdef\n",
                    ),
                ];

                for (event, expected) in cases {
                    let (stdout, stderr) = render_one(&mut TextState::new(false), event);

                    assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
                    assert_eq!(stderr, expected);
                }
            }

            #[test]
            fn config_changed_writes_its_notice_to_stderr() {
                let event = Event::ConfigChanged {
                    model: ModelSpec::resolve("claude-opus-5-5"),
                    effort: Some(Effort::Low),
                    requested: ConfigPick::Effort(Effort::Low),
                    auto_compaction: AutoCompactionConfig::disabled(),
                    notice: "Effort set to low".to_owned(),
                };

                let (stdout, stderr) = render_one(&mut TextState::new(false), event);

                assert_eq!(stdout, "");
                assert_eq!(stderr, "Effort set to low\n");
            }

            #[test]
            fn mode_changed_names_the_mode_in_use() {
                let (stdout, stderr) =
                    render_one(&mut TextState::new(false), Event::ModeChanged(Mode::Plan));

                assert_eq!(stdout, "");
                assert_eq!(stderr, "Switched to plan mode\n");
            }

            #[test]
            fn tui_only_events_emit_nothing_on_either_stream() {
                for event in [
                    Event::PromptDrained("queued".to_owned()),
                    Event::RateLimitsUpdated(RateLimits {
                        five_hour: None,
                        seven_day: Some(RateLimitWindow {
                            utilization: 0.5,
                            resets_at: 1_760_000_000,
                        }),
                    }),
                    Event::PromptUnsent,
                    Event::AutoCompactionPaused { failures: 3 },
                    Event::AutoCompactionResumed,
                    Event::SummaryToken("Recap".to_owned()),
                    Event::SummaryDiscarded,
                    Event::ArchiveUpdated {
                        target: SavedTarget::Prefix("saved".to_owned()),
                        state: ArchiveState::Archived,
                        result: Err(SavedError::Failed {
                            action: SavedAction::Archive,
                            id: "saved".to_owned(),
                            cause: "disk unavailable".to_owned(),
                        }),
                    },
                    Event::SavedFound(Err(SavedError::Live {
                        id: "live-ses".to_owned(),
                    })),
                    Event::SavedDeleted {
                        id: "saved".to_owned(),
                        result: Ok(()),
                    },
                    Event::BackgroundShellsUpdated(shells_in_each_state()),
                ] {
                    let (stdout, stderr) = render_one(&mut TextState::new(false), event);
                    assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
                    assert!(stderr.is_empty(), "stderr must stay empty: {stderr:?}");
                }
            }

            #[test]
            fn turn_complete_ends_the_open_reply_line_once() {
                let mut state = TextState::new(true);
                render_one(&mut state, Event::ThinkingToken("plan".to_owned()));

                let ended = render_one(&mut state, Event::TurnComplete);
                let again = render_one(&mut state, Event::TurnComplete);

                assert_eq!(ended, ("\n".to_owned(), String::new()));
                assert_eq!(again, (String::new(), String::new()));
            }

            #[test]
            fn turn_complete_leaves_a_closed_reply_line_alone() {
                let mut state = TextState::new(false);
                render_one(&mut state, Event::StreamToken("Done.\n".to_owned()));
                render_one(&mut state, Event::StreamToken(String::new()));

                assert_eq!(
                    render_one(&mut state, Event::TurnComplete),
                    (String::new(), String::new()),
                );
            }

            #[test]
            fn cancelled_ends_the_reply_line_before_the_marker() {
                let screen =
                    render_on_screen([Event::StreamToken("partial".to_owned()), Event::Cancelled]);

                assert_eq!(screen, format!("partial\n{INTERRUPTED_MARKER}\n"));
            }

            #[test]
            fn retrying_and_notices_write_the_text_to_stderr() {
                let retry = "HTTP 529 overloaded_error. Retrying in 1 s (retry 1 of 5).";
                let notice = "Skipped instruction file /repo/CLAUDE.md: Permission denied";

                let (retry_stdout, retry_stderr) = render_one(
                    &mut TextState::new(false),
                    Event::Retrying(retry.to_owned()),
                );
                let (notice_stdout, notice_stderr) =
                    render_one(&mut TextState::new(false), Event::Notice(notice.to_owned()));

                assert_eq!(retry_stdout, "");
                assert_eq!(retry_stderr, format!("{retry}\n"));
                assert_eq!(notice_stdout, "");
                assert_eq!(notice_stderr, format!("{notice}\n"));
            }

            #[test]
            fn turn_failed_ends_the_reply_line_before_the_error() {
                let screen = render_on_screen([
                    Event::StreamToken("partial reply".to_owned()),
                    Event::TurnFailed("boom".to_owned()),
                ]);

                assert_eq!(screen, "partial reply\nError: boom\n");
            }

            #[test]
            fn turn_failed_still_writes_stderr_when_stdout_is_closed() {
                let mut state = TextState::new(false);
                render_one(&mut state, Event::StreamToken("partial reply".to_owned()));
                let mut stderr = Vec::new();

                let rendered = state.render(
                    Event::TurnFailed("boom".to_owned()),
                    &mut ClosedPipe,
                    &mut stderr,
                );

                assert_eq!(
                    rendered
                        .unwrap_err()
                        .downcast_ref::<io::Error>()
                        .map(io::Error::kind),
                    Some(io::ErrorKind::BrokenPipe),
                );
                assert_eq!(stderr, b"Error: boom\n");
            }

            #[test]
            fn errors_prefix_with_error_label() {
                let (_, stderr) =
                    render_one(&mut TextState::new(false), Event::Error("boom".to_owned()));
                assert_eq!(stderr, "Error: boom\n");
                let (_, stderr) = render_one(
                    &mut TextState::new(false),
                    Event::RenameFailed("lost".to_owned()),
                );
                assert_eq!(stderr, "Error: lost\n");
            }
        }

        mod break_line {
            use super::*;

            #[test]
            fn ends_the_open_reply_line_past_the_echoed_signal() {
                let mut state = TextState::new(false);
                let screen = Screen::default();
                state
                    .render(
                        Event::StreamToken("partial".to_owned()),
                        &mut screen.clone(),
                        &mut screen.clone(),
                    )
                    .expect("render returns Ok");

                state
                    .break_line(&mut screen.clone(), &mut screen.clone())
                    .expect("break_line returns Ok");
                state
                    .render(Event::Cancelled, &mut screen.clone(), &mut screen.clone())
                    .expect("render returns Ok");

                assert_eq!(screen.text(), format!("partial\n{INTERRUPTED_MARKER}\n"));
            }

            #[test]
            fn writes_a_newline_to_stderr_when_no_reply_line_is_open() {
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

                TextState::new(false)
                    .break_line(&mut stdout, &mut stderr)
                    .expect("break_line returns Ok");

                assert_eq!((stdout, stderr), (Vec::new(), b"\n".to_vec()));
            }
        }
    }
}
