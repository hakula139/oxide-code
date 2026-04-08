use anyhow::Result;
use tokio::sync::mpsc;

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
            reason = "carried for structural completeness; not yet read by any consumer"
        )]
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool call has finished.
    ToolCallEnd {
        #[expect(
            dead_code,
            reason = "carried for structural completeness; not yet read by any consumer"
        )]
        id: String,
        title: Option<String>,
        content: String,
        #[expect(
            dead_code,
            reason = "carried for structural completeness; not yet read by any consumer"
        )]
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

/// Abstraction over where agent events are sent.
///
/// - [`ChannelSink`] sends events to the TUI via an async channel.
/// - [`StdioSink`] writes directly to stdout / stderr for the bare REPL.
///
/// This keeps the agent loop DRY — the same code drives both display modes.
pub(crate) trait AgentSink: Send + Sync {
    fn send(&self, event: AgentEvent) -> Result<()>;
}

// ── Channel Sink (TUI) ──

/// Sends agent events through an `mpsc` channel for TUI consumption.
pub(crate) struct ChannelSink {
    tx: mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelSink {
    pub(crate) fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl AgentSink for ChannelSink {
    fn send(&self, event: AgentEvent) -> Result<()> {
        self.tx
            .send(event)
            .map_err(|_| anyhow::anyhow!("TUI channel closed"))
    }
}

// ── Stdio Sink (bare REPL / headless) ──

/// Writes agent events directly to stdout / stderr. Used by `--no-tui`
/// and `-p` headless mode.
pub(crate) struct StdioSink {
    show_thinking: bool,
}

impl StdioSink {
    pub(crate) fn new(show_thinking: bool) -> Self {
        Self { show_thinking }
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
                if let Some(title) = tool_call_title(&name, &input) {
                    eprintln!("⟡ {name}: {title}");
                } else {
                    eprintln!("⟡ {name}");
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

/// Returns the display title for a tool call start event.
///
/// Each tool type extracts the most relevant field from its input for a
/// concise one-line summary.
pub(crate) fn tool_call_title<'a>(name: &str, input: &'a serde_json::Value) -> Option<&'a str> {
    let key = match name {
        "bash" => "command",
        "read" | "write" | "edit" => "file_path",
        "glob" | "grep" => "pattern",
        _ => return None,
    };
    input.get(key).and_then(serde_json::Value::as_str)
}

/// Creates a linked channel pair: the `ChannelSink` for the agent loop, and
/// the `UnboundedReceiver` for the TUI.
pub(crate) fn channel() -> (ChannelSink, mpsc::UnboundedReceiver<AgentEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ChannelSink::new(tx), rx)
}
