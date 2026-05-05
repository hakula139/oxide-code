# Cancellation and Queued Input

Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, and mid-turn queued user prompts.

## Implementation

The TUI exposes Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, POSIX Ctrl+D, and mid-turn queued user prompts. Enter while busy appends to `App::pending_prompts: VecDeque<String>` (rendered as a dim preview above the input); `agent_turn` accumulates the same submits in a per-turn `Vec<String>` and splices each into `messages` as a trailing User message at every round boundary, so the model sees the new instructions in the very next API request without aborting in-flight work. Tool-less turns have no round boundary, so their queued prompts fall through to `App::finalize_idle()`'s drain. Cancellation drops the `agent_turn` future -- reqwest closes the SSE stream, `kill_on_drop(true)` reaps subprocesses, and a dim italic `(interrupted)` marker lands in chat. The status bar colocates the active-key hint with the spinner: `Streaming . Esc to interrupt` / `Running {tool} . Esc to interrupt` / `Cancelling...` / `Press Ctrl+C again to exit`.

## Design Decisions

The picks lean toward the Codex precedent on most surfaces -- it is the one Rust reference and its patterns map onto our async / actor architecture without reshaping. The wire-shape choice (3) is the deliberate exception.

1. **Drop-the-future cancellation, no in-loop checks.** Esc / Ctrl+C drops the `agent_turn` future from the agent loop; reqwest's SSE-close-on-drop and `kill_on_drop(true)` clean up automatically. Simpler than Codex's protocol message.
2. **Discard partial assistant state on cancel.** The cancelled assistant message and orphan `tool_use` are not preserved; the TUI shows the streamed prefix with an `(interrupted)` suffix but the model sees clean history. Follows Codex (discard) over Claude Code (preserved tombstone).
3. **Mid-turn drain at round boundary, raw consecutive User messages on the wire.** Each queued prompt becomes its own `User` message with one `Text` block; Anthropic accepts consecutive same-role messages. Deliberately drops the `<system-reminder>` envelope that Claude Code and opencode wrap with -- keeps persistence trivial. May flip if the model ignores mid-turn instructions.
4. **Queue authority across the cancel window.** Normal busy: agent buffer is canonical, TUI mirror confirms via `PromptDrained`. On cancel: agent buffer drops with the future, TUI mirror replays via `finalize_idle`. The TUI holds mid-turn submits locally while `Status::Cancelling` -- forwarding would race the cancel signal.
5. **Cancellation does not auto-clear the queue.** Esc on idle pops the most-recent queued prompt into the textarea for editing; gated on the buffer being empty so a draft is never silently overwritten.
6. **`Status` is a projection of `App` state, not a separate machine.** `Idle | Streaming | ToolRunning { name } | Cancelling | ExitArmed { until }`. `set_active_status` short-circuits while `Cancelling` / `ExitArmed`.
7. **Cancellation rides the existing `AgentEvent` channel.** `AgentEvent::Cancelled` covers both TUI and `StdioSink`. No new event channel.
8. **`agent_loop_task` fires `Error` _or_ `TurnComplete`, never both.** Both run the same TUI teardown; emitting both double-drained the queue head on every API failure.

## Sources

- `crates/oxide-code/src/agent.rs` -- `agent_turn` round loop, `await_unless_aborted`, `record_drained_prompts`, `TurnAbort` enum.
- `crates/oxide-code/src/agent/event.rs` -- `AgentEvent::{Cancelled, PromptDrained}`, `UserAction::{Cancel, ConfirmExit, Quit}`, `INTERRUPTED_MARKER`, `StdioSink::render`.
- `crates/oxide-code/src/agent/pending_calls.rs` -- `clear()` evicts orphan tool calls at turn boundaries.
- `crates/oxide-code/src/main.rs` -- `agent_loop_task`, `bare_repl` / `headless`.
- `crates/oxide-code/src/tool/bash.rs` -- `kill_on_drop(true)`.
- `crates/oxide-code/src/tui/app.rs` -- `App::pending_prompts`, `dispatch_user_action` / `apply_action_locally`, `finalize_idle` / `drain_pending_prompt`, `set_active_status`, `expire_armed_exit`.
- `crates/oxide-code/src/tui/components/chat/blocks/interrupted.rs` -- `InterruptedMarker` block.
- `crates/oxide-code/src/tui/components/input.rs` -- placeholder copy, Ctrl+D POSIX gate, Esc-pop, `set_text`.
- `crates/oxide-code/src/tui/components/status.rs` -- `Status` variants, busy hints, spinner.
