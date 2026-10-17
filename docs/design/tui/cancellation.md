# Cancellation and Queued Input

Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, and mid-turn queued user prompts.

## Implementation

The TUI exposes these controls and behaviors:

- **Cancel and exit.** Esc / Ctrl+C cancels active work. Dual-press Ctrl+C exits from idle, and Ctrl+D exits when idle with an empty input buffer.
- **Queue input.** Enter while busy appends to `App::pending_prompts: VecDeque<String>`, rendered as a dim preview above the input, and forwards the prompt as `Op::Queue`.
- **Drain input.** `agent_turn` splices each forwarded prompt into `messages` as a trailing User message at its next round boundary and confirms it with `Event::PromptDrained`, so the model sees new instructions in the next API request without aborting in-flight work. A turn that ends first drops the prompts it still holds, and once it completes or fails the TUI submits the queue head as the next turn. A prompt whose automatic compaction failed was never sent, so the TUI hands it back to the user and leaves the queue waiting.
- **Interrupt work.** Cancellation drops the `agent_turn` future: reqwest closes the SSE stream, the bash tool kills the command's process tree, and a dim italic `(interrupted)` marker lands in chat. The transcript then ends on a `[Request interrupted by user]` user message, so the model knows the user abandoned that prompt.
- **Show status.** The status bar colocates the active-key hint with the spinner: `Streaming · Esc to interrupt` / `Running {tool} · Esc to interrupt` / `Cancelling` / `Press Ctrl+C again to exit`.

## Design Decisions

The picks lean toward the Codex precedent on most surfaces, since it is the one Rust reference and its patterns map onto our async / actor architecture without reshaping. The wire-shape choice (3) is the deliberate exception.

1. **Drop-the-future cancellation.** Esc / Ctrl+C drops the `agent_turn` future from the agent loop, which is simpler than Codex's protocol message. The `Tool::run` contract requires each tool to stop its work when the future drops, or to finish it together with its bookkeeping. reqwest closes the SSE stream on drop, and the bash tool's `ShellTree` kills the whole process tree, so backgrounded descendants stop with the shell. Edit and Write run each file write and its tracker record in one spawned task, so a write that lands after the cancel is still recorded.

2. **Commit only settled state on cancel.** A cancel mid-stream discards the partial assistant message, as Codex does. The TUI keeps the streamed prefix with an `(interrupted)` suffix. A cancel during a tool round commits the assistant message with a result for every call, because the finished calls already changed the workspace and the model must see them. Each unfinished call gets an `is_error` result saying the user interrupted it, so the transcript never holds an orphan `tool_use`. The result for a call stopped mid-run says it may have run partially, and the one for a call that never started, including one awaiting approval, says it did not run.

   The turn then ends on a `[Request interrupted by user]` user message after whatever it committed, and a shutdown mid-turn ends the same way. Without the marker, the cancelled prompt and the next one reach the model as two unanswered prompts, and it answers both. Claude Code records the same text for a cancel mid-stream and a `for tool use` variant after a tool round. oxide-code uses one text for both, since the round's results already mark each interrupted call. The prompt itself stays in the transcript, since a tool round may already have acted on it and the chat still shows it. The marker is an ordinary message, so the session file and `--continue` replay it unchanged, and a resumed chat shows it as the `(interrupted)` line.

3. **Mid-turn drain at round boundary, raw consecutive User messages on the wire.** Each queued prompt becomes its own `User` message with one `Text` block. Anthropic accepts consecutive same-role messages. oxide-code deliberately drops the `<system-reminder>` envelope that Claude Code and opencode wrap with, which keeps persistence trivial.

4. **The TUI owns the queue.** A queued prompt leaves `App::pending_prompts` only when a matching `PromptDrained` arrives or when it starts a turn of its own. `App::forwarded_prompts` counts the leading entries the in-flight turn holds, and a drain pops the head only when it names that head. While `Status::Compacting`, `Status::SigningIn`, `Status::Resuming`, or `Status::Cancelling`, no turn may be left to splice prompts in, so the TUI holds new submits and forwards them in order once a turn resumes streaming. The idle engine discards a late `Op::Queue`, since the TUI submits it again.

5. **A cancel leaves the queue to the user.** After a cancel the TUI goes idle without submitting the queue head, even when the turn completed before the cancel landed. Esc on idle pops the most recent queued prompt into the textarea for editing, gated on the buffer being empty so a draft is never silently overwritten. The next submit joins the back of the queue and starts a turn with its head, so prompts keep the order they were typed in.

6. **`Status` is projected from `App` state.** The variants (`Idle | Streaming | ToolRunning { name } | Compacting | SigningIn | Resuming | Cancelling | ExitArmed { until }`) derive from app fields, so `set_active_status` can short-circuit while `Cancelling` / `ExitArmed` without a parallel state to keep in sync.

7. **Cancellation rides the existing `Event` channel.** `Event::Cancelled` covers both the TUI and `StdioSink`. No new event channel.

8. **One terminal event per operation.** A prompt ends with exactly one of `TurnComplete`, `TurnFailed`, or `Cancelled`. `/compact` ends with `SessionCompacted`, `TurnFailed`, or `Cancelled`, and `/resume` with `SessionResumed` or `TurnFailed`. Only these run the TUI teardown that submits the queue head. `/clear` ends with `SessionRolled`, `/rename` with `SessionTitleUpdated` or `RenameFailed`, and `/model` and `/effort` with `ConfigChanged`, which counts as a failure when the model has no tier for an `/effort` pick. `TurnEnd::of` maps an op and an event to how the op ended, so the REPL and headless drivers stop rendering at the end of the op they sent. `Error` is a notice for failures that leave the operation running, such as a failed session write, and `Retrying` is a dim notice for a request the client sends again.

## Sources

- `crates/oxide-code/src/driver/stdio.rs`: `StdioSink::render`, and `render_op`, which renders until the op it sent ends.
- `crates/oxide-code/src/engine.rs`: `Engine::run`, which ignores an `Op::Queue` that arrives between turns, plus `Engine::submit_prompt`, `report_abort`, and `TurnEnd::of`, shared by the TUI, REPL, and headless drivers.
- `crates/oxide-code/src/engine/abort.rs`: `TurnAbort` enum, `await_unless_aborted`.
- `crates/oxide-code/src/engine/dispatch.rs`: `run_tool_round` / `ToolRound`, `dispatch_tool_call`.
- `crates/oxide-code/src/engine/turn.rs`: `agent_turn` round loop, `interrupt_turn`, `record_drained_prompts`.
- `crates/oxide-code/src/file_tracker.rs`: `FileTracker::write_and_record`.
- `crates/oxide-code/src/message.rs`: `Message::interruption`, the marker a user abort records.
- `crates/oxide-code/src/protocol.rs`: `Event::{Cancelled, PromptDrained, TurnFailed}`, `Interaction::Interrupted`, `Op::{Interrupt, Queue, Shutdown}`.
- `crates/oxide-code/src/session/history.rs`: `walk_transcript`, which shows a resumed interruption marker as `Interaction::Interrupted`.
- `crates/oxide-code/src/tool.rs`: `Tool::run` drop contract, `write_tracked`.
- `crates/oxide-code/src/tool/bash.rs`: `ShellTree`, which kills the shell's process group (Unix) or Job Object (Windows) when a call times out or drops before finishing.
- `crates/oxide-code/src/tui/action.rs`: `Action::ConfirmExit`.
- `crates/oxide-code/src/tui/app.rs`: `App::pending_prompts` / `forwarded_prompts`, and `App::active_prompt` / `auto_compacting`.
- `crates/oxide-code/src/tui/app/dispatch.rs`: `dispatch_op` / `start_turn` / `forward_queued_prompts`, `expire_armed_exit`.
- `crates/oxide-code/src/tui/app/events.rs`: `finish_turn`, `return_unsent_prompt`, `finalize_idle` / `enter_idle` / `drain_pending_prompt`, `set_active_status`.
- `crates/oxide-code/src/tui/components/chat.rs`: `ChatView::clear_pending_calls`, which evicts orphan tool calls at turn boundaries, and `ChatView::load_history`, which shows a resumed interruption as the `(interrupted)` line.
- `crates/oxide-code/src/tui/components/chat/blocks/interrupted.rs`: `InterruptedMarker` block.
- `crates/oxide-code/src/tui/components/input.rs`: Ctrl+D POSIX gate, Esc-pop, `set_text`.
- `crates/oxide-code/src/tui/components/status.rs`: `Status` variants, busy hints, spinner.
- `crates/oxide-code/src/tui/glyphs.rs`: `INTERRUPTED_MARKER`.
