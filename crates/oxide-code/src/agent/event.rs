use std::sync::Arc;

use anyhow::Result;

use crate::tool::ToolRegistry;

// ── Agent Events ──

/// Events emitted by the agent loop for display.
///
/// The agent loop sends these through a channel; the TUI (or REPL sink)
/// consumes them to update the display. Each variant carries exactly the
/// data needed for rendering — no model-facing payloads.
#[derive(Debug, Clone)]
pub(crate) enum AgentEvent {
    /// A chunk of assistant text (streamed incrementally).
    StreamToken(String),
    /// A chunk of thinking text (streamed incrementally).
    ThinkingToken(String),
    /// A tool call has started execution. `id` is the call's
    /// correlation handle — [`PendingCalls`](crate::agent::pending_calls::PendingCalls)
    /// stashes the tool name + input under it so the paired
    /// [`Self::ToolCallEnd`] can build a structured
    /// [`ToolResultView`](crate::tool::ToolResultView).
    ToolCallStart {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool call has finished. `metadata` carries the tool's
    /// [`ToolMetadata`](crate::tool::ToolMetadata) — the display
    /// title (nullable; falls back to the pending-call label),
    /// plus structured hints like Edit's replacement count that the
    /// TUI threads into
    /// [`ToolRegistry::result_view`](crate::tool::ToolRegistry::result_view)
    /// so per-tool renderers don't re-parse `content`.
    ToolCallEnd {
        id: String,
        content: String,
        is_error: bool,
        metadata: crate::tool::ToolMetadata,
    },
    /// The current assistant turn is complete (text-only response, no more
    /// tool calls).
    TurnComplete,
    /// Mid-flight turn was dropped in response to a [`UserAction::Cancel`].
    /// Same teardown as [`Self::TurnComplete`] plus an `(interrupted)`
    /// marker on the partial assistant block.
    Cancelled,
    /// A newly-generated session title (e.g., AI-generated via Haiku). The
    /// TUI updates the status bar slot; other sinks ignore it.
    SessionTitleUpdated(String),
    /// A fatal error from the API or agent loop.
    Error(String),
}

// ── User Actions ──

/// Actions from the user that the agent loop consumes.
#[derive(Debug, Clone)]
pub(crate) enum UserAction {
    /// Submit a prompt to the agent.
    SubmitPrompt(String),
    /// Cancel the in-flight turn. No-op when the agent is idle.
    Cancel,
    /// User requested quit.
    Quit,
}

// ── Agent Sink ──

/// Capacity of the bounded agent-event channel. `StreamToken` fires
/// ~30-60/s, so 4096 gives tens of seconds of headroom before a stalled
/// TUI surfaces `TrySendError::Full`.
pub(crate) const AGENT_EVENT_CHANNEL_CAP: usize = 4096;

/// Abstraction over where agent events are sent.
///
/// - `ChannelSink` (in `tui::event`) sends events to the TUI via an async
///   channel.
/// - [`StdioSink`] writes directly to stdout / stderr for the bare REPL.
///
/// This keeps the agent loop DRY — the same code drives both display modes.
pub(crate) trait AgentSink: Send + Sync {
    fn send(&self, event: AgentEvent) -> Result<()>;

    /// Emit an `Error` event for a session-write failure surfaced via
    /// `Outcome` / `RecordOutcome` in `session::handle`. No-op when
    /// `failure` is `None`. Send errors (e.g., a closed TUI channel
    /// during teardown) are dropped — surfacing is best-effort.
    fn session_write_error(&self, failure: Option<&str>) {
        if let Some(msg) = failure {
            _ = self.send(AgentEvent::Error(format!("Session write failed: {msg}")));
        }
    }
}

// ── Stdio Sink (bare REPL / headless) ──

/// Writes agent events directly to stdout / stderr. Used by `--no-tui`
/// and `-p` headless mode.
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

impl AgentSink for StdioSink {
    fn send(&self, event: AgentEvent) -> Result<()> {
        use std::io::Write;

        match event {
            AgentEvent::StreamToken(text) => {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            AgentEvent::ThinkingToken(text) => {
                if self.show_thinking {
                    let mut stdout = std::io::stdout().lock();
                    write!(stdout, "\x1b[2m{text}\x1b[22m")?;
                    stdout.flush()?;
                }
            }
            AgentEvent::ToolCallStart { name, input, .. } => {
                let icon = self.tools.icon(&name);
                let label = self.tools.label(&name, &input);
                eprintln!("{icon} {label}");
            }
            AgentEvent::ToolCallEnd {
                content, metadata, ..
            } => {
                if let Some(title) = metadata.title {
                    eprintln!("  {title}");
                }
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    eprintln!("{trimmed}");
                }
                eprintln!();
            }
            AgentEvent::TurnComplete => {
                // Newline after streamed text.
                println!();
            }
            AgentEvent::Cancelled => {
                // Marker on stderr so captured stdout (`-p`) stays reproducible.
                println!();
                eprintln!("(interrupted)");
            }
            AgentEvent::SessionTitleUpdated(_) => {
                // Titles are a TUI-only affordance; the stdio sink has no
                // persistent header to rewrite.
            }
            AgentEvent::Error(msg) => {
                eprintln!("Error: {msg}");
            }
        }
        Ok(())
    }
}

// ── Test Fixtures ──

/// Collects every event the code under test sends so assertions can
/// inspect both the sequence and the payload. Shared by `agent` and
/// `session::title_generator` tests (both drive code that writes
/// through an [`AgentSink`]).
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

    // ── StdioSink::send ──
    //
    // `send` writes to stdout/stderr, which cargo's test harness captures and
    // discards on success — so these tests exercise the match-arm dispatch
    // and the Result contract rather than asserting on rendered bytes.
    // Formatting-assertion tests belong behind an extracted rendering helper
    // (see `docs/roadmap.md` → Test Coverage).

    fn test_sink(show_thinking: bool) -> StdioSink {
        StdioSink::new(show_thinking, Arc::new(ToolRegistry::new(Vec::new())))
    }

    #[test]
    fn send_session_title_updated_is_silent_ok() {
        // AI-generated titles are a TUI-only affordance; the stdio path has
        // no persistent header to rewrite, so the arm must no-op cleanly.
        let sink = test_sink(false);
        sink.send(AgentEvent::SessionTitleUpdated("New title".to_owned()))
            .unwrap();
    }

    #[test]
    fn send_stream_token_writes_body_without_error() {
        let sink = test_sink(false);
        sink.send(AgentEvent::StreamToken("hello".to_owned()))
            .unwrap();
    }

    #[test]
    fn send_thinking_token_respects_show_thinking_flag() {
        // show_thinking = false must swallow the block entirely, not just
        // strip the dim escape codes — otherwise the stream lines bleed
        // into the transcript unformatted.
        test_sink(false)
            .send(AgentEvent::ThinkingToken("muted".to_owned()))
            .unwrap();
        test_sink(true)
            .send(AgentEvent::ThinkingToken("visible".to_owned()))
            .unwrap();
    }

    #[test]
    fn send_tool_call_start_renders_label_and_falls_back_to_name() {
        let sink = test_sink(false);
        sink.send(AgentEvent::ToolCallStart {
            id: "t1".to_owned(),
            name: "unregistered".to_owned(),
            input: serde_json::Value::Null,
        })
        .unwrap();
    }

    #[test]
    fn send_tool_call_end_handles_every_field_nullability() {
        let sink = test_sink(false);
        sink.send(AgentEvent::ToolCallEnd {
            id: "t1".to_owned(),
            content: "file1\nfile2\n".to_owned(),
            is_error: false,
            metadata: crate::tool::ToolMetadata {
                title: Some("ls".to_owned()),
                ..crate::tool::ToolMetadata::default()
            },
        })
        .unwrap();
        sink.send(AgentEvent::ToolCallEnd {
            id: "t2".to_owned(),
            content: "   \n".to_owned(),
            is_error: true,
            metadata: crate::tool::ToolMetadata::default(),
        })
        .unwrap();
    }

    #[test]
    fn send_turn_complete_emits_trailing_newline_without_error() {
        test_sink(false).send(AgentEvent::TurnComplete).unwrap();
    }

    #[test]
    fn send_cancelled_emits_marker_without_error() {
        test_sink(false).send(AgentEvent::Cancelled).unwrap();
    }

    #[test]
    fn send_error_routes_message_to_stderr() {
        test_sink(false)
            .send(AgentEvent::Error("boom".to_owned()))
            .unwrap();
    }
}
