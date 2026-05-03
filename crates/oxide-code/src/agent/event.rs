use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::Effort;
use crate::tool::ToolRegistry;

// ── Visible Markers ──

pub(crate) const INTERRUPTED_MARKER: &str = "(interrupted)";

// ── Agent Events ──

/// Events emitted by the agent loop for display.
#[derive(Debug, Clone)]
pub(crate) enum AgentEvent {
    StreamToken(String),
    ThinkingToken(String),
    ToolCallStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolCallEnd {
        id: String,
        content: String,
        is_error: bool,
        metadata: crate::tool::ToolMetadata,
    },
    PromptDrained(String),
    TurnComplete,
    Cancelled,
    SessionTitleUpdated {
        session_id: String,
        title: String,
    },
    SessionRolled {
        id: String,
    },
    ModelSwitched {
        model_id: String,
        effort: Option<Effort>,
    },
    EffortSwitched {
        pick: Effort,
        effort: Option<Effort>,
    },
    Error(String),
}

// ── User Actions ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UserAction {
    SubmitPrompt(String),
    Clear,
    SwitchModel(String),
    SwitchEffort(Effort),
    Cancel,
    /// TUI-only; agent loop ignores this.
    ConfirmExit,
    Quit,
}

/// Channel pair whose `recv()` stays pending forever (no in-process sender).
pub(crate) fn inert_user_action_channel() -> (mpsc::Sender<UserAction>, mpsc::Receiver<UserAction>)
{
    mpsc::channel(1)
}

// ── Agent Sink ──

pub(crate) const AGENT_EVENT_CHANNEL_CAP: usize = 4096;

/// Abstraction over where agent events are sent (TUI channel / stdio).
pub(crate) trait AgentSink: Send + Sync {
    fn send(&self, event: AgentEvent) -> Result<()>;

    fn session_write_error(&self, failure: Option<&str>) {
        if let Some(msg) = failure {
            _ = self.send(AgentEvent::Error(format!("Session write failed: {msg}")));
        }
    }
}

// ── Stdio Sink (bare REPL / headless) ──

pub(crate) struct StdioSink {
    show_thinking: bool,
    tools: Arc<ToolRegistry>,
}

impl StdioSink {
    pub(crate) fn new(show_thinking: bool, tools: Arc<ToolRegistry>) -> Self {
        Self {
            show_thinking,
            tools,
        }
    }
}

impl StdioSink {
    fn render<W1: std::io::Write, W2: std::io::Write>(
        &self,
        event: AgentEvent,
        stdout: &mut W1,
        stderr: &mut W2,
    ) -> Result<()> {
        match event {
            AgentEvent::StreamToken(text) => {
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            AgentEvent::ThinkingToken(text) => {
                if self.show_thinking {
                    write!(stdout, "\x1b[2m{text}\x1b[22m")?;
                    stdout.flush()?;
                }
            }
            AgentEvent::ToolCallStart { name, input, .. } => {
                let icon = self.tools.icon(&name);
                let label = self.tools.label(&name, &input);
                writeln!(stderr, "{icon} {label}")?;
            }
            AgentEvent::ToolCallEnd {
                content, metadata, ..
            } => {
                if let Some(title) = metadata.title {
                    writeln!(stderr, "  {title}")?;
                }
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    writeln!(stderr, "{trimmed}")?;
                }
                writeln!(stderr)?;
            }
            // TUI-only — no stdio surface to update.
            AgentEvent::PromptDrained(_)
            | AgentEvent::SessionTitleUpdated { .. }
            | AgentEvent::SessionRolled { .. }
            | AgentEvent::ModelSwitched { .. }
            | AgentEvent::EffortSwitched { .. } => {}
            AgentEvent::TurnComplete => {
                writeln!(stdout)?;
            }
            AgentEvent::Cancelled => {
                writeln!(stdout)?;
                writeln!(stderr, "{INTERRUPTED_MARKER}")?;
            }
            AgentEvent::Error(msg) => {
                writeln!(stderr, "Error: {msg}")?;
            }
        }
        Ok(())
    }
}

impl AgentSink for StdioSink {
    fn send(&self, event: AgentEvent) -> Result<()> {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        self.render(event, &mut stdout.lock(), &mut stderr.lock())
    }
}

// ── Test Fixtures ──

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct CapturingSink(std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>>);

#[cfg(test)]
impl CapturingSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn events(&self) -> Vec<AgentEvent> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl AgentSink for CapturingSink {
    fn send(&self, event: AgentEvent) -> Result<()> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;

    // ── StdioSink::render ──

    fn test_sink(show_thinking: bool) -> StdioSink {
        StdioSink::new(show_thinking, Arc::new(ToolRegistry::new(Vec::new())))
    }

    fn render_one(sink: &StdioSink, event: AgentEvent) -> (String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        sink.render(event, &mut stdout, &mut stderr)
            .expect("render returns Ok");
        (
            String::from_utf8(stdout).expect("stdout is UTF-8"),
            String::from_utf8(stderr).expect("stderr is UTF-8"),
        )
    }

    #[test]
    fn render_stream_token_writes_text_verbatim_to_stdout() {
        let (stdout, stderr) = render_one(
            &test_sink(false),
            AgentEvent::StreamToken("hello".to_owned()),
        );
        assert_eq!(stdout, "hello");
        assert!(stderr.is_empty());
    }

    #[test]
    fn render_thinking_token_wraps_in_dim_when_enabled() {
        let (stdout, _) = render_one(
            &test_sink(true),
            AgentEvent::ThinkingToken("plan".to_owned()),
        );
        assert_eq!(stdout, "\x1b[2mplan\x1b[22m");
    }

    #[test]
    fn render_thinking_token_swallowed_when_disabled() {
        let (stdout, stderr) = render_one(
            &test_sink(false),
            AgentEvent::ThinkingToken("plan".to_owned()),
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn render_tool_call_start_writes_icon_label_to_stderr() {
        let (_, stderr) = render_one(
            &test_sink(false),
            AgentEvent::ToolCallStart {
                id: "t1".to_owned(),
                name: "unregistered".to_owned(),
                input: serde_json::Value::Null,
            },
        );
        assert!(stderr.ends_with('\n'));
        assert_eq!(stderr.lines().count(), 1);
        assert!(stderr.contains("unregistered"));
    }

    #[test]
    fn render_tool_call_end_with_title_writes_title_then_content() {
        let (_, stderr) = render_one(
            &test_sink(false),
            AgentEvent::ToolCallEnd {
                id: "t1".to_owned(),
                content: "file1\nfile2\n".to_owned(),
                is_error: false,
                metadata: crate::tool::ToolMetadata {
                    title: Some("ls".to_owned()),
                    ..crate::tool::ToolMetadata::default()
                },
            },
        );
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(lines[0], "  ls");
        assert_eq!(lines[1], "file1");
        assert_eq!(lines[2], "file2");
        assert!(stderr.ends_with("\n\n"));
    }

    #[test]
    fn render_tool_call_end_without_title_skips_header_and_whitespace_content() {
        let (_, stderr) = render_one(
            &test_sink(false),
            AgentEvent::ToolCallEnd {
                id: "t2".to_owned(),
                content: "   \n".to_owned(),
                is_error: true,
                metadata: crate::tool::ToolMetadata::default(),
            },
        );
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn render_tui_only_events_emit_nothing_on_either_stream() {
        for event in [
            AgentEvent::PromptDrained("queued".to_owned()),
            AgentEvent::SessionTitleUpdated {
                session_id: "sid".to_owned(),
                title: "New title".to_owned(),
            },
            AgentEvent::SessionRolled {
                id: "rolled".to_owned(),
            },
            AgentEvent::ModelSwitched {
                model_id: "claude-opus-4-7".to_owned(),
                effort: Some(Effort::Xhigh),
            },
            AgentEvent::EffortSwitched {
                pick: Effort::High,
                effort: Some(Effort::High),
            },
        ] {
            let (stdout, stderr) = render_one(&test_sink(false), event);
            assert!(stdout.is_empty(), "stdout must stay empty: {stdout:?}");
            assert!(stderr.is_empty(), "stderr must stay empty: {stderr:?}");
        }
    }

    #[test]
    fn render_turn_complete_writes_trailing_newline_to_stdout() {
        let (stdout, stderr) = render_one(&test_sink(false), AgentEvent::TurnComplete);
        assert_eq!(stdout, "\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn render_cancelled_writes_marker_to_stderr_and_blank_to_stdout() {
        let (stdout, stderr) = render_one(&test_sink(false), AgentEvent::Cancelled);
        assert_eq!(stdout, "\n");
        assert_eq!(stderr.trim(), INTERRUPTED_MARKER);
    }

    #[test]
    fn render_error_prefixes_with_error_label() {
        let (_, stderr) = render_one(&test_sink(false), AgentEvent::Error("boom".to_owned()));
        assert_eq!(stderr, "Error: boom\n");
    }
}
