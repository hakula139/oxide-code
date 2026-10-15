# Cancellation and Queued Input

Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, and mid-turn queued user prompts.

## Implementation

The TUI exposes Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, POSIX Ctrl+D, and mid-turn queued user prompts. Enter while busy appends to `App::pending_prompts: VecDeque<String>`, rendered as a dim preview above the input, and forwards the prompt as `UserAction::QueuePrompt`. `agent_turn` splices each forwarded prompt into `messages` as a trailing User message at its next round boundary and confirms it with `AgentEvent::PromptDrained`, so the model sees new instructions in the next API request without aborting in-flight work. A turn that ends first drops the prompts it still holds, and once it completes or fails the TUI submits the queue head as the next turn. Cancellation drops the `agent_turn` future: reqwest closes the SSE stream, the bash tool kills the command's process group, and a dim italic `(interrupted)` marker lands in chat. The status bar colocates the active-key hint with the spinner: `Streaming · Esc to interrupt` / `Running {tool} · Esc to interrupt` / `Cancelling` / `Press Ctrl+C again to exit`.

## Design Decisions

The picks lean toward the Codex precedent on most surfaces, since it is the one Rust reference and its patterns map onto our async / actor architecture without reshaping. The wire-shape choice (3) is the deliberate exception.

1. **Drop-the-future cancellation.** Esc / Ctrl+C drops the `agent_turn` future from the agent loop, which is simpler than Codex's protocol message. The `Tool::run` contract requires each tool to stop its work when the future drops, or to finish it together with its bookkeeping. reqwest closes the SSE stream on drop, and the bash tool's `ProcessGroupGuard` kills the whole process group, so backgrounded descendants stop with the shell. Edit and Write run each file write and its tracker record in one spawned task, so a write that lands after the cancel is still recorded.

2. **Commit only settled state on cancel.** A cancel mid-stream discards the partial assistant message, as Codex does. The TUI keeps the streamed prefix with an `(interrupted)` suffix. A cancel during a tool round commits the assistant message with a result for every call, because the finished calls already changed the workspace and the model must see them. Each unfinished call gets an `is_error` result saying the user interrupted it, so the transcript never holds an orphan `tool_use`. The result for a call stopped mid-run says it may have run partially, and the one for a call that never started, including one awaiting approval, says it did not run.

3. **Mid-turn drain at round boundary, raw consecutive User messages on the wire.** Each queued prompt becomes its own `User` message with one `Text` block. Anthropic accepts consecutive same-role messages. oxide-code deliberately drops the `<system-reminder>` envelope that Claude Code and opencode wrap with, which keeps persistence trivial. May flip if the model ignores mid-turn instructions.

4. **The TUI owns the queue.** A queued prompt leaves `App::pending_prompts` only when a matching `PromptDrained` arrives or when it starts a turn of its own. `App::forwarded_prompts` counts the leading entries the in-flight turn holds, and a drain pops the head only when it names that head. While `Status::Compacting` or `Status::Cancelling`, no turn may be left to splice prompts in, so the TUI holds new submits and forwards them in order once a turn resumes streaming. The idle agent loop discards a late `QueuePrompt`, since the TUI submits it again.

5. **A cancel leaves the queue to the user.** After a cancel the TUI goes idle without submitting the queue head, even when the turn completed before the cancel landed. Esc on idle pops the most recent queued prompt into the textarea for editing, gated on the buffer being empty so a draft is never silently overwritten. The next submit joins the back of the queue and starts a turn with its head, so prompts keep the order they were typed in.

6. **`Status` is projected from `App` state.** The variants (`Idle | Streaming | ToolRunning { name } | Cancelling | ExitArmed { until }`) derive from app fields, so `set_active_status` can short-circuit while `Cancelling` / `ExitArmed` without a parallel state to keep in sync.

7. **Cancellation rides the existing `AgentEvent` channel.** `AgentEvent::Cancelled` covers both TUI and `StdioSink`. No new event channel.

8. **One terminal event per operation.** A prompt ends with exactly one of `TurnComplete`, `TurnFailed`, or `Cancelled`. `/compact` ends with `SessionCompacted`, `TurnFailed`, or `Cancelled`, and `/resume` with `SessionResumed` or `TurnFailed`. Only these run the TUI teardown that submits the queue head. `Error` is a notice for failures that leave the operation running, such as a session write failure, a failed automatic compaction, or a refusal before its `TurnComplete`, and `Retrying` is a dim notice for a request the client sends again.

## Sources

- `crates/oxide-code/src/agent.rs`: `agent_turn` round loop, `run_tool_round` / `ToolRound`, `dispatch_tool_call`, `await_unless_aborted`, `record_drained_prompts`, `TurnAbort` enum.
- `crates/oxide-code/src/agent/event.rs`: `AgentEvent::{Cancelled, PromptDrained, TurnFailed}`, `UserAction::{Cancel, ConfirmExit, QueuePrompt, Quit}`, `INTERRUPTED_MARKER`, `StdioSink::render`.
- `crates/oxide-code/src/driver.rs`: `Conversation::submit_prompt` and `report_abort`, shared by the TUI, REPL, and headless drivers.
- `crates/oxide-code/src/driver/tui.rs`: `serve_actions`, which ignores a `QueuePrompt` that arrives between turns.
- `crates/oxide-code/src/file_tracker.rs`: `FileTracker::write_and_record`.
- `crates/oxide-code/src/tool.rs`: `Tool::run` drop contract, `write_tracked`.
- `crates/oxide-code/src/tool/bash.rs`: `ProcessGroupGuard`, `kill_process_group`.
- `crates/oxide-code/src/tui/app.rs`: `App::pending_prompts` / `forwarded_prompts`, `start_turn` / `forward_queued_prompts`, `finish_turn`, `finalize_idle` / `enter_idle` / `drain_pending_prompt`, `set_active_status`, `expire_armed_exit`.
- `crates/oxide-code/src/tui/components/chat/blocks/interrupted.rs`: `InterruptedMarker` block.
- `crates/oxide-code/src/tui/components/input.rs`: Ctrl+D POSIX gate, Esc-pop, `set_text`.
- `crates/oxide-code/src/tui/components/status.rs`: `Status` variants, busy hints, spinner.
- `crates/oxide-code/src/tui/pending_calls.rs`: `clear()` evicts orphan tool calls at turn boundaries.
