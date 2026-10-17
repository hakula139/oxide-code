//! Stdio rendering of engine events for the `--no-tui` REPL and `--prompt` headless modes.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::debug;

use crate::config::Effort;
use crate::engine::TurnEnd;
use crate::model::ModelSpec;
use crate::permission::defaults::ShippedTier;
use crate::permission::{Basis, SessionGrant};
use crate::protocol::{Decision, Event, EventSink, Op, ToolCall, ToolResult};
use crate::session::display::{UNTITLED_MARKER, archive_notice, id_prefix};
use crate::tool::ApprovalPreview;
use crate::tool::tasks::{Progress, Task};
use crate::tui::glyphs::{INTERRUPTED_MARKER, task_marker};
use crate::util::text::escape_controls;

// ── Turn Rendering ──

/// Renders `events` on `sink` until `op`, already sent, ends, returning how it ended, or `None`
/// once the engine has stopped. Approval requests go to `approver` in place of `sink`, and its
/// answer goes back on `ops`.
pub(super) async fn render_op(
    events: &mut mpsc::UnboundedReceiver<Event>,
    sink: &dyn EventSink,
    ops: &mpsc::Sender<Op>,
    op: &Op,
    approver: &mut impl Approver,
) -> Option<TurnEnd> {
    while let Some(event) = events.recv().await {
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
        let decision = approver.decide(&preview, &basis, grant.as_deref()).await;
        // A send fails only once the engine has stopped, which the next `recv` reports.
        _ = ops.send(Op::Resolve { id, decision }).await;
    }
    None
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
                    "This run cannot ask for approval. To allow this call, add `{}` to \
                     `permission.allow.{}` in the user config.",
                    toml::Value::from(rule.pattern()),
                    rule.class(),
                )
            }
            (None, Basis::Builtin(group)) if group.tier() == ShippedTier::Ask => format!(
                "This run cannot ask for approval, and {basis} holds this call. Approve it in an \
                 interactive session, or set `permission.defaults.{group} = false` in the user \
                 config."
            ),
            (None, Basis::Mode(_)) => {
                "This run cannot ask for approval, and no allow pattern matches this call. \
                 Under `permission.allow` in the user config, a chained command needs a pattern \
                 per command, and a word that expands needs a pattern ending in `*` at that word. \
                 A command that hides what it runs, such as `$(...)` or `sh -c`, needs approval \
                 in an interactive session."
                    .to_owned()
            }
            (None, basis) => format!(
                "This run cannot ask for approval, and {basis} holds this call. Approve it in an \
                 interactive session."
            ),
        };
        std::future::ready(Decision::DenyWithFeedback { feedback })
    }
}

// ── StdioSink ──

/// [`EventSink`] for the `--no-tui` REPL: assistant tokens stream to stdout, everything else (tool
/// calls, errors, the interrupted marker) to stderr so piped output stays clean. Headless runs
/// reuse [`Self::render`] for their text output. Model and tool text passes through
/// [`escape_controls`], so it cannot redraw the terminal around a REPL approval prompt.
pub(super) struct StdioSink {
    show_thinking: bool,
    calls: Mutex<StartedCalls>,
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
        }
    }

    pub(super) fn render<W1: Write, W2: Write>(
        &self,
        event: Event,
        stdout: &mut W1,
        stderr: &mut W2,
    ) -> Result<()> {
        match event {
            Event::StreamToken(text) => {
                stdout.write_all(escape_controls(&text).as_bytes())?;
                stdout.flush()?;
            }
            Event::ThinkingToken(text) => {
                if self.show_thinking {
                    write!(stdout, "\x1b[2m{}\x1b[22m", escape_controls(&text))?;
                    stdout.flush()?;
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
                writeln!(stderr, "{header}")?;
                self.calls().start(id, header);
            }
            Event::ToolCallEnd(result) => self.render_result(result, stderr)?,
            Event::TasksUpdated(tasks) => write!(stderr, "{}", task_lines(&tasks))?,
            Event::SessionCompacted {
                pre_count,
                automatic,
                ..
            } => {
                let label = if automatic {
                    "Auto-compacted"
                } else {
                    "Compacted"
                };
                writeln!(stderr, "{label} {pre_count} messages into summary")?;
            }
            Event::SessionTitleUpdated { title, .. } => {
                let title = escape_controls(&title);
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
            Event::ConfigChanged {
                model,
                effort,
                requested_effort,
                ..
            } => {
                let notice = config_change(&model, effort, requested_effort);
                writeln!(stderr, "{notice}")?;
            }
            Event::ModeChanged(mode) => writeln!(stderr, "Using {mode} mode")?,
            // TUI-only events have no stdio surface to update.
            Event::PromptDrained(_)
            | Event::UsageUpdated(_)
            | Event::RateLimitsUpdated(_)
            | Event::AutoCompactionStarted
            | Event::AutoCompactionPaused { .. }
            | Event::AutoCompactionResumed
            | Event::SummaryToken(_)
            | Event::SummaryDiscarded
            | Event::LoginStarted { .. }
            | Event::SignedIn { .. }
            | Event::SignedOut { .. }
            | Event::ApprovalRequested { .. }
            | Event::SessionRulesUpdated(_)
            | Event::BackgroundShellsUpdated(_) => {}
            Event::TurnComplete => {
                writeln!(stdout)?;
            }
            Event::Cancelled => {
                writeln!(stdout)?;
                writeln!(stderr, "{INTERRUPTED_MARKER}")?;
            }
            Event::Retrying(msg) => {
                writeln!(stderr, "{}", escape_controls(&msg))?;
            }
            Event::Error(msg) | Event::TurnFailed(msg) | Event::RenameFailed(msg) => {
                writeln!(stderr, "Error: {}", escape_controls(&msg))?;
            }
        }
        Ok(())
    }
}

impl StdioSink {
    fn calls(&self) -> MutexGuard<'_, StartedCalls> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
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
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        self.render(event, &mut stdout.lock(), &mut stderr.lock())
    }
}

/// A progress header, one marked line per task, and a closing blank line, or nothing for a
/// cleared list.
fn task_lines(tasks: &[Task]) -> String {
    let Some(Progress { completed, total }) = Progress::of(tasks) else {
        return String::new();
    };
    let mut lines = format!("Tasks: {completed}/{total}\n");
    for task in tasks {
        let content = escape_controls(&task.content);
        _ = writeln!(lines, "  {} {content}", task_marker(task.status));
    }
    lines.push('\n');
    lines
}

/// The session an [`Event::SessionResumed`] continues, then a note when resuming it unarchived it.
fn resumed_notice(id: &str, title: Option<&str>, unarchived: bool) -> String {
    let title = title.map_or_else(|| UNTITLED_MARKER.to_owned(), escape_controls);
    let resumed = format!("Resumed session {}: {title}", id_prefix(id));
    if unarchived {
        return format!("{resumed}\n{}", archive_notice(id, false));
    }
    resumed
}

/// The model and effort an [`Event::ConfigChanged`] leaves in use, noting a requested effort that
/// `model` has no tier for.
fn config_change(model: &ModelSpec, effort: Option<Effort>, requested: Option<Effort>) -> String {
    let model = format!("{} ({})", model.display_name(), model.id());
    match (effort, requested) {
        (Some(effort), _) => format!("Using {model} at {effort} effort"),
        (None, Some(requested)) => format!("Using {model}. {}.", requested.unapplied_notice()),
        (None, None) => format!("Using {model}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use indoc::indoc;

    use super::*;
    use crate::client::{RateLimitWindow, RateLimits};
    use crate::config::compaction::AutoCompactionConfig;
    use crate::permission::defaults::Shipped;
    use crate::permission::rule::Rule;
    use crate::permission::{Access, Mode, PermissionClass, Policy, Tier, Workspace};
    use crate::protocol::{CapturingSink, FailingSink};
    use crate::tool::bash::background::testing::shells_in_each_state;
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::{plan, task};
    use crate::tool::{ApprovalBody, ToolResultView};

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

    fn bash_preview(command: &str) -> ApprovalPreview {
        ApprovalPreview {
            title: "Bash".to_owned(),
            body: ApprovalBody::Command(command.to_owned()),
        }
    }

    fn bash_grant(command: &str) -> SessionGrant {
        Policy::default()
            .session_grant(
                PermissionClass::Exec,
                &Access::for_command(command, &Workspace::new(Path::new("/repo"))),
            )
            .unwrap()
    }

    // ── render_op ──

    #[tokio::test]
    async fn render_op_answers_each_approval_on_ops() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let approval = Event::ApprovalRequested {
            id: "t1".to_owned(),
            preview: bash_preview("ls"),
            basis: Basis::Mode(Mode::Manual),
            grant: Some(Box::new(bash_grant("ls"))),
        };
        for event in [approval, Event::TurnComplete] {
            tx.send(event).unwrap();
        }
        let (ops_tx, mut ops) = mpsc::channel(1);
        let sink = CapturingSink::new();
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
    async fn render_op_stops_at_the_terminal_event_even_when_rendering_fails() {
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
    async fn render_op_waits_for_the_terminal_event_of_its_own_op() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let notice = Event::Error("Session write failed".to_owned());
        let rolled = Event::SessionRolled {
            id: "next".to_owned(),
            archived: false,
        };
        for event in [notice.clone(), rolled.clone(), Event::TurnComplete] {
            tx.send(event).unwrap();
        }
        drop(tx);
        let sink = CapturingSink::new();

        let (ops, _) = mpsc::channel(1);
        let mut approver = fixed(Decision::Deny);
        let end = render_op(&mut events, &sink, &ops, &Op::Clear, &mut approver).await;

        assert_eq!(end, Some(TurnEnd::Completed));
        assert_eq!(sink.events(), [notice, rolled]);
        assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
    }

    #[tokio::test]
    async fn render_op_reports_a_stopped_engine() {
        let (tx, mut events) = mpsc::unbounded_channel();
        tx.send(Event::StreamToken("partial".to_owned())).unwrap();
        drop(tx);
        let sink = CapturingSink::new();

        let (ops, _) = mpsc::channel(1);
        let mut approver = fixed(Decision::Deny);
        let end = render_op(&mut events, &sink, &ops, &prompt(), &mut approver).await;

        assert_eq!(end, None);
        assert_eq!(sink.events(), [Event::StreamToken("partial".to_owned())]);
    }

    // ── Unattended::decide ──

    #[tokio::test]
    async fn unattended_denies_naming_the_allow_pattern_or_what_holds_the_call() {
        let command = r#"echo "a\b""#;
        let preview = bash_preview(command);
        let manual = Basis::Mode(Mode::Manual);
        let grant = bash_grant(command);
        let ask = Basis::Rule {
            tier: Tier::Ask,
            rule: Rule::parse(PermissionClass::Exec, "git push *").unwrap(),
        };
        let cases = [
            (
                &manual,
                Some(&grant),
                r"This run cannot ask for approval. To allow this call, add `'''echo 'a\b''''` to `permission.allow.exec` in the user config.",
            ),
            (
                &manual,
                None,
                "This run cannot ask for approval, and no allow pattern matches this call. \
                 Under `permission.allow` in the user config, a chained command needs a pattern \
                 per command, and a word that expands needs a pattern ending in `*` at that word. \
                 A command that hides what it runs, such as `$(...)` or `sh -c`, needs approval \
                 in an interactive session.",
            ),
            (
                &Basis::Builtin(Shipped::Git),
                None,
                "This run cannot ask for approval, and the built-in `git` rule holds this call. \
                 Approve it in an interactive session, or set `permission.defaults.git = false` in \
                 the user config.",
            ),
            (
                &Basis::Builtin(Shipped::Ssh),
                None,
                "This run cannot ask for approval, and the built-in `ssh` rule holds this call. \
                 Approve it in an interactive session.",
            ),
            (
                &ask,
                None,
                "This run cannot ask for approval, and the `ask.exec` pattern `git push *` holds \
                 this call. Approve it in an interactive session.",
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

    // ── StdioSink::render ──

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

    fn text_result(id: &str, content: &str, is_error: bool, title: Option<&str>) -> ToolResult {
        ToolResult {
            id: id.to_owned(),
            content: content.to_owned(),
            is_error,
            title: title.map(ToOwned::to_owned),
            view: ToolResultView::Text {
                content: content.to_owned(),
            },
        }
    }

    #[test]
    fn render_stream_token_writes_text_verbatim_to_stdout() {
        let (stdout, stderr) =
            render_one(&test_sink(false), Event::StreamToken("hello".to_owned()));
        assert_eq!(stdout, "hello");
        assert_eq!(stderr, "");
    }

    #[test]
    fn render_escapes_terminal_sequences_in_model_and_tool_text() {
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
    fn render_thinking_token_wraps_in_dim_when_enabled() {
        let (stdout, _) = render_one(&test_sink(true), Event::ThinkingToken("plan".to_owned()));
        assert_eq!(stdout, "\x1b[2mplan\x1b[22m");
    }

    #[test]
    fn render_thinking_token_swallowed_when_disabled() {
        let (stdout, stderr) =
            render_one(&test_sink(false), Event::ThinkingToken("plan".to_owned()));
        assert_eq!(stdout, "");
        assert_eq!(stderr, "");
    }

    #[test]
    fn render_progress_update_writes_to_stderr_only_when_thinking_hidden() {
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
    fn render_tool_call_start_writes_icon_label_to_stderr() {
        let (stdout, stderr) = render_one(
            &test_sink(false),
            Event::ToolCallStart(ToolCall {
                id: "t1".to_owned(),
                name: "bash".to_owned(),
                input: serde_json::json!({"command": "ls"}),
                icon: "$".to_owned(),
                label: "ls".to_owned(),
            }),
        );
        assert_eq!(stdout, "");
        assert_eq!(stderr, "$ ls\n");
    }

    #[test]
    fn render_tool_call_end_with_title_writes_title_then_content() {
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
    fn render_tool_call_end_after_another_call_started_repeats_its_header() {
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
    fn render_tool_call_end_without_title_skips_header_and_whitespace_content() {
        let (_, stderr) = render_one(
            &test_sink(false),
            Event::ToolCallEnd(text_result("t2", "   \n", true, None)),
        );
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn render_tasks_updated_marks_each_task_under_its_progress_and_skips_a_cleared_list() {
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
    fn render_tui_only_events_emit_nothing_on_either_stream() {
        for event in [
            Event::PromptDrained("queued".to_owned()),
            Event::RateLimitsUpdated(RateLimits {
                five_hour: None,
                seven_day: Some(RateLimitWindow {
                    utilization: 0.5,
                    resets_at: 1_760_000_000,
                }),
            }),
            Event::AutoCompactionStarted,
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
    fn render_session_confirmations_write_to_stderr() {
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
    fn render_config_changed_names_the_model_and_the_effort_in_use() {
        let config_changed = |id: &str, effort, requested_effort| Event::ConfigChanged {
            model: ModelSpec::resolve(id.to_owned()),
            effort,
            requested_effort,
            auto_compaction: AutoCompactionConfig::disabled(),
        };
        let cases = [
            (
                config_changed("claude-opus-5-5", Some(Effort::Low), Some(Effort::Low)),
                "Using Claude Opus 5.5 (claude-opus-5-5) at low effort\n",
            ),
            (
                config_changed("custom-model", None, Some(Effort::High)),
                "Using custom-model (custom-model). Effort unchanged: model has no effort tier \
                 (asked for high).\n",
            ),
            (
                config_changed("custom-model", None, None),
                "Using custom-model (custom-model)\n",
            ),
        ];

        for (event, expected) in cases {
            let (stdout, stderr) = render_one(&test_sink(false), event);

            assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
            assert_eq!(stderr, expected);
        }
    }

    #[test]
    fn render_mode_changed_names_the_mode_in_use() {
        let (stdout, stderr) = render_one(&test_sink(false), Event::ModeChanged(Mode::Plan));

        assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
        assert_eq!(stderr, "Using plan mode\n");
    }

    #[test]
    fn render_session_compacted_writes_stderr_boundary() {
        let (_, stderr) = render_one(
            &test_sink(false),
            Event::SessionCompacted {
                summary: "summary".to_owned(),
                pre_count: 4,
                instructions: None,
                automatic: false,
            },
        );
        assert_eq!(stderr, "Compacted 4 messages into summary\n");

        let (_, stderr) = render_one(
            &test_sink(false),
            Event::SessionCompacted {
                summary: "summary".to_owned(),
                pre_count: 4,
                instructions: None,
                automatic: true,
            },
        );
        assert_eq!(stderr, "Auto-compacted 4 messages into summary\n");
    }

    #[test]
    fn render_turn_complete_writes_trailing_newline_to_stdout() {
        let (stdout, stderr) = render_one(&test_sink(false), Event::TurnComplete);
        assert_eq!(stdout, "\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn render_cancelled_writes_marker_to_stderr_and_blank_to_stdout() {
        let (stdout, stderr) = render_one(&test_sink(false), Event::Cancelled);
        assert_eq!(stdout, "\n");
        assert_eq!(stderr.trim(), INTERRUPTED_MARKER);
    }

    #[test]
    fn render_retrying_writes_the_notice_to_stderr() {
        let notice = "HTTP 529 overloaded_error. Retrying in 1 s (retry 1 of 5).";
        let (stdout, stderr) = render_one(&test_sink(false), Event::Retrying(notice.to_owned()));
        assert!(stdout.is_empty(), "{stdout:?}");
        assert_eq!(stderr, format!("{notice}\n"));
    }

    #[test]
    fn render_errors_and_failures_prefix_with_error_label() {
        let (_, stderr) = render_one(&test_sink(false), Event::Error("boom".to_owned()));
        assert_eq!(stderr, "Error: boom\n");
        let (_, stderr) = render_one(&test_sink(false), Event::TurnFailed("bust".to_owned()));
        assert_eq!(stderr, "Error: bust\n");
        let (_, stderr) = render_one(&test_sink(false), Event::RenameFailed("lost".to_owned()));
        assert_eq!(stderr, "Error: lost\n");
    }
}
