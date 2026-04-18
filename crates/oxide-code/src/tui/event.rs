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

/// Capacity of the bounded agent-event channel.
///
/// `StreamToken` events fire once per delta (roughly ~30-60/s during a
/// response), so this gives tens of seconds of headroom if the TUI
/// momentarily stalls on terminal I/O. A bound prevents the queue from
/// growing unboundedly when the TUI is completely wedged, and errors out
/// loudly instead of silently consuming memory.
pub(crate) const AGENT_EVENT_CHANNEL_CAP: usize = 4096;

/// Sends agent events through an `mpsc` channel for TUI consumption.
pub(crate) struct ChannelSink {
    tx: mpsc::Sender<AgentEvent>,
}

impl ChannelSink {
    pub(crate) fn new(tx: mpsc::Sender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl AgentSink for ChannelSink {
    fn send(&self, event: AgentEvent) -> Result<()> {
        use mpsc::error::TrySendError;
        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                anyhow::bail!(
                    "agent event channel is full (capacity {AGENT_EVENT_CHANNEL_CAP}); \
                     TUI is not draining events fast enough"
                )
            }
            Err(TrySendError::Closed(_)) => anyhow::bail!("TUI channel closed"),
        }
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

/// Returns a per-tool icon character for display.
pub(crate) fn tool_call_icon(name: &str) -> &'static str {
    match name {
        "bash" => "$",
        "read" => "→",
        "write" => "←",
        "edit" => "✎",
        "glob" => "✱",
        "grep" => "⌕",
        _ => "⟡",
    }
}

/// Creates a linked channel pair: the `ChannelSink` for the agent loop, and
/// the bounded `Receiver` for the TUI.
pub(crate) fn channel() -> (ChannelSink, mpsc::Receiver<AgentEvent>) {
    let (tx, rx) = mpsc::channel(AGENT_EVENT_CHANNEL_CAP);
    (ChannelSink::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // ── tool_call_title ──

    #[test]
    fn tool_call_title_bash_extracts_command() {
        let input = json!({"command": "ls -la"});
        assert_eq!(tool_call_title("bash", &input), Some("ls -la"));
    }

    #[test]
    fn tool_call_title_read_extracts_file_path() {
        let input = json!({"file_path": "/tmp/foo.rs"});
        assert_eq!(tool_call_title("read", &input), Some("/tmp/foo.rs"));
    }

    #[test]
    fn tool_call_title_write_extracts_file_path() {
        let input = json!({"file_path": "/tmp/out.txt", "content": "hello"});
        assert_eq!(tool_call_title("write", &input), Some("/tmp/out.txt"));
    }

    #[test]
    fn tool_call_title_edit_extracts_file_path() {
        let input = json!({"file_path": "src/main.rs"});
        assert_eq!(tool_call_title("edit", &input), Some("src/main.rs"));
    }

    #[test]
    fn tool_call_title_glob_extracts_pattern() {
        let input = json!({"pattern": "**/*.rs"});
        assert_eq!(tool_call_title("glob", &input), Some("**/*.rs"));
    }

    #[test]
    fn tool_call_title_grep_extracts_pattern() {
        let input = json!({"pattern": "TODO"});
        assert_eq!(tool_call_title("grep", &input), Some("TODO"));
    }

    #[test]
    fn tool_call_title_unknown_tool_returns_none() {
        let input = json!({"foo": "bar"});
        assert_eq!(tool_call_title("unknown", &input), None);
    }

    #[test]
    fn tool_call_title_missing_key_returns_none() {
        let input = json!({"other_field": "value"});
        assert_eq!(tool_call_title("bash", &input), None);
    }

    #[test]
    fn tool_call_title_non_string_value_returns_none() {
        let input = json!({"command": 42});
        assert_eq!(tool_call_title("bash", &input), None);
    }

    // ── tool_call_icon ──

    #[test]
    fn tool_call_icon_known_tools() {
        assert_eq!(tool_call_icon("bash"), "$");
        assert_eq!(tool_call_icon("read"), "→");
        assert_eq!(tool_call_icon("write"), "←");
        assert_eq!(tool_call_icon("edit"), "✎");
        assert_eq!(tool_call_icon("glob"), "✱");
        assert_eq!(tool_call_icon("grep"), "⌕");
    }

    #[test]
    fn tool_call_icon_unknown_tool_returns_default() {
        assert_eq!(tool_call_icon("unknown"), "⟡");
        assert_eq!(tool_call_icon(""), "⟡");
    }

    // ── channel ──

    #[tokio::test]
    async fn channel_sink_delivers_events() {
        let (sink, mut rx) = channel();
        sink.send(AgentEvent::StreamToken("hello".to_owned()))
            .unwrap();
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, AgentEvent::StreamToken(s) if s == "hello"));
    }

    #[test]
    fn channel_sink_closed_receiver_returns_error() {
        let (sink, rx) = channel();
        drop(rx);
        assert!(sink.send(AgentEvent::TurnComplete).is_err());
    }
}
