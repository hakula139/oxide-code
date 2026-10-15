//! Stdio rendering of engine events for the `--no-tui` REPL and `--prompt` headless modes.

use std::io::Write;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::debug;

use crate::engine::TurnEnd;
use crate::protocol::{Event, EventSink, ToolCall, ToolResult};
use crate::tui::glyphs::INTERRUPTED_MARKER;

// ── Turn Rendering ──

/// Renders `events` on `sink` until the prompt in flight ends, returning how it ended, or `None`
/// once the engine has stopped.
pub(super) async fn render_turn(
    events: &mut mpsc::UnboundedReceiver<Event>,
    sink: &dyn EventSink,
) -> Option<TurnEnd> {
    while let Some(event) = events.recv().await {
        let end = TurnEnd::of_prompt(&event);
        render(sink, event);
        if end.is_some() {
            return end;
        }
    }
    None
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

// ── StdioSink ──

/// [`EventSink`] for the `--no-tui` REPL and `--prompt` headless modes: assistant tokens stream
/// to stdout, everything else (tool calls, errors, the interrupted marker) to stderr so piped
/// output stays clean.
pub(super) struct StdioSink {
    show_thinking: bool,
}

impl StdioSink {
    pub(super) fn new(show_thinking: bool) -> Self {
        Self { show_thinking }
    }

    fn render<W1: Write, W2: Write>(
        &self,
        event: Event,
        stdout: &mut W1,
        stderr: &mut W2,
    ) -> Result<()> {
        match event {
            Event::StreamToken(text) => {
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            Event::ThinkingToken(text) => {
                if self.show_thinking {
                    write!(stdout, "\x1b[2m{text}\x1b[22m")?;
                    stdout.flush()?;
                }
            }
            Event::ProgressUpdate(text) => {
                if !self.show_thinking {
                    writeln!(stderr, "{}", text.trim())?;
                }
            }
            Event::ToolCallStart(ToolCall { icon, label, .. }) => {
                writeln!(stderr, "{icon} {label}")?;
            }
            Event::ToolCallEnd(ToolResult { content, title, .. }) => {
                if let Some(title) = title {
                    writeln!(stderr, "  {title}")?;
                }
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    writeln!(stderr, "{trimmed}")?;
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
                writeln!(stderr, "{msg}")?;
            }
            Event::Error(msg) | Event::TurnFailed(msg) => {
                writeln!(stderr, "Error: {msg}")?;
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
    use super::*;
    use crate::config::{AutoCompactionConfig, Effort};
    use crate::model::ModelSpec;
    use crate::protocol::{CapturingSink, FailingSink};
    use crate::tool::ToolResultView;

    // ── render_turn ──

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

        let end = render_turn(&mut events, &FailingSink).await;

        assert_eq!(end, Some(TurnEnd::Cancelled));
        assert_eq!(events.try_recv().ok(), Some(Event::TurnComplete));
    }

    #[tokio::test]
    async fn render_turn_reports_a_stopped_engine() {
        let (tx, mut events) = mpsc::unbounded_channel();
        tx.send(Event::StreamToken("partial".to_owned())).unwrap();
        drop(tx);
        let sink = CapturingSink::new();

        let end = render_turn(&mut events, &sink).await;

        assert_eq!(end, None);
        assert_eq!(sink.events(), [Event::StreamToken("partial".to_owned())]);
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
        assert_eq!(stderr, "  Listed\nfile1\nfile2\n\n");
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
