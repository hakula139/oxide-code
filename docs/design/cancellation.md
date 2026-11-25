# Cancellation and Queued Input

The TUI cancels active work on Esc or Ctrl+C, exits on a second Ctrl+C or Ctrl+D from idle, and queues prompts typed while busy. [Engine](engine.md) covers the operation and event contract these controls drive.

## Controls

- **Cancel and exit.** Esc / Ctrl+C cancels active work. A second Ctrl+C (or second Ctrl+D with an empty composer, since Ctrl+D deletes forward when text is present) exits from idle. `/exit` quits from any state like a shutdown.
- **Queue and drain.** Enter while busy appends to `PromptQueue` (shown dim above the input) and forwards `Op::Queue`. The engine splices forwarded prompts at the next round boundary ([Turn Pipeline](engine.md#turn-pipeline)) and confirms with `Event::PromptDrained`. Any prompts left when a turn ends stay in the TUI queue, whose head starts as the next turn on completion or failure.
- **Hand back input.** On failed pre-prompt auto-compaction, the TUI returns the unsent prompt (**The TUI owns the queue**). Mid-turn compaction failures end the turn and hold the queue ([Inside a Turn](compaction.md#inside-a-turn)). `TurnRolledBack` returns the refused prompt and drained prompts (or only post-compaction drained prompts after a mid-turn compaction).
- **Interrupt work.** Cancelling drops the `agent_turn` future (closing the reqwest SSE stream and killing `bash`'s process tree), renders a dim italic `(interrupted)` line, and records a `[Request interrupted by user]` user message.
- **Show status.** The status bar names the active operation and stop key beside the spinner (such as `Streaming · Esc to interrupt`), switches to `Cancelling` until `OpEnded`, and shows armed-exit keys (omitting Ctrl+D while the input holds a draft).

## Design Decisions

Choices follow Codex where its Rust async / actor patterns fit, with the wire shape (**Mid-turn drain at round boundary, raw consecutive User messages on the wire**) as the deliberate exception.

1. **Drop-the-future cancellation.** Dropping `agent_turn` is simpler than Codex's protocol cancel message. `Tool::run` requires tools to stop on drop, poll `ToolCtx::cancel` (cancelled after drop, used by blocking `grep` / `glob` walks), or finish atomically with bookkeeping (Edit and Write run file writes and tracker updates in one spawned task). reqwest closes SSE on drop, and `ShellTree` kills the process tree including trailing-`&` descendants.

2. **Commit only settled state on cancel.** Mid-stream cancel discards the partial assistant message (as in Codex) while the TUI keeps the streamed prefix plus `(interrupted)`. Mid-tool-round cancel commits the assistant message and a result for every call so completed workspace changes stay visible, giving unfinished calls `is_error` results distinguishing partially run calls from never-started ones (including pending approvals).

   Both cancel and mid-turn shutdown then append `[Request interrupted by user]` so the cancelled prompt and next prompt do not reach the model as two unanswered turns (using one text where Claude Code adds a `for tool use` variant, since tool results already mark interrupted calls). The original prompt stays in the transcript, and resume replays the marker as `(interrupted)`.

3. **Mid-turn drain at round boundary, raw consecutive User messages on the wire.** Each queued prompt becomes a `User` message with one `Text` block (accepted by Anthropic), omitting the `<system-reminder>` wrapper used by Claude Code and opencode.

4. **The TUI owns the queue.** `PromptQueue` tracks the active and queued prompts, storing both the display line (typed text or `/init <file>`) and engine text (which differ for `//` escapes and prompt-building slash commands):

   - **Hand-back.** Failed turns return a slash prompt to an empty input as its command line (noting Enter re-runs against current files) and to the queue as its built prompt so draining never reopens a picker. Unsent prompts return to an empty input, or to the queue head when prompts wait or a draft exists, holding the queue.
   - **Leaving the queue.** Entries leave only on matching `PromptDrained` (tracked against leading entries owned by the active turn), when starting their own turn, on idle Esc recall, or on session switch (**A cancel leaves the queue to the user**).
   - **Forwarding.** Queued prompts forward only during an active turn with no pending cancel or auto-compaction. Prompts typed during other ops wait and start when the op ends (**One operation in flight, ended by its own `OpEnded`**), and the idle engine drops late `Op::Queue`.

5. **A cancel leaves the queue to the user.** Cancel always returns to idle with the queue held until the next submit or session switch, even if the turn finished before cancel landed. Cancel before prompt recording marks the prompt unsent (handing it back with no interrupted marker):

   - **Recall and resubmit.** Idle Esc pops the newest queued prompt into an empty composer without overwriting drafts. The next submit appends and starts from the queue head, except prompt-building slash commands (like `/init`), whose echoed line starts its own turn ahead of the queue.
   - **Session switches.** `/clear`, bare `/archive`, and `/resume` discard pre-switch queued prompts with a count notice while holding input busy so prompts typed during the switch run in the new session.
   - **Failed switches.** Failed switches hold those prompts, running them if a matching retry lands (`/clear` retries bare `/archive`, and any `/resume` retries `/resume`). Switching to the other kind treats them as pre-switch, and submitting before retrying runs the held prompts in the current session ahead of the new submission. Esc cannot stop an in-flight switch, and instead clears the switch mark (`PromptQueue::leave_to_user`) while holding the queue so later retries discard them as pre-switch.
   - **Compaction.** `/compact` preserves the queue because the conversation continues.

6. **One operation in flight, ended by its own `OpEnded`.** `App::in_flight` tracks the running idle-only op and its `OpId` until its matching `OpEnded` ([**One end event per operation, under the op's ID**](engine.md#design-decisions)), blocking overlapping idle-only ops (including `/rename` and `/model`):

   - **What follows an op.** `App::end_operation` alone decides: cancels and `PromptQueue::fail_turn` failures hold the queue, and an already-held queue stays held until the next submit or session switch even when another op completes. Otherwise the queue head starts.
   - **Modals.** Queue start defers while any modal with `Modal::sends_idle_only_ops` stays open (such as `/resume` after an archive toggle or `/delete` confirm) so its op cannot fail as busy, resuming when the last such modal closes. Read-only modals like `/help` defer nothing.
   - **Status and keys.** Status labels derive from `in_flight` plus the latest stream / tool / compaction event (holding `Cancelling` while a cancel is pending). `App::exit_armed` tracks armed exit separately on idle input, and `App` routes Esc, Ctrl+C, and Ctrl+D centrally without reading state back from views.

7. **Cancellation is an op's end.** Cancelled ops end with `OpEnded` (`OpEnd::Cancelled`) on the standard event stream, letting the TUI and headless `run_op` settle cancels without a second channel.

## Sources

- `crates/ox-core/src/engine.rs`: `Engine::serve`, which ignores an `Op::Queue` that arrives between turns, plus `Engine::submit_prompt`, `report_abort`, and `report_unsent`, which every frontend's operation ends through.
- `crates/ox-core/src/engine/abort.rs`: the `TurnAbort` enum and `Mailbox`, which races each wait against interrupts and holds the queued prompts.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round` / `ToolRound`, `Dispatcher::run_ready`, which cancels the running calls on an abort.
- `crates/ox-core/src/engine/history.rs`: `walk_transcript`, which shows a resumed interruption marker as `Interaction::Interrupted`.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn` round loop, `interrupt_turn`, `record_drained_prompts`.
- `crates/ox-core/src/message.rs`: `Message::interruption`, the marker a user abort records.
- `crates/ox-core/src/protocol.rs`: `Event::{OpEnded, PromptDrained, TurnFailed}`, `OpEnd`, `Interaction::Interrupted`, `Op::{Interrupt, Queue, Shutdown}`.
- `crates/ox-core/src/tool.rs`: `Tool::run` drop contract, `ToolCtx::cancel`.
- `crates/ox-core/src/tool/bash.rs`: `ShellTree`, which kills the shell's process group (Unix) or Job Object (Windows) when a call times out or drops before finishing.
- `crates/ox-tui/src/app.rs`: `App::in_flight` and `App::exit_armed`.
- `crates/ox-tui/src/app/dispatch.rs`: `dispatch_op` / `start_turn` / `forward_queued_prompts`, `handle_esc`, `handle_ctrl_c`, the delete-forward and empty-input exit split in `handle_ctrl_d`, and `expire_armed_exit`.
- `crates/ox-tui/src/app/events.rs`: `handle_engine_event`, `end_operation`, `settle_failure`, `discard_queue`, `drain_pending_prompt`, and `set_activity`.
- `crates/ox-tui/src/app/operation.rs`: `InFlight`, which tracks the idle-only op until its `OpEnded`, and the `Activity` the status bar shows.
- `crates/ox-tui/src/app/queue.rs`: `PromptQueue` and its `QueuedPrompt` entries, the active prompt, the queued prompts, what a failed turn hands back, and the prompts a session switch leaves behind.
- `crates/ox-tui/src/app/render.rs`: `App::run_label`, the status bar's operation label and the key that stops it.
- `crates/ox/src/driver/stdio.rs`: `run_op`, which renders one operation until its `OpEnded` or a signal, and `TextState::render`, which prints the `(interrupted)` marker.
