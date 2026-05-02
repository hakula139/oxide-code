use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::tool::ToolRegistry;

// ── Visible Markers ──

/// User-facing string surfaced when a turn is dropped via
/// [`UserAction::Cancel`]. Both the TUI's [`InterruptedMarker`] block
/// and [`StdioSink::send`] emit this verbatim, so they stay in sync.
///
/// [`InterruptedMarker`]: crate::tui::components::chat::blocks::InterruptedMarker
pub(crate) const INTERRUPTED_MARKER: &str = "(interrupted)";

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
    /// A queued mid-turn submit was spliced into the conversation as a
    /// trailing user message at the round boundary. The TUI pops the
    /// matching head from its preview queue (FIFO) and renders the
    /// drained prompt as a regular user message in chat history.
    PromptDrained(String),
    /// The current assistant turn finished cleanly (text-only response,
    /// no more tool calls). Distinct from [`crate::agent::TurnAbort`],
    /// which carries every *early-exit* reason (cancel, quit, failure)
    /// internally inside [`crate::agent::agent_turn`]'s `Result`. Sinks
    /// see only display-facing events; the abort enum stays inside the
    /// agent loop.
    TurnComplete,
    /// Mid-flight turn was dropped in response to a [`UserAction::Cancel`].
    /// Same teardown as [`Self::TurnComplete`] plus an `(interrupted)`
    /// marker on the partial assistant block.
    Cancelled,
    /// A newly-generated session title (e.g., AI-generated via Haiku).
    /// `session_id` scopes the event to the session that produced it —
    /// the TUI ignores titles for sessions other than its current one,
    /// so a slow Haiku call straddling a `/clear` doesn't paint the
    /// old title onto the new session. Stdio sinks ignore it.
    SessionTitleUpdated { session_id: String, title: String },
    /// `/clear` rolled the session — `id` is the new UUID. The TUI
    /// updates `session_info.session_id` and clears the (now-stale) AI
    /// title; other sinks ignore it.
    SessionRolled { id: String },
    /// A fatal error from the API or agent loop.
    Error(String),
}

// ── User Actions ──

/// Actions from the user that the agent loop consumes.
#[derive(Debug, Clone)]
pub(crate) enum UserAction {
    /// Submit a prompt to the agent.
    SubmitPrompt(String),
    /// `/clear` — agent loop finalizes the old session, swaps in a
    /// fresh one, and emits [`AgentEvent::SessionRolled`].
    Clear,
    /// Cancel the in-flight turn. No-op when the agent is idle.
    Cancel,
    /// Idle Ctrl+C — arm a 1-second exit confirmation in the TUI; a
    /// second arm within the window flips to [`Self::Quit`]. The agent
    /// loop ignores this variant; only the TUI consumes it.
    ConfirmExit,
    /// Hard quit (Ctrl+D, or confirmed exit). Both the TUI and the
    /// agent loop tear down on this.
    Quit,
}

/// `UserAction` channel pair where `recv()` stays pending forever.
/// Used by display modes (bare REPL, headless) that have no in-process
/// source of `UserAction`s — the caller holds the returned sender on
/// its stack so [`agent_turn`](crate::agent::agent_turn)'s race against
/// `user_rx.recv()` blocks indefinitely until something else (e.g.
/// `shutdown_signal`) drops the turn future externally.
pub(crate) fn inert_user_action_channel() -> (mpsc::Sender<UserAction>, mpsc::Receiver<UserAction>)
{
    mpsc::channel(1)
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

impl StdioSink {
    /// Write `event` to the supplied byte sinks. Extracted so tests
    /// can pass `Vec<u8>` and assert on rendered bytes; production
    /// passes locked stdout / stderr.
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
            | AgentEvent::SessionRolled { .. } => {}
            AgentEvent::TurnComplete => {
                // Newline after streamed text.
                writeln!(stdout)?;
            }
            AgentEvent::Cancelled => {
                // Marker on stderr so captured stdout (`-p`) stays reproducible.
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

    // ── StdioSink::render ──

    fn test_sink(show_thinking: bool) -> StdioSink {
        StdioSink::new(show_thinking, Arc::new(ToolRegistry::new(Vec::new())))
    }

    /// Capture stdout / stderr bytes for one event so assertions can
    /// pin the exact rendered shape (not just `Ok(())`).
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
        // Unregistered tool name exercises the label fallback path.
        let (_, stderr) = render_one(
            &test_sink(false),
            AgentEvent::ToolCallStart {
                id: "t1".to_owned(),
                name: "unregistered".to_owned(),
                input: serde_json::Value::Null,
            },
        );
        assert!(stderr.ends_with('\n'));
        // Generic icon + tool name fallback when registry doesn't know
        // the tool — the exact icon depends on ToolRegistry's default
        // but the stderr must non-emptily render *something* on one line.
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
        // Trailing blank separator between tool blocks.
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
        // No title and whitespace-only content — only the trailing
        // separator newline lands on stderr.
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn render_prompt_drained_and_session_title_are_silent() {
        for event in [
            AgentEvent::PromptDrained("queued".to_owned()),
            AgentEvent::SessionTitleUpdated {
                session_id: "sid".to_owned(),
                title: "New title".to_owned(),
            },
        ] {
            let (stdout, stderr) = render_one(&test_sink(false), event);
            assert!(stdout.is_empty());
            assert!(stderr.is_empty());
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

    // ── StdioSink::send ──

    #[test]
    fn send_prompt_drained_accepts_tui_only_event() {
        test_sink(false)
            .send(AgentEvent::PromptDrained("queued".to_owned()))
            .unwrap();
    }
}
