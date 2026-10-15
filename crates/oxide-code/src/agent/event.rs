//! Agent event types and the sink trait for delivering them to a UI.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::{CompactionConfig, Effort};
use crate::model::{ModelSpec, ResolvedModelId};
use crate::tool::ToolResultView;

// ── Visible Markers ──

/// Inline glyph appended to the chat (and stderr in `--no-tui` mode) when a turn is cancelled
/// mid-stream. Matched by the chat block layer to render as a dim italic separator.
pub(crate) const INTERRUPTED_MARKER: &str = "(interrupted)";

// ── Agent Events ──

/// Token and cost snapshot for status-line rendering.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UsageSnapshot {
    /// Cache-aware input total: `input + cache_creation + cache_read`. Drives the context-pressure
    /// percentage and auto-compaction threshold. `None` after a compaction (manual `/compact` or
    /// automatic) until the next request reports usage, which hides the context segment.
    pub(crate) context_tokens: Option<u32>,
    /// Active model context window. `None` hides the percentage.
    pub(crate) context_window: Option<u32>,
    /// Running session cost in USD, accumulated across every request, summaries included. Cleared
    /// on session roll (`/clear`) and `/resume`. `None` hides the cost segment.
    pub(crate) estimated_cost_usd: Option<f64>,
}

/// One-way notifications from the engine to the frontend reading its
/// [`EngineHandle`](crate::engine::EngineHandle).
#[derive(Debug, Clone)]
pub(crate) enum AgentEvent {
    /// Assistant text chunk to append to the in-flight reply.
    StreamToken(String),
    /// Extended-thinking chunk rendered separately from `StreamToken` and only when the user
    /// has thinking display enabled.
    ThinkingToken(String),
    /// Progress note the model wrote before a tool call, emitted as that call's block starts
    /// streaming. Sinks drop it when thinking display is on, since the same text already streamed
    /// as a [`Self::ThinkingToken`].
    ProgressUpdate(String),
    /// A request failed before any content and will be sent again after a delay. Leaves the turn
    /// running and renders as a dim notice.
    Retrying(String),
    /// Tool call emitted by the model.
    ToolCallStart(ToolCall),
    /// Result of the call the matching [`Self::ToolCallStart`] opened.
    ToolCallEnd(ToolResult),
    /// A [`UserAction::QueuePrompt`] was spliced into the live transcript, so the UI moves it from
    /// the queue into the chat.
    PromptDrained(String),
    /// Turn ended cleanly with a final assistant reply (no further tool rounds).
    TurnComplete,
    /// The in-flight prompt, `/compact`, or `/resume` failed. Like [`Self::TurnComplete`] and
    /// [`Self::Cancelled`], it ends the operation.
    TurnFailed(String),
    /// Live usage and estimated cost snapshot for the current process.
    UsageUpdated(UsageSnapshot),
    /// User cancelled mid-turn ([`UserAction::Cancel`]). The in-flight reply is truncated and the
    /// inline [`INTERRUPTED_MARKER`] is rendered.
    Cancelled,
    /// Automatic compaction started before the submitted prompt runs. TUI switches to compacting
    /// status while the summarizer request streams.
    AutoCompactionStarted,
    /// Background title generator finished, so the UI updates the chrome label.
    SessionTitleUpdated { session_id: String, title: String },
    /// `/clear` or `/archive` rolled the session, making a new session UUID active. `archived`
    /// marks an `/archive` roll, so the UI can name the session it archived.
    SessionRolled { id: String, archived: bool },
    /// `/resume` swapped to an existing session in place. Payload carries the target's transcript
    /// projected for display, so the UI can rebuild chat without restarting the process.
    /// `unarchived` reports that the target stayed archived until this resume.
    SessionResumed {
        id: String,
        title: Option<String>,
        unarchived: bool,
        history: Vec<Interaction>,
    },
    /// `/compact` finished. The in-memory transcript has already been swapped to the synthetic
    /// continuation, so the UI clears its chat and replays a single boundary block. `automatic`
    /// is true when the compact ran ahead of a submitted prompt, in which case the TUI keeps the
    /// busy state since the next turn starts immediately.
    SessionCompacted {
        summary: String,
        pre_count: u32,
        instructions: Option<String>,
        automatic: bool,
    },
    /// Live config after a [`UserAction::SwapConfig`]. `effort` is the resolved value.
    /// `requested_effort` is the user's pick, surfaced when an unknown model drops it.
    ConfigChanged {
        model: ModelSpec,
        effort: Option<Effort>,
        compaction: CompactionConfig,
        requested_effort: Option<Effort>,
    },
    /// User-visible error that leaves any in-flight turn running, such as a session write failure.
    /// Renders as a dedicated chat block.
    Error(String),
    /// A gated tool call needs the user's approval before it runs. Carries the tool-use id so a
    /// stale decision for a dropped call is ignored, plus a small preview for the modal to render.
    /// The agent loop blocks on the matching [`UserAction::ApprovalDecision`].
    ApprovalRequested {
        id: String,
        preview: ApprovalPreview,
    },
}

// ── Tool Calls ──

/// A tool call as the row a UI shows for it, live in [`AgentEvent::ToolCallStart`] and resumed in
/// [`Interaction::ToolCall`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) icon: String,
    pub(crate) label: String,
}

/// A tool call's result, live in [`AgentEvent::ToolCallEnd`] and resumed in
/// [`Interaction::ToolResult`]. `content` is what the model sees, `title` the tool's own header for
/// the result, and `view` its structured body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResult {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) title: Option<String>,
    pub(crate) view: ToolResultView,
}

// ── History ──

/// A unit of a resumed transcript as a frontend shows it, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Interaction {
    /// The boundary a compaction left, which the rest of the transcript continues from. Renders
    /// like a live [`AgentEvent::SessionCompacted`].
    Compacted {
        summary: String,
        pre_count: u32,
        instructions: Option<String>,
    },
    UserText(String),
    AssistantText(String),
    AssistantThinking(String),
    /// Note introducing the next tool call. Its text also appears in the preceding
    /// [`Self::AssistantThinking`], so renderers show one or the other.
    ProgressUpdate(String),
    ToolCall(ToolCall),
    /// A result that follows its call, or stands alone when the transcript lost the call.
    ToolResult(ToolResult),
}

// ── Approval ──

/// What a pending approval shows the user. `Clone` and small, since it rides the bounded event
/// channel. The body is pre-rendered by the tool's gate seam so the modal stays display-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalPreview {
    /// Tool display label, e.g. `Bash(cargo test)` or `Write(src/main.rs)`.
    pub(crate) title: String,
    /// Body lines: an edit / write diff or the command string, capped to a display height when the
    /// modal flattens it for render.
    pub(crate) body: ApprovalBody,
}

/// The renderable body of an [`ApprovalPreview`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalBody {
    /// A shell command string (`bash`).
    Command(String),
    /// A unified diff (`edit` / `write`), reusing the chat's `+` / `-` chunk shape.
    Diff(Vec<crate::tool::DiffChunk>),
}

/// The user's verdict on a pending approval, carried back on [`UserAction::ApprovalDecision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Approve,
    Deny,
}

// ── User Actions ──

/// Commands from the UI to the agent loop. Multiplexed onto a single `mpsc` so the loop can race
/// them against in-flight stream / tool futures with one biased `select!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UserAction {
    /// Starts a turn with this prompt.
    SubmitPrompt(String),
    /// A prompt typed while a turn runs. The turn splices it in at its next round boundary and
    /// confirms with [`AgentEvent::PromptDrained`]. The UI keeps every unconfirmed prompt and
    /// submits it later, so the agent drops one that arrives after its turn ended.
    QueuePrompt(String),
    Clear,
    /// Bare `/archive`: archive the live session, then roll to a fresh one like [`Self::Clear`].
    Archive,
    /// `/resume <id-prefix>` or picker selection: swap to the session file at `path` in place.
    Resume {
        path: PathBuf,
    },
    /// `/compact [instructions]`: stream a summarization request, replace the in-memory
    /// transcript with a synthetic continuation, persist a `Compact` JSONL boundary.
    Compact {
        instructions: Option<String>,
    },
    /// Manual title from `/rename` or its modal. Suppresses AI title generation for this session.
    Rename {
        title: String,
    },
    /// Symmetric model + effort swap. At least one field must be `Some`. `None` leaves that axis
    /// as-is. Modal pickers and typed-arg `/model <id>` / `/effort <tier>` both flow through here.
    SwapConfig {
        model: Option<ResolvedModelId>,
        effort: Option<Effort>,
    },
    /// TUI-only: live-preview the picker's highlighted theme without committing it. Cursor moves
    /// in the `/theme` modal emit this, and cancelling the modal restores the open-time snapshot.
    PreviewTheme {
        name: String,
    },
    /// TUI-only: commit a theme swap for the rest of the session. Restart returns to user config.
    SwapTheme {
        name: String,
    },
    /// The user's verdict on the outstanding [`AgentEvent::ApprovalRequested`]. The `id` is matched
    /// against the blocked call so a decision for an already-dropped call is ignored.
    ApprovalDecision {
        id: String,
        decision: ApprovalDecision,
    },
    Cancel,
    /// TUI-only. Agent loop ignores this.
    ConfirmExit,
    Quit,
}

// ── Agent Sink ──

/// Transport from the agent loop to a UI. Implementations must be cheap to clone-by-`&` (the
/// loop calls `send` on a hot path) and tolerate dropped events without panicking. Call sites use
/// [`Self::emit`] for one-shot or user-facing events (logs on drop). Raw [`Self::send`] with
/// `_ = ` is reserved for per-token streaming where dropping a single event is harmless.
pub(crate) trait AgentSink: Send + Sync {
    fn send(&self, event: AgentEvent) -> Result<()>;

    /// Sends `event` and logs at `error!` if the channel rejects it. Use for any signal whose
    /// loss would leave the UI inconsistent (errors, cancellation, turn completion, state changes).
    fn emit(&self, event: AgentEvent, label: &'static str) {
        if let Err(e) = self.send(event) {
            tracing::error!("{label} event dropped: {e}");
        }
    }

    /// Surfaces a session-writer failure as a single user-visible [`AgentEvent::Error`].
    /// No-op when `failure` is `None`.
    fn session_write_error(&self, failure: Option<&str>) {
        if let Some(msg) = failure {
            self.emit(
                AgentEvent::Error(format!("Session write failed: {msg}")),
                "session-write-failed",
            );
        }
    }
}

// ── Stdio Sink (bare REPL / headless) ──

/// [`AgentSink`] for the `--no-tui` REPL and `--prompt` headless modes: assistant tokens stream
/// to stdout, everything else (tool calls, errors, the interrupted marker) to stderr so piped
/// output stays clean.
pub(crate) struct StdioSink {
    show_thinking: bool,
}

impl StdioSink {
    pub(crate) fn new(show_thinking: bool) -> Self {
        Self { show_thinking }
    }

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
            AgentEvent::ProgressUpdate(text) => {
                if !self.show_thinking {
                    writeln!(stderr, "{}", text.trim())?;
                }
            }
            AgentEvent::ToolCallStart(ToolCall { icon, label, .. }) => {
                writeln!(stderr, "{icon} {label}")?;
            }
            AgentEvent::ToolCallEnd(ToolResult { content, title, .. }) => {
                if let Some(title) = title {
                    writeln!(stderr, "  {title}")?;
                }
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    writeln!(stderr, "{trimmed}")?;
                }
                writeln!(stderr)?;
            }
            AgentEvent::SessionCompacted {
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
            // TUI-only — no stdio surface to update.
            AgentEvent::PromptDrained(_)
            | AgentEvent::UsageUpdated(_)
            | AgentEvent::AutoCompactionStarted
            | AgentEvent::SessionTitleUpdated { .. }
            | AgentEvent::SessionRolled { .. }
            | AgentEvent::SessionResumed { .. }
            | AgentEvent::ConfigChanged { .. }
            | AgentEvent::ApprovalRequested { .. } => {}
            AgentEvent::TurnComplete => {
                writeln!(stdout)?;
            }
            AgentEvent::Cancelled => {
                writeln!(stdout)?;
                writeln!(stderr, "{INTERRUPTED_MARKER}")?;
            }
            AgentEvent::Retrying(msg) => {
                writeln!(stderr, "{msg}")?;
            }
            AgentEvent::Error(msg) | AgentEvent::TurnFailed(msg) => {
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

/// Channel pair whose `recv()` stays pending forever.
#[cfg(test)]
pub(crate) fn inert_user_action_channel() -> (
    tokio::sync::mpsc::Sender<UserAction>,
    tokio::sync::mpsc::Receiver<UserAction>,
) {
    tokio::sync::mpsc::channel(1)
}

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

/// An [`AgentSink`] whose `send` always fails, for exercising the fail-closed paths that depend on
/// a control-plane event reaching the UI.
#[cfg(test)]
pub(crate) struct FailingSink;

#[cfg(test)]
impl AgentSink for FailingSink {
    fn send(&self, _event: AgentEvent) -> Result<()> {
        anyhow::bail!("sink closed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── StdioSink::render ──

    fn test_sink(show_thinking: bool) -> StdioSink {
        StdioSink::new(show_thinking)
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
        let (stdout, stderr) = render_one(
            &test_sink(false),
            AgentEvent::StreamToken("hello".to_owned()),
        );
        assert_eq!(stdout, "hello");
        assert_eq!(stderr, "");
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
        assert_eq!(stdout, "");
        assert_eq!(stderr, "");
    }

    #[test]
    fn render_progress_update_writes_to_stderr_only_when_thinking_hidden() {
        let event = || AgentEvent::ProgressUpdate("Found the loader.\n".to_owned());
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
            AgentEvent::ToolCallStart(ToolCall {
                id: "t1".to_owned(),
                name: "bash".to_owned(),
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
            AgentEvent::ToolCallEnd(text_result("t1", "file1\nfile2\n", false, Some("Listed"))),
        );
        assert_eq!(stderr, "  Listed\nfile1\nfile2\n\n");
    }

    #[test]
    fn render_tool_call_end_without_title_skips_header_and_whitespace_content() {
        let (_, stderr) = render_one(
            &test_sink(false),
            AgentEvent::ToolCallEnd(text_result("t2", "   \n", true, None)),
        );
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn render_tui_only_events_emit_nothing_on_either_stream() {
        for event in [
            AgentEvent::PromptDrained("queued".to_owned()),
            AgentEvent::AutoCompactionStarted,
            AgentEvent::SessionTitleUpdated {
                session_id: "sid".to_owned(),
                title: "New title".to_owned(),
            },
            AgentEvent::SessionRolled {
                id: "rolled".to_owned(),
                archived: true,
            },
            AgentEvent::ConfigChanged {
                model: ModelSpec::resolve("claude-opus-5-5".to_owned()),
                effort: Some(Effort::Xhigh),
                compaction: CompactionConfig::disabled(),
                requested_effort: Some(Effort::Xhigh),
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
            AgentEvent::SessionCompacted {
                summary: "summary".to_owned(),
                pre_count: 4,
                instructions: None,
                automatic: false,
            },
        );
        assert_eq!(stderr, "Compacted 4 messages into summary\n");

        let (_, stderr) = render_one(
            &test_sink(false),
            AgentEvent::SessionCompacted {
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
        let (stdout, stderr) = render_one(&test_sink(false), AgentEvent::TurnComplete);
        assert_eq!(stdout, "\n");
        assert_eq!(stderr, "");
    }

    #[test]
    fn render_cancelled_writes_marker_to_stderr_and_blank_to_stdout() {
        let (stdout, stderr) = render_one(&test_sink(false), AgentEvent::Cancelled);
        assert_eq!(stdout, "\n");
        assert_eq!(stderr.trim(), INTERRUPTED_MARKER);
    }

    #[test]
    fn render_retrying_writes_the_notice_to_stderr() {
        let notice = "HTTP 529 overloaded_error. Retrying in 1s (retry 1 of 5).";
        let (stdout, stderr) =
            render_one(&test_sink(false), AgentEvent::Retrying(notice.to_owned()));
        assert!(stdout.is_empty(), "{stdout:?}");
        assert_eq!(stderr, format!("{notice}\n"));
    }

    #[test]
    fn render_error_and_turn_failure_prefix_with_error_label() {
        let (_, stderr) = render_one(&test_sink(false), AgentEvent::Error("boom".to_owned()));
        assert_eq!(stderr, "Error: boom\n");
        let (_, stderr) = render_one(&test_sink(false), AgentEvent::TurnFailed("bust".to_owned()));
        assert_eq!(stderr, "Error: bust\n");
    }
}
