# Cancellation and Queued Input

Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, and mid-turn queued user prompts.

## Implementation

The TUI exposes Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, POSIX Ctrl+D, and mid-turn queued user prompts. Enter while busy appends to `App::pending_prompts: VecDeque<String>`, rendered as a dim preview above the input. `agent_turn` accumulates the same submits in a per-turn `Vec<String>` and splices each into `messages` as a trailing User message at every round boundary, so the model sees new instructions in the next API request without aborting in-flight work. Tool-less turns have no round boundary, so queued prompts fall through to `App::finalize_idle()`'s drain. Cancellation drops the `agent_turn` future: reqwest closes the SSE stream, `kill_on_drop(true)` kills a running `bash` tool's direct child process, and a dim italic `(interrupted)` marker lands in chat. Processes that child spawned can keep running, since the `bash` tool kills its whole process group only on timeout. The status bar colocates the active-key hint with the spinner: `Streaming · Esc to interrupt` / `Running {tool} · Esc to interrupt` / `Cancelling` / `Press Ctrl+C again to exit`.

## Design Decisions

The picks lean toward the Codex precedent on most surfaces, since it is the one Rust reference and its patterns map onto our async / actor architecture without reshaping. The wire-shape choice (3) is the deliberate exception.

1. **Drop-the-future cancellation.** Esc / Ctrl+C drops the `agent_turn` future from the agent loop. Dropping it closes the reqwest SSE stream and kills the spawned `bash` child through `kill_on_drop(true)`, so cancellation needs no protocol message of the kind Codex sends.

2. **Discard partial assistant state on cancel.** The TUI shows the streamed prefix with an `(interrupted)` suffix, and the model's history omits the cancelled assistant message and any orphan `tool_use`. Codex discards the partial message the same way, while Claude Code keeps a tombstone.

3. **Mid-turn drain at round boundary, raw consecutive User messages on the wire.** Each queued prompt becomes its own `User` message with one `Text` block. Anthropic accepts consecutive same-role messages. oxide-code deliberately drops the `<system-reminder>` envelope that Claude Code and opencode wrap with, which keeps persistence trivial. May flip if the model ignores mid-turn instructions.

4. **Queue authority across the cancel window.** Normal busy: agent buffer is canonical, TUI mirror confirms via `PromptDrained`. On cancel: agent buffer drops with the future, TUI mirror replays via `finalize_idle`. The TUI holds mid-turn submits locally while `Status::Cancelling`, since forwarding would race the cancel signal.

5. **Cancellation does not auto-clear the queue.** Esc on idle pops the most-recent queued prompt into the textarea for editing, gated on the buffer being empty so a draft is never silently overwritten.

6. **`Status` is projected from `App` state.** The variants (`Idle | Streaming | ToolRunning { name } | Cancelling | ExitArmed { until }`) derive from app fields, so `set_active_status` can short-circuit while `Cancelling` / `ExitArmed` without a parallel state to keep in sync.

7. **Cancellation rides the existing `AgentEvent` channel.** `AgentEvent::Cancelled` covers both TUI and `StdioSink`. No new event channel.

8. **A turn reports its outcome with one event.** `Conversation::submit_prompt` ends a turn with `TurnComplete`, `Error`, or `Cancelled`, and never pairs `Error` with `TurnComplete`, because both run the same TUI teardown and the pair would drain the queue head twice on an API failure. Auto-compaction and session-write failures emit their own `Error` earlier in the turn, and the TUI runs the same teardown for those.

## Sources

- `crates/oxide-code/src/agent.rs`: `agent_turn` round loop, `await_unless_aborted`, `record_drained_prompts`, `TurnAbort` enum.
- `crates/oxide-code/src/agent/event.rs`: `AgentEvent::{Cancelled, PromptDrained}`, `UserAction::{Cancel, ConfirmExit, Quit}`, `INTERRUPTED_MARKER`, `StdioSink::render`.
- `crates/oxide-code/src/driver.rs`: `Conversation::submit_prompt` and `report_abort`, shared by the TUI, REPL, and headless drivers.
- `crates/oxide-code/src/tool/bash.rs`: `kill_on_drop(true)` on the spawned child, plus the timeout-only `kill_process_group`.
- `crates/oxide-code/src/tui/app.rs`: `App::pending_prompts`, `dispatch_user_action` / `apply_action_locally`, `finalize_idle` / `drain_pending_prompt`, `set_active_status`, `expire_armed_exit`.
- `crates/oxide-code/src/tui/components/chat/blocks/interrupted.rs`: `InterruptedMarker` block.
- `crates/oxide-code/src/tui/components/input.rs`: Ctrl+D POSIX gate, Esc-pop, `set_text`.
- `crates/oxide-code/src/tui/components/status.rs`: `Status` variants, busy hints, spinner.
- `crates/oxide-code/src/tui/pending_calls.rs`: `clear()` evicts orphan tool calls at turn boundaries.
