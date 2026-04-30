# Cancellation and Queued Input

Research notes on three coupled TUI controls: (1) Esc / Ctrl+C cancelling an in-flight stream or tool call, (2) double-press Ctrl+C exit from idle, and (3) typing while the agent is busy, with Enter queueing the prompt to fire after the current turn. The shared problem: today the TUI exposes only "submit" and "quit", so a hung tool has no escape, a finished thought during streaming has nowhere to go, and Ctrl+C is a hammer that always closes the program. The three features only make sense together — without cancel there is nowhere to escape, without queueing the user has to babysit the spinner, and without dual-press exit the cancel key would compete with quit. Based on analysis of [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [opencode](https://github.com/anomalyco/opencode), and [OpenAI Codex](https://github.com/openai/codex).

## Reference Implementations

### Claude Code (TypeScript)

Cancellation rides an `AbortController`; the user-facing controls are split: Esc cancels, Ctrl+C is overloaded for cancel-then-exit.

**Cancellation flow.** Esc in any busy state aborts via `useCancelRequest.ts` → `abortController.abort('interrupt')`. The streaming `query()` generator (`query.ts:1027`) catches the abort and yields a synthetic `tool_result` block with content `"Interrupted by user"` against any dangling `tool_use` ids — the partial assistant turn is preserved as a normal message, with the interruption marker as the trailing tool result. On resume, `conversationRecovery.ts` detects the marker, transforms it to an `interrupted_prompt`, and appends a `"Continue from where you left off"` synthetic user message so the model can pick up the thread. The "submit-interrupts" path (user types during streaming → Enter cancels and dispatches) skips the tombstone (line 1046, `signal.reason !== 'interrupt'`) because the queued prompt itself provides enough context.

**Dual-press exit.** `useDoublePress.ts` exposes an 800 ms confirmation window for Ctrl+C and Ctrl+D. First press flips a modal `exitState.pending = true` that ~50 components read to render `"Press Ctrl-C again to exit"` (or `Ctrl-D`) inline; second press within 800 ms exits, the timeout resets the flag silently. Esc is _not_ dual-press — it only cancels or pops the queue, it never exits.

**Queue.** Implemented as a true FIFO with priorities (`messageQueueManager.ts`). User input during streaming is enqueued at priority `'next'` (dequeue order: `now > next > later`). `useQueueProcessor` dispatches the head when `isLoading` transitions true → false. `PromptInputQueuedCommands.tsx` renders the upcoming prompts as fully-formatted user messages directly below the input box, capped at 3 with a `"+N more"` overflow tag. Up-arrow / Esc on a non-empty queue calls `popAllEditable()`, pulling queued prompts back into the input buffer for editing; a second Esc with the queue still non-empty calls `clearCommandQueue()`.

**Sources:**

- `claude-code/src/hooks/useDoublePress.ts` — 800 ms window, Ctrl+C / Ctrl+D.
- `claude-code/src/hooks/useCancelRequest.ts` — abort-controller wiring.
- `claude-code/src/query.ts:1027,1046` — interrupt tombstone, submit-interrupts skip.
- `claude-code/src/utils/conversationRecovery.ts` — resume-side `interrupted_prompt`.
- `claude-code/src/services/messageQueueManager.ts` — FIFO + priorities.
- `claude-code/src/components/PromptInputQueuedCommands.tsx` — preview render.

### OpenAI Codex (Rust)

State is implicit, distributed across `ChatWidget` flags (`agent_turn_running: bool`, `is_review_mode: bool`, `submit_pending_steers_after_interrupt: bool`, `interrupted_turn_notice_mode: InterruptedTurnNoticeMode`). The TUI has no monolithic enum — the _protocol_ layer carries the cancellation primitive (`Op::Interrupt`, `TurnAbortReason`).

**Cancellation flow.** Esc is repurposed as "interrupt and submit pending steers" — only fires when there is a queued steer to take precedence over (otherwise it is a no-op). Ctrl+C is the general interrupt: defers first to the bottom-pane (close modal / popup / clear composer), then escalates to `AppCommand::interrupt()` if work is active. The interrupt is _not_ a `tokio_util::sync::CancellationToken`. It is a message (`Op::Interrupt`) routed through the app-server protocol, which the server-side actor uses to abort the SSE stream and any in-flight tool subprocesses.

**Tombstone.** None — there is no `"Interrupted by user"` tool_result. On `on_interrupted_turn()` the partial assistant message is discarded, then `finalize_turn()` runs and queued / pending / rejected steers are merged back into the composer as a single mergeable `UserMessage`. The user can edit and resubmit. The notice is a footer log line ("Conversation interrupted — tell the model what to do differently."), suppressible via `InterruptedTurnNoticeMode::Suppress`.

**Queue.** True FIFO: `queued_user_messages: VecDeque<QueuedUserMessage>`. Render lives in a `PendingInputPreview` widget _above_ the composer, with three layered sections — pending steers (auto-fire after tool boundaries unless Esc preempts), rejected steers (validation failures, will resubmit), and queued user drafts. Drained one per turn boundary by `maybe_send_next_queued_input()`, gated by `suppress_queue_autosend`. Alt+Up pops the most recent queued message back into the composer for editing. The busy header shows `(press esc to interrupt and send immediately)` only when actionable.

**Dual-press exit.** Infrastructure exists (`QUIT_SHORTCUT_TIMEOUT = 1s`, footer hint "press Ctrl+C again to quit") but currently disabled (`DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED = false`). Ctrl+D is single-press exit (composer empty, no modal). Esc never exits.

**Status indicator.** `StatusIndicatorWidget` is not just a spinner: animated frames + "Working" header + elapsed time (`42s`, `1m 23s`, `2h 03m 45s`) + an explicit "Esc to interrupt" hint when the action is meaningful + an inline message slot for the current tool name or context summary. Example: `⠙ Working (42s • Esc to interrupt) · Running my_tool()`.

**Sources:**

- `codex-rs/tui/src/chatwidget.rs` — run-state flags, `on_ctrl_c`, `on_interrupted_turn`, `finalize_turn`.
- `codex-rs/tui/src/bottom_pane/mod.rs` — `DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED`, `QUIT_SHORTCUT_TIMEOUT`.
- `codex-rs/tui/src/pending_input_preview.rs` — queue render.
- `codex-rs/tui/src/status_indicator_widget.rs` — busy display.

### opencode (TypeScript)

Server-side state machine in `packages/opencode/src/session/run-state.ts` (`idle | busy | retry`). Each session owns a `Runner` (Effect-TS `InstanceState`); `cancel` aborts an active runner, idle is set directly. Cleaner separation than Claude Code or Codex: the TUI never owns turn state, only mirrors it.

**Cancellation flow.** Dual-press Esc — first press increments `store.interrupt`, second within a 5-second window calls `sdk.client.session.abort({ sessionID })`. `Runner.cancel` is an Effect combinator that abort-signals the HTTP stream and tool subprocesses together. No `"Interrupted by user"` marker — the partial assistant message is left mid-render with whatever streamed, and the status flips back to idle. The dual-press is _for cancel_, not exit.

**Queue.** Single-slot `AsyncQueue<TuiRequest>` on the server, but no separate UI for queued drafts. Typed prompts during streaming go through `/append-prompt`, which fires a `TuiEvent.PromptAppend` that _appends inline to the current input textarea_ — there is no "queued for next turn" concept; the user is composing a longer prompt that will fire as one when they hit Enter. The simplest of the three models, and arguably the least useful when the user wanted to fire-and-forget a follow-up.

**Dual-press exit.** None — `app_exit` (`ctrl+c,ctrl+d,<leader>q`) is single-press when the prompt is empty. The dual-press machinery in opencode is specifically for interrupt, not for exit.

**Status indicator.** Minimal — 8-frame braille spinner at 80 ms, muted gray, in the assistant message header during streaming. No tool name, no elapsed time, no token count. The footer carries LSP / MCP status and permissions, but nothing turn-scoped.

**Sources:**

- `opencode/packages/opencode/src/session/run-state.ts` — state machine, `Runner` cancel.
- `opencode/packages/tui/src/components/prompt/index.tsx:274-303` — Esc dual-press interrupt.
- `opencode/packages/opencode/src/server/routes/instance/tui.ts:19-29,82-103` — `AsyncQueue`, `PromptAppend` inline insert.
- `opencode/packages/tui/src/components/keybinds.ts:19` — `app_exit` single-press.
- `opencode/packages/tui/src/components/spinner.tsx` — 8-frame braille.

## Comparison

| Repo               | Run state location               | Cancel keys                         | Cancel transport              | Tombstone                         | Queue                                  | Pop / edit queued | Dual-press exit          | Busy hint                                          |
| ------------------ | -------------------------------- | ----------------------------------- | ----------------------------- | --------------------------------- | -------------------------------------- | ----------------- | ------------------------ | -------------------------------------------------- |
| Claude Code        | implicit (`isLoading`)           | Esc; Ctrl+C overloaded              | `AbortController`             | `"Interrupted by user"` synthetic | FIFO + priorities, preview below input | Esc / Up pop      | 800 ms (Ctrl+C / Ctrl+D) | spinner only                                       |
| Codex              | distributed flags + protocol op  | Esc (steer-only); Ctrl+C            | `Op::Interrupt` actor message | none; queued restored to composer | `VecDeque`, header preview             | Alt+Up pops       | 1 s, currently disabled  | spinner + elapsed + tool name + "Esc to interrupt" |
| opencode           | server actor (`idle/busy/retry`) | dual-press Esc within 5 s           | `session.abort(id)` Effect    | none                              | single `AsyncQueue`, inline append     | n/a               | none (single-press exit) | spinner only                                       |
| oxide-code (today) | implicit (`Status` + `enabled`)  | Ctrl+C = quit, no Esc, no busy-time | n/a — Ctrl+C exits            | n/a                               | n/a — input disabled drops keys        | n/a               | n/a (single-press exit)  | spinner + tool icon                                |

## oxide-code Today

The TUI exposes one busy-state control: Ctrl+C immediately quits via `InputArea::handle_event` (`tui/components/input.rs:88-95`), regardless of whether a turn is in flight. There is no Esc handling, no cancel signal, no dual-press confirmation. The agent loop itself has no cancellation seam — `agent_turn` (`agent.rs:61-168`) drives `MAX_TOOL_ROUNDS` rounds of stream-then-dispatch with no `CancellationToken`, no abort flag, no early-return path between rounds.

The non-TUI modes already half-solve cancellation by accident: bare REPL (`main.rs:447-454`) and headless (`main.rs:501-508`) race `agent_turn` against `shutdown_signal()` in a `tokio::select!`. When SIGINT fires, the future is dropped — the SSE stream closes (reqwest's `bytes_stream` cleans up on drop), in-flight bash subprocesses die via `kill_on_drop(true)` (`tool/bash.rs:108`), and session state already written to JSONL is left alone. Resume-side sanitization heals any dangling tool_use. The bare REPL `break`s out of the prompt loop on cancel, so the user can't type a follow-up; headless exits entirely. The TUI doesn't even get this — its `select!` arm catches SIGTERM / SIGHUP only (raw mode eats SIGINT, `main.rs:309-312`), and the user-side Ctrl+C is the always-quit path through the input component.

Status state lives in two places: the `Status` enum in `tui/components/status.rs:46-50` (`Idle | Streaming | ToolRunning`) drives the spinner, and `InputArea::enabled` (`tui/components/input.rs:62-72`) gates input acceptance. Both are derived state — no single source of truth. The `App` itself does not name the run state. The status bar carries no hint about what keys are active in each state; the input area's footer is static (`Enter: send · Shift+Enter: newline · Ctrl+C: quit`).

Input typed while `enabled = false` is silently dropped (`input.rs:98-100`). There is no message queue. The agent loop task (`main.rs:341-392`) consumes one `UserAction` from the mpsc, runs the turn to completion, and only then loops back to `recv()` — naturally serial. The mpsc itself is a 32-slot FIFO, so multiple Submits _would_ queue at the channel level, but the input component never produces them while busy because of the disabled-input gate.

The chat-block stack handles partial mid-stream state via `StreamingAssistant` (`tui/components/chat/blocks/streaming.rs`) which `commit_streaming()` promotes to a finalized `AssistantText` block on `TurnComplete`. There is no path for "finalize-and-mark-interrupted"; if cancellation lands mid-stream today the buffer would either commit normally on a synthetic `TurnComplete` or get stranded. `pending_calls` (`agent/pending_calls.rs`) already evicts orphaned tool-call entries at turn boundaries via `clear()` (`tui/app.rs:248-259`), so an aborted tool call won't carry over — but the rendered chat would show a `ToolCallStart` block with no matching result, and no marker tells the reader why.

## Design Decisions for oxide-code

The shipping unit is three coupled features. Decisions span five surfaces: where cancellation lives, what happens to in-flight state, the key-to-action map, the queue model, and the visible feedback.

1. **Drop-the-future cancellation, no in-loop checks.** Race `agent_turn` against a per-turn `CancellationToken` in the agent loop, mirroring the bare REPL / headless `agent_turn` vs. `shutdown_signal` pattern (`main.rs:447-454`). Reqwest cleans up the SSE stream on drop, `kill_on_drop(true)` on `tokio::process::Child` (`tool/bash.rs:108`) kills bash subprocesses, and the actor-backed session writes are mpsc-queued so a dropped await still flushes. No `is_cancelled()` seams scattered through `agent_turn` itself — the loop stays straight-line. We use `tokio_util::sync::CancellationToken` rather than a bare `oneshot` because the token's `child_token()` lets a session-level cancel kill everything in flight when we ever need that (e.g., on `App` teardown).

2. **Discard partial assistant state on cancel; rely on resume-side sanitization for cross-session healing.** The cancelled assistant message and any orphan `tool_use` are not preserved in `messages`. The TUI commits the in-flight `StreamingAssistant` block with a dim italic `(interrupted)` suffix so the user sees what was on screen, but the model on the next turn sees a clean history. Claude Code's preserved-tombstone approach gives the model context but adds plumbing (synthesizing a `tool_result` against an orphan id, persisting through resume). Codex discards too, and the abort-and-retype workflow is fine when cancel is fast. Revisit only if users report the model "forgetting" what it was doing.

3. **Esc and Ctrl+C share cancel semantics while busy; Ctrl+C in idle is dual-press exit; Esc in idle pops queue.** Esc never exits — Claude Code's split. Ctrl+C overloads cancel-then-exit — Codex's split. While busy, both keys cancel; the user does not have to remember the difference. From idle, Ctrl+C arms a 1-second exit window (matches Codex; longer than Claude Code's 800 ms but a more comfortable double-tap). Ctrl+D is _not_ aliased to exit yet — defer until we know what users expect on macOS / SSH where Ctrl+D historically means EOF.

4. **TUI-side queue, FIFO `VecDeque<String>`.** Press Enter while busy appends to the queue and renders dim ghost user-messages between the chat area and the input. On `TurnComplete`, the head pops, the chat pushes a regular user message, and `try_send(UserAction::SubmitPrompt)` fires the next turn. The queue lives in `App` (not relying solely on the existing 32-slot `mpsc<UserAction>` FIFO) because the user needs to _see_ and _pop_ queued items — a pure-channel queue is invisible.

5. **Cancellation does not auto-clear the queue.** A user who interrupts a wandering turn typically still wants their planned follow-up. Esc on a non-empty queue while idle pops the most recent queued back into the input textarea for editing — repeated Esc clears items one at a time. (Up arrow could share the popping affordance, matching Codex's Alt+Up; defer until Up's current behavior in the textarea is verified to not conflict.)

6. **Single source of truth: `RunState` enum on `App`.** Variants: `Idle | Busy(BusyKind) | Cancelling | ExitArmed { until: Instant }`, with `BusyKind = Streaming | Tool { name: String }`. Today's `Status` enum (`tui/components/status.rs:46-50`) becomes a _display projection_ of `RunState`, and `InputArea::enabled` becomes `state.allows_input()`. Two parallel state machines are exactly the kind of ad-hoc derivation we should consolidate before adding a third (queue-pending) on top.

7. **Status bar surfaces every state, with an actionable hint.** Codex's `(42s • Esc to interrupt)` is the model. `Idle` → "Idle". `Busy(Streaming)` → "Streaming · Esc to interrupt". `Busy(Tool { name })` → "{name} · Esc to interrupt". `Cancelling` → "Cancelling...". `ExitArmed` → "Press Ctrl+C again to exit". The hint colocates with the spinner so the user always knows the active key — no separate notification system.

8. **Input footer is dynamic.** `Idle` → `Enter: send · Shift+Enter: newline · Ctrl+C: quit`. `Busy(*)` → `Esc / Ctrl+C: interrupt · Enter: queue prompt`. `Idle` with non-empty queue → `Up / Esc: edit queued · Enter: send`. The footer is the most reliably-glanced surface for active controls; making it state-aware is cheap and avoids surprise.

9. **Cancellation flows through the existing `AgentEvent` channel.** Add `AgentEvent::Cancelled` (paired with a `Cancelled` arm in `StdioSink` that newlines like `TurnComplete`). The TUI handler treats it like `TurnComplete` plus the italic `(interrupted)` suffix on any in-flight `StreamingAssistant`. No new event channel, no new sink trait method.

10. **Bare REPL upgrades from "exit on Ctrl+C" to "abort the turn".** Today the bare REPL `break`s out (`main.rs:452`) on Ctrl+C, ending the session. Replace with a soft-cancel that returns the user to the prompt; only Ctrl+C-while-already-cancelling (or SIGTERM / SIGHUP) exits. Headless stays exit-on-cancel — there is no follow-up prompt to return to, and the existing Summary-write-on-signal path works.

11. **Per-turn token recreated by `agent_loop_task`, owned by `App` for cancellation.** Each iteration of `agent_loop_task` creates a fresh `CancellationToken` and hands a clone to `App` over the existing user-action channel (or a sibling channel — TBD during implementation). On Esc / Ctrl+C-while-busy, `App` calls `.cancel()` on the held clone. This ties cancellation lifetime to a turn, so a stale token from a prior turn cannot accidentally cancel the next one.

The decisions intentionally lean toward the _Codex_ model over the _Claude Code_ model on most surfaces (drop-and-discard cancellation, status-bar hint, FIFO queue with header preview) because Codex is the one Rust precedent and its patterns map onto our existing async / actor architecture without reshaping. Claude Code's interrupted-tombstone preservation and `popAllEditable` are richer but require a transcript-edit pass we have not paid for; opencode's server-state machine is a cleaner separation than what we have today but would force the agent loop into a server-actor refactor we have not committed to.

## Sources

- `crates/oxide-code/src/agent.rs:61-168` — `agent_turn` round loop, the cancellation insertion site.
- `crates/oxide-code/src/agent.rs:271-329` — `stream_response` SSE pump; drop-the-future cancellation already works because `mpsc::Receiver::recv` cancels cleanly and reqwest closes the HTTP stream on drop.
- `crates/oxide-code/src/agent/event.rs:14-51` — `AgentEvent` variants; the new `Cancelled` arm slots in here.
- `crates/oxide-code/src/agent/event.rs:57-62` — `UserAction` enum, the path for the new `Cancel` action.
- `crates/oxide-code/src/agent/pending_calls.rs:96-103` — `clear()` already evicts orphaned tool calls at turn boundaries.
- `crates/oxide-code/src/main.rs:218-242` — `shutdown_signal()`, the existing SIGINT / SIGTERM / SIGHUP handler.
- `crates/oxide-code/src/main.rs:341-392` — TUI `agent_loop_task`, the consumer side of `UserAction` (FIFO via mpsc — multiple queued submits already work at the channel level).
- `crates/oxide-code/src/main.rs:447-454,501-508` — bare REPL and headless `agent_turn` vs. `shutdown_signal()` race, the existing soft cancel.
- `crates/oxide-code/src/tool/bash.rs:108` — `kill_on_drop(true)` on `tokio::process::Child`, the path that ensures bash subprocesses die on cancel.
- `crates/oxide-code/src/tui/app.rs:96-131` — main `tokio::select!` loop; the dual-press exit timer plugs into the tick arm.
- `crates/oxide-code/src/tui/app.rs:135-154` — `handle_crossterm_event`, the routing point for Esc and Ctrl+C disambiguation.
- `crates/oxide-code/src/tui/components/input.rs:87-121` — current `Ctrl+C → Quit` path; the always-quit logic moves out of the input component into `App::dispatch_user_action`.
- `crates/oxide-code/src/tui/components/status.rs:46-50` — `Status` enum; `Cancelling` and the dual-press hint slot here.
- `crates/oxide-code/src/tui/components/chat/blocks/streaming.rs` — `commit_streaming()`; cancellation needs a sibling that finalizes the buffer with an "interrupted" marker.
