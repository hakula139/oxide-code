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
    /// A tool call has started execution.
    ToolCallStart {
        #[expect(
            dead_code,
            reason = "carried for structural completeness; no consumer reads this field"
        )]
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool call has finished.
    ToolCallEnd {
        #[expect(
            dead_code,
            reason = "carried for structural completeness; no consumer reads this field"
        )]
        id: String,
        title: Option<String>,
        content: String,
        is_error: bool,
    },
    /// The current assistant turn is complete (text-only response, no more
    /// tool calls).
    TurnComplete,
    /// A fatal error from the API or agent loop.
    Error(String),
}

// ── User Actions ──

/// Actions from the user that the agent loop consumes.
#[derive(Debug, Clone)]
pub(crate) enum UserAction {
    /// Submit a prompt to the agent.
    SubmitPrompt(String),
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
                if let Some(title) = self.tools.summarize_input(&name, &input) {
                    eprintln!("{icon} {name}: {title}");
                } else {
                    eprintln!("{icon} {name}");
                }
            }
            AgentEvent::ToolCallEnd { title, content, .. } => {
                if let Some(title) = title {
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
            AgentEvent::Error(msg) => {
                eprintln!("Error: {msg}");
            }
        }
        Ok(())
    }
}
