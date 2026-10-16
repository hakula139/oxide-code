//! Stdio rendering of engine events for the `--no-tui` REPL and `--prompt` headless modes.

use std::io::Write;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::debug;

use crate::engine::TurnEnd;
use crate::permission::SessionGrant;
use crate::protocol::{Decision, Event, EventSink, Op, ToolCall, ToolResult};
use crate::tool::ApprovalPreview;
use crate::tui::glyphs::INTERRUPTED_MARKER;
use crate::util::text::escape_controls;

// ── Turn Rendering ──

/// Renders `events` on `sink` until the prompt in flight ends, returning how it ended, or `None`
/// once the engine has stopped. Approval requests go to `approver` in place of `sink`, and its
/// answer goes back on `ops`.
pub(super) async fn render_turn(
    events: &mut mpsc::UnboundedReceiver<Event>,
    sink: &dyn EventSink,
    ops: &mpsc::Sender<Op>,
    approver: &mut impl Approver,
) -> Option<TurnEnd> {
    while let Some(event) = events.recv().await {
        let Event::ApprovalRequested { id, preview, grant } = event else {
            let end = TurnEnd::of_prompt(&event);
            render(sink, event);
            if end.is_some() {
                return end;
            }
            continue;
        };
        let decision = approver.decide(&preview, grant.as_deref()).await;
        // A send fails only once the engine has stopped, which the next `recv` reports.
        _ = ops.send(Op::Resolve { id, decision }).await;
    }
    None
}

/// How a stdio frontend answers an approval request.
pub(super) trait Approver {
    /// Decides the call `preview` shows, which `grant` may allow for the rest of the session.
    fn decide(
        &mut self,
        preview: &ApprovalPreview,
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

/// Answers every approval with a denial that names the allow rule a rerun would need, for a run
/// with nobody to ask.
pub(super) struct Unattended;

impl Approver for Unattended {
    fn decide(
        &mut self,
        _: &ApprovalPreview,
        grant: Option<&SessionGrant>,
    ) -> impl Future<Output = Decision> {
        let feedback = match grant {
            Some(grant) => format!(
                "This run cannot ask for approval. To allow this call, add `{}` to the `allow` \
                 list in the `[permission]` table of the user config.",
                toml::Value::from(grant.suggested().to_string()),
            ),
            None => "This run cannot ask for approval, and no allow rule names this call exactly. \
                     Approve it in an interactive session, or add a broader rule that covers it \
                     to the `allow` list in the `[permission]` table of the user config."
                .to_owned(),
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
}

impl StdioSink {
    pub(super) fn new(show_thinking: bool) -> Self {
        Self { show_thinking }
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
            Event::ToolCallStart(ToolCall { icon, label, .. }) => {
                writeln!(stderr, "{icon} {}", escape_controls(&label))?;
            }
            Event::ToolCallEnd(ToolResult { content, title, .. }) => {
                if let Some(title) = title {
                    writeln!(stderr, "  {}", escape_controls(&title))?;
                }
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    writeln!(stderr, "{}", escape_controls(trimmed))?;
                }
                writeln!(stderr)?;
            }
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
            // TUI-only events have no stdio surface to update.
            Event::PromptDrained(_)
            | Event::UsageUpdated(_)
            | Event::AutoCompactionStarted
            | Event::SessionTitleUpdated { .. }
            | Event::SessionRolled { .. }
            | Event::SessionResumed { .. }
            | Event::ConfigChanged { .. }
            | Event::LoginStarted { .. }
            | Event::SignedIn { .. }
            | Event::SignedOut { .. }
            | Event::ApprovalRequested { .. }
            | Event::SessionRulesUpdated(_) => {}
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
            Event::Error(msg) | Event::TurnFailed(msg) => {
                writeln!(stderr, "Error: {}", escape_controls(&msg))?;
            }
        }
        Ok(())
    }
}

impl EventSink for StdioSink {
    fn send(&self, event: Event) -> Result<()> {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        self.render(event, &mut stdout.lock(), &mut stderr.lock())
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::config::{AutoCompactionConfig, Effort};
    use crate::model::ModelSpec;
    use crate::permission::{GateTarget, PermissionClass, Policy};
    use crate::protocol::{CapturingSink, FailingSink};
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
            grant: Option<&SessionGrant>,
        ) -> impl Future<Output = Decision> {
            let rule = grant.map(|grant| grant.suggested().to_string());
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

    fn bash_preview(command: &str) -> ApprovalPreview {
        ApprovalPreview {
            title: "Bash".to_owned(),
            body: ApprovalBody::Command(command.to_owned()),
        }
    }

    fn bash_grant(command: &str) -> SessionGrant {
        Policy::default()
            .session_grant(
                PermissionClass::Bash,
                &GateTarget::Command(command.to_owned()),
            )
            .unwrap()
    }

    // ── render_turn ──

    #[tokio::test]
    async fn render_turn_answers_each_approval_on_ops() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let approval = Event::ApprovalRequested {
            id: "t1".to_owned(),
            preview: bash_preview("ls"),
            grant: Some(Box::new(bash_grant("ls"))),
        };
        for event in [approval, Event::TurnComplete] {
            tx.send(event).unwrap();
        }
        let (ops_tx, mut ops) = mpsc::channel(1);
        let sink = CapturingSink::new();
        let mut approver = fixed(Decision::Deny);

        let end = render_turn(&mut events, &sink, &ops_tx, &mut approver).await;

        assert_eq!(end, Some(TurnEnd::Completed));
        assert_eq!(
            approver.offered,
            [("Bash".to_owned(), Some("bash(ls)".to_owned()))],
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
    async fn render_turn_stops_at_the_terminal_event_even_when_rendering_fails() {
        let (tx, mut events) = mpsc::unbounded_channel();
        for event in [
            Event::StreamToken("reply".to_owned()),
            Event::Cancelled,
            Event::TurnComplete,
        ] {
            tx.send(event).unwrap();
        }

        let (ops, _) = mpsc::channel(1);
        let end = render_turn(&mut events, &FailingSink, &ops, &mut fixed(Decision::Deny)).await;

        assert_eq!(end, Some(TurnEnd::Cancelled));
        assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
    }

    #[tokio::test]
    async fn render_turn_reports_a_stopped_engine() {
        let (tx, mut events) = mpsc::unbounded_channel();
        tx.send(Event::StreamToken("partial".to_owned())).unwrap();
        drop(tx);
        let sink = CapturingSink::new();

        let (ops, _) = mpsc::channel(1);
        let end = render_turn(&mut events, &sink, &ops, &mut fixed(Decision::Deny)).await;

        assert_eq!(end, None);
        assert_eq!(sink.events(), [Event::StreamToken("partial".to_owned())]);
    }

    // ── Unattended::decide ──

    #[tokio::test]
    async fn unattended_denies_naming_the_allow_rule_or_saying_that_none_names_the_call() {
        let command = r#"echo "a\b""#;
        let preview = bash_preview(command);

        let Decision::DenyWithFeedback { feedback } = Unattended
            .decide(&preview, Some(&bash_grant(command)))
            .await
        else {
            panic!("an unattended approval denies with feedback");
        };
        assert!(
            feedback.contains(r#"add `'bash(echo "a\b")'` to the `allow` list"#),
            "the rule is quoted as a TOML string: {feedback}"
        );

        let Decision::DenyWithFeedback { feedback } = Unattended.decide(&preview, None).await
        else {
            panic!("an unattended approval denies with feedback");
        };
        assert!(
            feedback.contains("no allow rule names this call exactly"),
            "{feedback}"
        );
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
    fn render_tool_call_end_without_title_skips_header_and_whitespace_content() {
        let (_, stderr) = render_one(
            &test_sink(false),
            Event::ToolCallEnd(text_result("t2", "   \n", true, None)),
        );
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn render_tui_only_events_emit_nothing_on_either_stream() {
        for event in [
            Event::PromptDrained("queued".to_owned()),
            Event::AutoCompactionStarted,
            Event::SessionTitleUpdated {
                session_id: "sid".to_owned(),
                title: "New title".to_owned(),
            },
            Event::SessionRolled {
                id: "rolled".to_owned(),
                archived: true,
            },
            Event::ConfigChanged {
                model: ModelSpec::resolve("claude-opus-5-5".to_owned()),
                effort: Some(Effort::Xhigh),
                requested_effort: Some(Effort::Xhigh),
                auto_compaction: AutoCompactionConfig::disabled(),
            },
        ] {
            let (stdout, stderr) = render_one(&test_sink(false), event);
            assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
            assert!(stderr.is_empty(), "stderr must stay empty: {stderr:?}");
        }
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
        let notice = "HTTP 529 overloaded_error. Retrying in 1s (retry 1 of 5).";
        let (stdout, stderr) = render_one(&test_sink(false), Event::Retrying(notice.to_owned()));
        assert!(stdout.is_empty(), "{stdout:?}");
        assert_eq!(stderr, format!("{notice}\n"));
    }

    #[test]
    fn render_error_and_turn_failure_prefix_with_error_label() {
        let (_, stderr) = render_one(&test_sink(false), Event::Error("boom".to_owned()));
        assert_eq!(stderr, "Error: boom\n");
        let (_, stderr) = render_one(&test_sink(false), Event::TurnFailed("bust".to_owned()));
        assert_eq!(stderr, "Error: bust\n");
    }
}
