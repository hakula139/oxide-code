//! Stdio rendering of engine events for the `--no-tui` REPL and `--print` headless modes.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::debug;

use ox_core::engine::TurnEnd;
use ox_core::permission::defaults::ShippedTier;
use ox_core::permission::{Basis, SessionGrant};
use ox_core::protocol::{Decision, Event, EventSink, Op, ToolCall, ToolResult, notice};
use ox_core::session::display::{UNTITLED_MARKER, archive_notice, id_prefix};
use ox_core::session::entry::ArchiveState;
use ox_core::tool::tasks::{Progress, Task};
use ox_core::tool::view::ApprovalPreview;
use ox_core::util::sync::lock_or_recover;
use ox_core::util::text::{escape_controls, escape_line};
use ox_tui::glyphs::{INTERRUPTED_MARKER, task_marker};

// ── Turn Rendering ──

/// Renders `events` on `sink` until `op`, already sent, ends, returning how it ended, or `None`
/// once the engine has stopped. Approval requests go to `approver` in place of `sink`, and its
/// answer goes back on `ops`. Events that arrive while `approver` decides wait until it answers,
/// unless one ends `op`, such as the cancellation an interrupt causes, which drops the question.
pub(super) async fn render_op(
    events: &mut mpsc::UnboundedReceiver<Event>,
    sink: &dyn EventSink,
    ops: &mpsc::Sender<Op>,
    op: &Op,
    approver: &mut impl Approver,
) -> Option<TurnEnd> {
    let mut held = VecDeque::new();
    loop {
        let event = match held.pop_front() {
            Some(event) => event,
            None => events.recv().await?,
        };
        let Event::ApprovalRequested {
            id,
            preview,
            basis,
            grant,
        } = event
        else {
            let end = TurnEnd::of(op, &event);
            render(sink, event);
            if end.is_some() {
                return end;
            }
            continue;
        };

        let mut decision = pin!(approver.decide(&preview, &basis, grant.as_deref()));
        let decision = loop {
            tokio::select! {
                biased;

                decision = &mut decision => break Some(decision),
                event = events.recv() => {
                    let event = event?;
                    let ends = TurnEnd::of(op, &event).is_some();
                    held.push_back(event);
                    if ends {
                        break None;
                    }
                }
            }
        };
        if let Some(decision) = decision {
            // A send fails only once the engine has stopped, which the next `recv` reports.
            _ = ops.send(Op::Resolve { id, decision }).await;
        }
    }
}

/// How a stdio frontend answers an approval request.
pub(super) trait Approver {
    /// Decides the call `preview` shows, which `basis` made ask and `grant` may allow for the rest
    /// of the session.
    fn decide(
        &mut self,
        preview: &ApprovalPreview,
        basis: &Basis,
        grant: Option<&SessionGrant>,
    ) -> impl Future<Output = Decision>;
}

/// Renders the events already queued, such as the engine's startup warnings.
pub(super) fn render_queued(events: &mut mpsc::UnboundedReceiver<Event>, sink: &dyn EventSink) {
    while let Ok(event) = events.try_recv() {
        render(sink, event);
    }
}

fn render(sink: &dyn EventSink, event: Event) {
    if let Err(e) = sink.send(event) {
        debug!("stdio render failed: {e}");
    }
}

// ── Unattended ──

/// Answers every approval with a denial that names what a rerun would need, for a run with nobody
/// to ask: the allow pattern that admits the call, or the rule that holds it.
pub(super) struct Unattended;

impl Approver for Unattended {
    fn decide(
        &mut self,
        _: &ApprovalPreview,
        basis: &Basis,
        grant: Option<&SessionGrant>,
    ) -> impl Future<Output = Decision> {
        let feedback = match (grant, basis) {
            (Some(grant), _) => {
                let rule = grant.suggested();
                format!(
                    "This run cannot ask for approval. \
                     To allow this call, add `{}` to `permission.allow.{}` in the user config.",
                    toml::Value::from(rule.pattern()),
                    rule.class(),
                )
            }
            (None, Basis::Builtin(group)) if group.tier() == ShippedTier::Ask => format!(
                "This run cannot ask for approval, and {basis} holds this call. \
                 Approve it in an interactive session, \
                 or set `permission.defaults.{group} = false` in the user config."
            ),
            (None, Basis::Mode(_)) => {
                "This run cannot ask for approval, and no allow pattern matches this call. \
                 Under `permission.allow` in the user config, \
                 a chained command needs a pattern per command, \
                 and a word that expands needs a pattern ending in `*` at that word. \
                 A command that hides what it runs, \
                 such as `$(...)` or `sh -c`, needs approval in an interactive session."
                    .to_owned()
            }
            (None, basis) => format!(
                "This run cannot ask for approval, and {basis} holds this call. \
                 Approve it in an interactive session."
            ),
        };
        std::future::ready(Decision::DenyWithFeedback { feedback })
    }
}

// ── StdioSink ──

/// Starts dim text, the style used for thinking.
const DIM: &str = "\x1b[2m";
/// Restores normal intensity.
const NORMAL_INTENSITY: &str = "\x1b[22m";

/// [`EventSink`] for the `--no-tui` REPL: assistant tokens stream to stdout and everything else to
/// stderr, so piped output stays clean. Headless runs reuse [`Self::render`] for their text output.
/// Model and tool text passes through [`escape_controls`], so it cannot redraw the terminal around
/// a REPL approval prompt. An open reply line on stdout ends before a tool call starts, before the
/// reply that follows shown thinking, and when the operation ends.
pub(super) struct StdioSink {
    show_thinking: bool,
    calls: Mutex<StartedCalls>,
    /// Whether the last text written to stdout left the current line open.
    line_open: AtomicBool,
    /// Whether the last text written to stdout was thinking.
    thinking: AtomicBool,
}

/// The started calls awaiting a result, so a result printed after another call's header can repeat
/// its own.
#[derive(Default)]
struct StartedCalls {
    headers: HashMap<String, String>,
    last: Option<String>,
}

impl StdioSink {
    pub(super) fn new(show_thinking: bool) -> Self {
        Self {
            show_thinking,
            calls: Mutex::default(),
            line_open: AtomicBool::new(false),
            thinking: AtomicBool::new(false),
        }
    }

    /// Writes `event` to `stdout` or `stderr`.
    pub(super) fn render<W1: Write, W2: Write>(
        &self,
        event: Event,
        stdout: &mut W1,
        stderr: &mut W2,
    ) -> Result<()> {
        match event {
            Event::StreamToken(text) => {
                if self.thinking.swap(false, Ordering::Relaxed) {
                    self.end_line(stdout)?;
                }
                self.write_reply(stdout, &escape_controls(&text), "", "")?;
            }
            Event::ThinkingToken(text) => {
                if self.show_thinking {
                    let text = escape_controls(&text);
                    self.write_reply(stdout, &text, DIM, NORMAL_INTENSITY)?;
                    self.thinking.store(true, Ordering::Relaxed);
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
                self.calls().start(id, header);
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
                id,
                title,
                unarchived,
                ..
            } => {
                let notice = resumed_notice(&id, title.as_deref(), unarchived);
                writeln!(stderr, "{notice}")?;
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

    /// Ends the reply line left open on `stdout`, if any.
    pub(super) fn end_line(&self, stdout: &mut impl Write) -> io::Result<()> {
        if self.line_open.swap(false, Ordering::Relaxed) {
            writeln!(stdout)?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// Moves to a new line after a signal, past the `^C` a terminal echoes, by ending the open
    /// reply line on `stdout` or else writing a newline to `stderr`.
    pub(super) fn break_line(
        &self,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<()> {
        if self.line_open.load(Ordering::Relaxed) {
            return self.end_line(stdout);
        }
        writeln!(stderr)?;
        stderr.flush()
    }

    /// Writes reply or thinking `text` to `stdout` between `start` and `end` styles, noting
    /// whether it leaves the line open.
    fn write_reply(
        &self,
        stdout: &mut impl Write,
        text: &str,
        start: &str,
        end: &str,
    ) -> io::Result<()> {
        write!(stdout, "{start}{text}{end}")?;
        stdout.flush()?;
        if !text.is_empty() {
            self.line_open
                .store(!text.ends_with('\n'), Ordering::Relaxed);
        }
        Ok(())
    }

    /// Ends the open reply line, then writes `line` to `stderr`, which it does even when ending the
    /// reply line failed.
    fn end_reply_then(
        &self,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
        line: &str,
    ) -> io::Result<()> {
        let ended = self.end_line(stdout);
        writeln!(stderr, "{line}")?;
        ended
    }

    fn calls(&self) -> MutexGuard<'_, StartedCalls> {
        lock_or_recover(&self.calls, "started tool calls")
    }

    /// Writes a call's result under its title, after its call's header again when another call's
    /// header came after it.
    fn render_result(&self, result: ToolResult, stderr: &mut impl Write) -> Result<()> {
        let ToolResult {
            id, content, title, ..
        } = result;
        if let Some(header) = self.calls().end(&id) {
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

impl EventSink for StdioSink {
    fn send(&self, event: Event) -> Result<()> {
        self.render(event, &mut io::stdout().lock(), &mut io::stderr().lock())
    }
}

/// A progress header, one marked line per task, and a closing blank line, or nothing for a
/// cleared list.
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
fn resumed_notice(id: &str, title: Option<&str>, unarchived: bool) -> String {
    let title = title.map_or_else(|| UNTITLED_MARKER.to_owned(), escape_line);
    let resumed = format!("Resumed session {}: {title}", id_prefix(id));
    if unarchived {
        return format!("{resumed}\n{}", archive_notice(id, ArchiveState::Active));
    }
    resumed
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use wiremock::MockServer;

    use ox_core::client::{RateLimitWindow, RateLimits};
    use ox_core::config::Effort;
    use ox_core::config::compaction::AutoCompactionConfig;
    use ox_core::engine::testing::{engine, with_pinned_home};
    use ox_core::model::ModelSpec;
    use ox_core::permission::defaults::Shipped;
    use ox_core::permission::rule::Rule;
    use ox_core::permission::testing::exec_grant;
    use ox_core::permission::{Mode, PermissionClass, Tier};
    use ox_core::protocol::testing::{CapturingSink, FailingSink, ls_call, text_result};
    use ox_core::protocol::{CompactTrigger, ConfigPick};
    use ox_core::session::store::testing::test_store;
    use ox_core::tool::bash::background::testing::shells_in_each_state;
    use ox_core::tool::tasks::TaskStatus;
    use ox_core::tool::tasks::testing::{plan, task};
    use ox_core::tool::testing::bash_preview;

    use super::*;
    use crate::driver::testing::{ClosedPipe, Screen};

    mod render_op {
        use super::*;

        /// Answers every approval with its decision, recording what each request offered.
        struct Fixed {
            decision: Decision,
            offered: Vec<(String, Option<String>)>,
        }

        impl Approver for Fixed {
            fn decide(
                &mut self,
                preview: &ApprovalPreview,
                _: &Basis,
                grant: Option<&SessionGrant>,
            ) -> impl Future<Output = Decision> {
                let rule = grant.map(|grant| grant.suggested().pattern().to_owned());
                self.offered.push((preview.title.clone(), rule));
                std::future::ready(self.decision.clone())
            }
        }

        fn fixed(decision: Decision) -> Fixed {
            Fixed {
                decision,
                offered: Vec::new(),
            }
        }

        fn prompt() -> Op {
            Op::Prompt("hi".to_owned())
        }

        #[tokio::test]
        async fn answers_each_approval_on_ops() {
            let (tx, mut events) = mpsc::unbounded_channel();
            let approval = Event::ApprovalRequested {
                id: "t1".to_owned(),
                preview: bash_preview("ls"),
                basis: Basis::Mode(Mode::Manual),
                grant: Some(Box::new(exec_grant("ls"))),
            };
            for event in [approval, Event::TurnComplete] {
                tx.send(event).unwrap();
            }
            let (ops_tx, mut ops) = mpsc::channel(1);
            let sink = CapturingSink::default();
            let mut approver = fixed(Decision::Deny);

            let end = render_op(&mut events, &sink, &ops_tx, &prompt(), &mut approver).await;

            assert_eq!(end, Some(TurnEnd::Completed));
            assert_eq!(
                approver.offered,
                [("Bash".to_owned(), Some("ls".to_owned()))],
            );
            assert_eq!(
                ops.try_recv().ok(),
                Some(Op::Resolve {
                    id: "t1".to_owned(),
                    decision: Decision::Deny,
                }),
            );
            assert_eq!(sink.events(), [Event::TurnComplete]);
        }

        #[tokio::test]
        async fn drops_an_unanswered_approval_once_the_op_ends() {
            /// Never answers, as a person who has not typed an answer yet.
            struct Undecided;

            impl Approver for Undecided {
                fn decide(
                    &mut self,
                    _: &ApprovalPreview,
                    _: &Basis,
                    _: Option<&SessionGrant>,
                ) -> impl Future<Output = Decision> {
                    std::future::pending()
                }
            }

            let (tx, mut events) = mpsc::unbounded_channel();
            for event in [
                Event::ApprovalRequested {
                    id: "t1".to_owned(),
                    preview: bash_preview("ls"),
                    basis: Basis::Mode(Mode::Manual),
                    grant: None,
                },
                Event::ModeChanged(Mode::Plan),
                Event::Cancelled,
            ] {
                tx.send(event).unwrap();
            }
            let (ops_tx, mut ops) = mpsc::channel(1);
            let sink = CapturingSink::default();

            let end = render_op(&mut events, &sink, &ops_tx, &prompt(), &mut Undecided).await;

            assert_eq!(end, Some(TurnEnd::Cancelled));
            assert_eq!(
                sink.events(),
                [Event::ModeChanged(Mode::Plan), Event::Cancelled],
            );
            assert_eq!(ops.try_recv().ok(), None);
        }

        #[tokio::test]
        async fn holds_events_that_arrive_during_an_approval_until_it_is_answered() {
            /// Answers once the events sent with the request have arrived, recording what had
            /// rendered by then.
            struct Watching<'a> {
                sink: &'a CapturingSink,
                rendered: Option<Vec<Event>>,
            }

            impl Approver for Watching<'_> {
                async fn decide(
                    &mut self,
                    _: &ApprovalPreview,
                    _: &Basis,
                    _: Option<&SessionGrant>,
                ) -> Decision {
                    tokio::task::yield_now().await;
                    self.rendered = Some(self.sink.events());
                    Decision::Deny
                }
            }

            let (tx, mut events) = mpsc::unbounded_channel();
            for event in [
                Event::ApprovalRequested {
                    id: "t1".to_owned(),
                    preview: bash_preview("ls"),
                    basis: Basis::Mode(Mode::Manual),
                    grant: None,
                },
                Event::ModeChanged(Mode::Plan),
                Event::TurnComplete,
            ] {
                tx.send(event).unwrap();
            }
            let (ops, _ops_rx) = mpsc::channel(1);
            let sink = CapturingSink::default();
            let mut approver = Watching {
                sink: &sink,
                rendered: None,
            };

            let end = render_op(&mut events, &sink, &ops, &prompt(), &mut approver).await;

            assert_eq!(end, Some(TurnEnd::Completed));
            assert_eq!(approver.rendered, Some(Vec::new()));
            assert_eq!(
                sink.events(),
                [Event::ModeChanged(Mode::Plan), Event::TurnComplete],
            );
        }

        #[tokio::test]
        async fn stops_at_the_terminal_event_even_when_rendering_fails() {
            let (tx, mut events) = mpsc::unbounded_channel();
            for event in [
                Event::StreamToken("reply".to_owned()),
                Event::Cancelled,
                Event::TurnComplete,
            ] {
                tx.send(event).unwrap();
            }

            let (ops, _) = mpsc::channel(1);
            let mut approver = fixed(Decision::Deny);
            let end = render_op(&mut events, &FailingSink, &ops, &prompt(), &mut approver).await;

            assert_eq!(end, Some(TurnEnd::Cancelled));
            assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
        }

        #[tokio::test]
        async fn waits_for_the_terminal_event_of_its_own_op() {
            let (tx, mut events) = mpsc::unbounded_channel();
            let cleared = [
                Event::SessionRolled {
                    id: "next".to_owned(),
                    archived: false,
                },
                Event::Error("Previous session write failed: disk full".to_owned()),
                Event::Notice(
                    "Skipped instruction file /repo/CLAUDE.md: permission denied".to_owned(),
                ),
                Event::SessionReady,
            ];
            for event in cleared.iter().cloned().chain([Event::TurnComplete]) {
                tx.send(event).unwrap();
            }
            drop(tx);
            let sink = CapturingSink::default();

            let (ops, _) = mpsc::channel(1);
            let mut approver = fixed(Decision::Deny);
            let end = render_op(&mut events, &sink, &ops, &Op::Clear, &mut approver).await;

            assert_eq!(end, Some(TurnEnd::Completed));
            assert_eq!(sink.events(), cleared);
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
                handle.ops.send(Op::Clear).await.unwrap();
                let sink = CapturingSink::default();
                let mut approver = fixed(Decision::Deny);

                let end = render_op(
                    &mut handle.events,
                    &sink,
                    &handle.ops,
                    &Op::Clear,
                    &mut approver,
                )
                .await;

                assert_eq!(end, Some(TurnEnd::Completed));
                let warning = format!(
                    "Skipped instruction file {}: stream did not contain valid UTF-8",
                    instructions.display(),
                );
                assert!(
                    matches!(
                        sink.events().as_slice(),
                        [
                            Event::SessionRolled { .. },
                            Event::Notice(notice),
                            Event::SessionReady,
                        ] if *notice == warning
                    ),
                    "{:?}",
                    sink.events(),
                );
                handle.shutdown().await.unwrap();
            })
            .await;
        }

        #[tokio::test]
        async fn reports_a_stopped_engine() {
            let (tx, mut events) = mpsc::unbounded_channel();
            tx.send(Event::StreamToken("partial".to_owned())).unwrap();
            drop(tx);
            let sink = CapturingSink::default();

            let (ops, _) = mpsc::channel(1);
            let mut approver = fixed(Decision::Deny);
            let end = render_op(&mut events, &sink, &ops, &prompt(), &mut approver).await;

            assert_eq!(end, None);
            assert_eq!(sink.events(), [Event::StreamToken("partial".to_owned())]);
        }
    }

    mod unattended {
        use super::*;

        mod decide {
            use super::*;

            #[tokio::test]
            async fn denies_naming_the_allow_pattern_or_what_holds_the_call() {
                let command = r#"echo "a\b""#;
                let preview = bash_preview(command);
                let manual = Basis::Mode(Mode::Manual);
                let grant = exec_grant(command);
                let ask = Basis::Rule {
                    tier: Tier::Ask,
                    rule: Rule::parse(PermissionClass::Exec, "git push *").unwrap(),
                };
                let cases = [
                    (
                        &manual,
                        Some(&grant),
                        "This run cannot ask for approval. \
                         To allow this call, \
                         add `'''echo 'a\\b''''` to `permission.allow.exec` in the user config.",
                    ),
                    (
                        &manual,
                        None,
                        "This run cannot ask for approval, and no allow pattern matches this call. \
                         Under `permission.allow` in the user config, \
                         a chained command needs a pattern per command, \
                         and a word that expands needs a pattern ending in `*` at that word. \
                         A command that hides what it runs, \
                         such as `$(...)` or `sh -c`, needs approval in an interactive session.",
                    ),
                    (
                        &Basis::Builtin(Shipped::Git),
                        None,
                        "This run cannot ask for approval, \
                         and the built-in `git` rule holds this call. \
                         Approve it in an interactive session, \
                         or set `permission.defaults.git = false` in the user config.",
                    ),
                    (
                        &Basis::Builtin(Shipped::Ssh),
                        None,
                        "This run cannot ask for approval, \
                         and the built-in `ssh` rule holds this call. \
                         Approve it in an interactive session.",
                    ),
                    (
                        &ask,
                        None,
                        "This run cannot ask for approval, \
                         and the `ask.exec` pattern `git push *` holds this call. \
                         Approve it in an interactive session.",
                    ),
                ];

                for (basis, grant, expected) in cases {
                    let decision = Unattended.decide(&preview, basis, grant).await;

                    assert_eq!(
                        decision,
                        Decision::DenyWithFeedback {
                            feedback: expected.to_owned()
                        },
                        "{basis:?}"
                    );
                }
            }
        }
    }

    mod stdio_sink {
        use super::*;

        mod render {
            use super::*;

            fn test_sink(show_thinking: bool) -> StdioSink {
                StdioSink::new(show_thinking)
            }

            fn render_one(sink: &StdioSink, event: Event) -> (String, String) {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                sink.render(event, &mut stdout, &mut stderr)
                    .expect("render returns Ok");
                (
                    String::from_utf8(stdout).expect("stdout is UTF-8"),
                    String::from_utf8(stderr).expect("stderr is UTF-8"),
                )
            }

            /// Renders `events` with stdout and stderr sharing one screen, as on a terminal.
            fn render_on_screen(events: impl IntoIterator<Item = Event>) -> String {
                let sink = test_sink(false);
                let screen = Screen::default();
                for event in events {
                    sink.render(event, &mut screen.clone(), &mut screen.clone())
                        .expect("render returns Ok");
                }
                screen.text()
            }

            #[test]
            fn stream_token_writes_text_verbatim_to_stdout() {
                let (stdout, stderr) =
                    render_one(&test_sink(false), Event::StreamToken("hello".to_owned()));
                assert_eq!(stdout, "hello");
                assert_eq!(stderr, "");
            }

            #[test]
            fn escapes_terminal_sequences_in_model_and_tool_text() {
                let sink = test_sink(false);
                let (stdout, _) = render_one(&sink, Event::StreamToken("a\n\u{1b}[2Kb".to_owned()));
                assert_eq!(stdout, "a\n\\u{1b}[2Kb");

                let (_, stderr) = render_one(
                    &sink,
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
                    &sink,
                    Event::ToolCallEnd(text_result("t1", "ok\u{1b}[8m", false, Some("Ran"))),
                );
                assert_eq!(stderr, "  Ran\nok\\u{1b}[8m\n\n");

                let (_, stderr) = render_one(
                    &sink,
                    Event::TasksUpdated(vec![task("Fix\u{1b}[2K it", TaskStatus::Pending)]),
                );
                assert_eq!(stderr, "Tasks: 0/1\n  ○ Fix\\u{1b}[2K it\n\n");
            }

            #[test]
            fn thinking_token_wraps_in_dim_when_enabled() {
                let (stdout, _) =
                    render_one(&test_sink(true), Event::ThinkingToken("plan".to_owned()));
                assert_eq!(stdout, "\x1b[2mplan\x1b[22m");
            }

            #[test]
            fn reply_after_shown_thinking_starts_on_its_own_line() {
                let sink = test_sink(true);
                render_one(&sink, Event::ThinkingToken("plan".to_owned()));

                let (first, _) = render_one(&sink, Event::StreamToken("Done".to_owned()));
                let (next, _) = render_one(&sink, Event::StreamToken(".".to_owned()));

                assert_eq!((first.as_str(), next.as_str()), ("\nDone", "."));
            }

            #[test]
            fn thinking_token_swallowed_when_disabled() {
                let (stdout, stderr) =
                    render_one(&test_sink(false), Event::ThinkingToken("plan".to_owned()));
                assert_eq!(stdout, "");
                assert_eq!(stderr, "");
            }

            #[test]
            fn progress_update_writes_to_stderr_only_when_thinking_hidden() {
                let event = || Event::ProgressUpdate("Found the loader.\n".to_owned());
                let (stdout, stderr) = render_one(&test_sink(false), event());
                assert_eq!(stdout, "");
                assert_eq!(stderr, "Found the loader.\n");

                // The same text already streamed as dim thinking on stdout.
                assert_eq!(
                    render_one(&test_sink(true), event()),
                    (String::new(), String::new()),
                );
            }

            #[test]
            fn tool_call_start_writes_icon_label_to_stderr() {
                let (stdout, stderr) =
                    render_one(&test_sink(false), Event::ToolCallStart(ls_call()));
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
                    &test_sink(false),
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
                let sink = test_sink(false);
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
                .map(|event| render_one(&sink, event).1);

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
                    &test_sink(false),
                    Event::ToolCallEnd(text_result("t2", "   \n", true, None)),
                );
                assert_eq!(stderr, "\n");
            }

            #[test]
            fn tasks_updated_marks_each_task_under_its_progress_and_skips_a_cleared_list() {
                let sink = test_sink(false);

                let (stdout, stderr) = render_one(&sink, Event::TasksUpdated(plan()));
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

                let (stdout, stderr) = render_one(&sink, Event::TasksUpdated(Vec::new()));
                assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));
            }

            #[test]
            fn auto_compaction_started_explains_the_pause() {
                let (stdout, stderr) = render_one(&test_sink(false), Event::AutoCompactionStarted);

                assert_eq!(stdout, "");
                assert_eq!(stderr, "Compacting the conversation to free up context\n");
            }

            #[test]
            fn session_compacted_writes_the_boundary_with_its_focus_escaped() {
                let (stdout, stderr) = render_one(
                    &test_sink(false),
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
            fn session_confirmations_write_to_stderr() {
                let cases = [
                    (
                        Event::SessionTitleUpdated {
                            session_id: "sid".to_owned(),
                            title: "Fix login\u{1b}[2J".to_owned(),
                        },
                        "Renamed the session to Fix login\\u{1b}[2J\n",
                    ),
                    (
                        Event::SessionRolled {
                            id: "0123456789abcdef".to_owned(),
                            archived: false,
                        },
                        "Started session 01234567\n",
                    ),
                    (
                        Event::SessionResumed {
                            id: "89abcdef01234567".to_owned(),
                            title: Some("Fix login".to_owned()),
                            unarchived: false,
                            history: Vec::new(),
                        },
                        "Resumed session 89abcdef: Fix login\n",
                    ),
                    (
                        Event::SessionResumed {
                            id: "89abcdef01234567".to_owned(),
                            title: None,
                            unarchived: true,
                            history: Vec::new(),
                        },
                        "Resumed session 89abcdef: (untitled)\nUnarchived session 89abcdef\n",
                    ),
                ];

                for (event, expected) in cases {
                    let (stdout, stderr) = render_one(&test_sink(false), event);

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

                let (stdout, stderr) = render_one(&test_sink(false), event);

                assert_eq!(stdout, "");
                assert_eq!(stderr, "Effort set to low\n");
            }

            #[test]
            fn mode_changed_names_the_mode_in_use() {
                let (stdout, stderr) =
                    render_one(&test_sink(false), Event::ModeChanged(Mode::Plan));

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
                    Event::BackgroundShellsUpdated(shells_in_each_state()),
                ] {
                    let (stdout, stderr) = render_one(&test_sink(false), event);
                    assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
                    assert!(stderr.is_empty(), "stderr must stay empty: {stderr:?}");
                }
            }

            #[test]
            fn turn_complete_ends_the_open_reply_line_once() {
                let sink = test_sink(true);
                render_one(&sink, Event::ThinkingToken("plan".to_owned()));

                let ended = render_one(&sink, Event::TurnComplete);
                let again = render_one(&sink, Event::TurnComplete);

                assert_eq!(ended, ("\n".to_owned(), String::new()));
                assert_eq!(again, (String::new(), String::new()));
            }

            #[test]
            fn turn_complete_leaves_a_closed_reply_line_alone() {
                let sink = test_sink(false);
                render_one(&sink, Event::StreamToken("Done.\n".to_owned()));
                render_one(&sink, Event::StreamToken(String::new()));

                assert_eq!(
                    render_one(&sink, Event::TurnComplete),
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

                let (retry_stdout, retry_stderr) =
                    render_one(&test_sink(false), Event::Retrying(retry.to_owned()));
                let (notice_stdout, notice_stderr) =
                    render_one(&test_sink(false), Event::Notice(notice.to_owned()));

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
                let sink = test_sink(false);
                render_one(&sink, Event::StreamToken("partial reply".to_owned()));
                let mut stderr = Vec::new();

                let rendered = sink.render(
                    Event::TurnFailed("boom".to_owned()),
                    &mut ClosedPipe,
                    &mut stderr,
                );

                assert!(rendered.is_err());
                assert_eq!(stderr, b"Error: boom\n");
            }

            #[test]
            fn errors_prefix_with_error_label() {
                let (_, stderr) = render_one(&test_sink(false), Event::Error("boom".to_owned()));
                assert_eq!(stderr, "Error: boom\n");
                let (_, stderr) =
                    render_one(&test_sink(false), Event::RenameFailed("lost".to_owned()));
                assert_eq!(stderr, "Error: lost\n");
            }
        }

        mod break_line {
            use super::*;

            #[test]
            fn ends_the_open_reply_line_past_the_echoed_signal() {
                let sink = StdioSink::new(false);
                let screen = Screen::default();
                sink.render(
                    Event::StreamToken("partial".to_owned()),
                    &mut screen.clone(),
                    &mut screen.clone(),
                )
                .expect("render returns Ok");

                sink.break_line(&mut screen.clone(), &mut screen.clone())
                    .expect("break_line returns Ok");
                sink.render(Event::Cancelled, &mut screen.clone(), &mut screen.clone())
                    .expect("render returns Ok");

                assert_eq!(screen.text(), format!("partial\n{INTERRUPTED_MARKER}\n"));
            }

            #[test]
            fn writes_a_newline_to_stderr_when_no_reply_line_is_open() {
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

                StdioSink::new(false)
                    .break_line(&mut stdout, &mut stderr)
                    .expect("break_line returns Ok");

                assert_eq!((stdout, stderr), (Vec::new(), b"\n".to_vec()));
            }
        }
    }
}
