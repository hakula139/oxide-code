# Cancellation and Queued Input

Research notes on three coupled TUI controls: (1) Esc / Ctrl+C cancelling an in-flight stream or tool call, (2) double-press Ctrl+C exit from idle, and (3) typing while the agent is busy, with Enter queueing the prompt to fire after the current turn. The shared problem: today the TUI exposes only "submit" and "quit", so a hung tool has no escape, a finished thought during streaming has nowhere to go, and Ctrl+C is a hammer that always closes the program. The three features only make sense together — without cancel there is nowhere to escape, without queueing the user has to babysit the spinner, and without dual-press exit the cancel key would compete with quit. Based on analysis of [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [opencode](https://github.com/anomalyco/opencode), and [OpenAI Codex](https://github.com/openai/codex).

## Reference Implementations

### Claude Code (TypeScript)

Cancellation rides an `AbortController`; the user-facing controls are split: Esc cancels, Ctrl+C is overloaded for cancel-then-exit.

**Cancellation flow.** Esc in any busy state aborts via `useCancelRequest.ts` → `abortController.abort('interrupt')`. The streaming `query()` generator (`query.ts:1027`) catches the abort and yields a synthetic `tool_result` block with content `"Interrupted by user"` against any dangling `tool_use` ids — the partial assistant turn is preserved as a normal message, with the interruption marker as the trailing tool result. On resume, `conversationRecovery.ts` detects the marker, transforms it to an `interrupted_prompt`, and appends a `"Continue from where you left off"` synthetic user message so the model can pick up the thread. The "submit-interrupts" path (user types during streaming → Enter cancels and dispatches) skips the tombstone (line 1046, `signal.reason !== 'interrupt'`) because the queued prompt itself provides enough context.

**Dual-press exit.** `useDoublePress.ts` exposes an 800 ms confirmation window for Ctrl+C and Ctrl+D. First press flips a modal `exitState.pending = true` that ~50 components read to render `"Press Ctrl-C again to exit"` (or `Ctrl-D`) inline; second press within 800 ms exits, the timeout resets the flag silently. Esc is _not_ dual-press — it only cancels or pops the queue, it never exits.

**Queue.** Module-level `commandQueue` array (`messageQueueManager.ts`) with priorities (`now > next > later`). Keyboard prompts default to **`next`**, which means **mid-turn drain between tool waves, not turn-boundary**. Inside `query.ts` (lines 1535-1643), after every tool wave finishes, the queue is snapshotted (filter by priority cap) and each item is converted to an `AttachmentMessage(type: 'queued_command')` → `UserMessage` wrapped in `<system-reminder>`, spliced into the `toolResults` array so the **next** model API call in the same `queryLoop` sees it as user context. The code carries an explicit comment about the API constraint: _"Be careful to do this after tool calls are done, because the API will error if we interleave tool_result messages with regular user messages."_ Default Enter does **not** abort the stream / tools — only `priority === 'now'` or "all-in-flight-tools-are-cancel-interruptible" (i.e. only Sleep) triggers an abort. If the assistant produced text only (no tool_use), the loop returns without draining; the `useQueueProcessor` idle hook then dispatches at turn end (the `later`-equivalent path). `PromptInputQueuedCommands.tsx` renders the upcoming prompts as user messages directly below the input box. Up-arrow / Esc on a non-empty queue calls `popAllEditable()`, pulling queued prompts back into the input buffer for editing.

**Sources:**

- `claude-code/src/components/PromptInput/PromptInputQueuedCommands.tsx` — preview render.
- `claude-code/src/hooks/useCancelRequest.ts` — abort-controller wiring.
- `claude-code/src/hooks/useDoublePress.ts` — 800 ms window, Ctrl+C / Ctrl+D.
- `claude-code/src/query.ts:1027,1046` — interrupt tombstone, submit-interrupts skip.
- `claude-code/src/query.ts:1535-1643` — mid-turn drain; the comment about the tool_result interleaving constraint.
- `claude-code/src/types/textInputTypes.ts:276-293` — `QueuePriority` doc-comment defining `now / next / later` semantics.
- `claude-code/src/utils/attachments.ts:1044` — `getQueuedCommandAttachments` payload shape.
- `claude-code/src/utils/conversationRecovery.ts` — resume-side `interrupted_prompt`.
- `claude-code/src/utils/messageQueueManager.ts` — module-level queue, FIFO + priorities.
- `claude-code/src/utils/messages.ts:3739` — `<system-reminder>` wrapping in `normalizeAttachmentForAPI`.

### OpenAI Codex (Rust)

State is implicit, distributed across `ChatWidget` flags (`agent_turn_running: bool`, `is_review_mode: bool`, `submit_pending_steers_after_interrupt: bool`, `interrupted_turn_notice_mode: InterruptedTurnNoticeMode`). The TUI has no monolithic enum — the _protocol_ layer carries the cancellation primitive (`Op::Interrupt`, `TurnAbortReason`).

**Cancellation flow.** Esc is repurposed as "interrupt and submit pending steers" — only fires when there is a queued steer to take precedence over (otherwise it is a no-op). Ctrl+C is the general interrupt: defers first to the bottom-pane (close modal / popup / clear composer), then escalates to `AppCommand::interrupt()` if work is active. The interrupt is _not_ a `tokio_util::sync::CancellationToken`. It is a message (`Op::Interrupt`) routed through the app-server protocol, which the server-side actor uses to abort the SSE stream and any in-flight tool subprocesses.

**Tombstone.** None — there is no `"Interrupted by user"` tool_result. On `on_interrupted_turn()` the partial assistant message is discarded, then `finalize_turn()` runs and queued / pending / rejected steers are merged back into the composer as a single mergeable `UserMessage`. The user can edit and resubmit. The notice is a footer log line ("Conversation interrupted — tell the model what to do differently."), suppressible via `InterruptedTurnNoticeMode::Suppress`.

**Queue.** Two distinct mechanisms with different keybindings, both visible in the same `PendingInputPreview` widget _above_ the composer.

The default mid-turn path is **Enter → `steer_input` → `pending_input` on the active turn state** (`core/src/session/turn.rs:373-469`). The core `run_turn` loop drains `sess.get_pending_input()` at each sampling-iteration boundary, calls `record_pending_input()` to insert the items into history as `ResponseInputItem::Message { role: "user", content }`, then `clone_history()` builds the next request. Comment at the head of the loop spells out the design: _"Pending input is drained into history before building the next model request."_ The drain is **non-disruptive** — it does not abort the stream or tools; cancellation is a separate `Op::Interrupt` path that explicitly clears `pending_input` before emitting `TurnAborted`.

The secondary path is the TUI-side `queued_user_messages: VecDeque<QueuedUserMessage>` (`tui/src/chatwidget.rs:929-942`), used for **Tab while task running**, plan-streaming, or shell-command edge cases. Drained one per turn boundary by `maybe_send_next_queued_input()`, gated by `suppress_queue_autosend`, fired from `on_task_complete`. The preview widget renders three layered sections — `pending_steers` (already submitted to core, awaiting commit), `rejected_steers` (validation failures, will resubmit first), and `queued_user_messages` (turn-boundary drafts). Alt+Up pops the most recent queued message back into the composer for editing. The busy header shows `(press esc to interrupt and send immediately)` only when actionable.

**Dual-press exit.** Infrastructure exists (`QUIT_SHORTCUT_TIMEOUT = 1s`, footer hint "press Ctrl+C again to quit") but currently disabled (`DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED = false`). Ctrl+D is single-press exit (composer empty, no modal). Esc never exits.

**Status indicator.** `StatusIndicatorWidget` is not just a spinner: animated frames + "Working" header + elapsed time (`42s`, `1m 23s`, `2h 03m 45s`) + an explicit "Esc to interrupt" hint when the action is meaningful + an inline message slot for the current tool name or context summary. Example: `⠙ Working (42s • Esc to interrupt) · Running my_tool()`.

**Sources:**

- `codex-rs/core/src/session/mod.rs:2934` — `steer_input`: pushes onto `TurnState.pending_input`, no cancellation.
- `codex-rs/core/src/session/turn.rs:373-469` — `run_turn` loop, `can_drain_pending_input` flag, `get_pending_input` / `record_pending_input` round-boundary drain.
- `codex-rs/core/src/state/turn.rs:137` — `clear_pending` on interrupt; pending_input is wiped before `TurnAborted`.
- `codex-rs/protocol/src/models.rs:911` — `Vec<UserInput> → ResponseInputItem::Message { role: "user", ... }` conversion.
- `codex-rs/tui/src/bottom_pane/chat_composer.rs:41-42,2895-2903` — Enter submits, Tab queues; documented inline.
- `codex-rs/tui/src/bottom_pane/mod.rs` — `DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED`, `QUIT_SHORTCUT_TIMEOUT`.
- `codex-rs/tui/src/bottom_pane/pending_input_preview.rs` — three-section queue render.
- `codex-rs/tui/src/chatwidget.rs:929-942,5469-5507,7523-7556` — `queued_user_messages` (Tab path), `submit_user_message` vs `queue_user_message`, `maybe_send_next_queued_input`.
- `codex-rs/tui/src/chatwidget.rs:3412-3450` — `on_interrupted_turn`: optional "submit pending steers as new turn after interrupt" path.
- `codex-rs/tui/src/status_indicator_widget.rs` — busy display.

### opencode (TypeScript)

Server-side state machine in `packages/opencode/src/session/run-state.ts` (`idle | busy | retry`). Each session owns a `Runner` (Effect-TS `InstanceState`); `cancel` aborts an active runner, idle is set directly. Cleaner separation than Claude Code or Codex: the TUI never owns turn state, only mirrors it.

**Cancellation flow.** Dual-press Esc — first press increments `store.interrupt`, second within a 5-second window calls `sdk.client.session.abort({ sessionID })`. `Runner.cancel` is an Effect combinator that abort-signals the HTTP stream and tool subprocesses together. No `"Interrupted by user"` marker — the partial assistant message is left mid-render with whatever streamed, and the status flips back to idle. The dual-press is _for cancel_, not exit.

**Queue.** A user-selectable `general.followup` setting picks one of two modes; the default is **`steer`**.

In **`steer` mode** (the default), Enter while busy fires `prompt_async` immediately. The server-side `Runner.ensureRunning` ignores the second invocation (returns the in-flight run's promise), but `SessionPrompt.prompt` always calls `createUserMessage` _before_ entering `loop`, so the new user row is **persisted to the transcript** even when the runner stays on its existing fiber (`packages/opencode/src/session/prompt.ts:1276-1294`). The single long-lived `runLoop` `while (true)` reloads `msgs = loadTranscript(sessionID)` on every iteration; when `step > 1`, any newly-arrived user text is wrapped in a `<system-reminder>` block (`prompt.ts:1453-1468`) telling the model to acknowledge the mid-task user message and continue. Net effect: the queued message lands in the same multi-step turn before the next sampling round, no abort.

In **`queue` mode** (opt-in), drafts are persisted client-side in a Solid store (`followup.v1`, workspace-scoped) and held until the session goes idle (`busy(session)` flips false), then auto-sent FIFO via a `createEffect` watcher (`packages/app/src/pages/session.tsx:1711-1724`). Rendered as `SessionFollowupDock` above the composer with Send-now / Edit affordances. This is the turn-boundary path.

Both modes converge at the same drain shape: a real persisted `MessageV2` `role: "user"` that the server's transcript reload picks up.

**Dual-press exit.** None — `app_exit` (`ctrl+c,ctrl+d,<leader>q`) is single-press when the prompt is empty. The dual-press machinery in opencode is specifically for interrupt, not for exit.

**Status indicator.** Minimal — 8-frame braille spinner at 80 ms, muted gray, in the assistant message header during streaming. No tool name, no elapsed time, no token count. The footer carries LSP / MCP status and permissions, but nothing turn-scoped.

**Sources:**

- `opencode/packages/app/src/components/prompt-input/submit.ts:155-162,427-431` — `promptAsync` fire-and-forget, `shouldQueue()` guard.
- `opencode/packages/app/src/context/settings.tsx:106-111` — default `general.followup = "steer"`.
- `opencode/packages/app/src/pages/session.tsx:1499-1504,1554-1558,1711-1724` — `busy()` predicate, `queueEnabled` accessor, idle-drain `createEffect`.
- `opencode/packages/app/src/pages/session/composer/session-composer-region.tsx:241-261` — `SessionFollowupDock` queue preview.
- `opencode/packages/opencode/src/effect/runner.ts:103-111` — `Runner.ensureRunning`: returns the in-flight run when called a second time, drops new work.
- `opencode/packages/opencode/src/session/prompt.ts:1276-1294` — `SessionPrompt.prompt`: `createUserMessage` _before_ `loop`, so new user rows land in the transcript even while a runner is busy.
- `opencode/packages/opencode/src/session/prompt.ts:1453-1468` — `<system-reminder>` wrapping for mid-loop user messages at `step > 1`.
- `opencode/packages/opencode/src/session/run-state.ts:76-84` — `SessionRunState.cancel` → fiber interrupt.
- `opencode/packages/tui/src/components/keybinds.ts:19` — `app_exit` single-press.
- `opencode/packages/tui/src/components/prompt/index.tsx:274-303` — Esc dual-press interrupt.
- `opencode/packages/tui/src/components/spinner.tsx` — 8-frame braille.

## Comparison

| Repo        | Run state location                   | Cancel keys                        | Cancel transport              | Tombstone                         | Queue drain timing                                                                             | Queue location                                | Pop / edit queued | Dual-press exit          | Busy hint                                          |
| ----------- | ------------------------------------ | ---------------------------------- | ----------------------------- | --------------------------------- | ---------------------------------------------------------------------------------------------- | --------------------------------------------- | ----------------- | ------------------------ | -------------------------------------------------- |
| Claude Code | implicit (`isLoading`)               | Esc; Ctrl+C overloaded             | `AbortController`             | `"Interrupted by user"` synthetic | mid-turn between tool waves (default `next`); `later` = turn-end                               | module-level array, FIFO + priorities         | Esc / Up pop      | 800 ms (Ctrl+C / Ctrl+D) | spinner only                                       |
| Codex       | distributed flags + protocol op      | Esc (steer-only); Ctrl+C           | `Op::Interrupt` actor message | none; queued restored to composer | mid-turn at sampling boundary (Enter steers) + Tab queue at turn-end                           | `pending_input` on TurnState + TUI VecDeque   | Alt+Up pops       | 1 s, currently disabled  | spinner + elapsed + tool name + "Esc to interrupt" |
| opencode    | server actor (`idle/busy/retry`)     | dual-press Esc within 5 s          | `session.abort(id)` Effect    | none                              | mid-turn via persisted user row + transcript reload (`steer` default); turn-end (`queue` mode) | persisted `MessageV2` rows + Solid store      | n/a               | none (single-press exit) | spinner only                                       |
| oxide-code  | `App` mirror + per-turn `agent_turn` | Esc / Ctrl+C cancel; Ctrl+C×2 exit | drop-the-future               | `(interrupted)` marker            | mid-turn at round boundary (turn-end fallback for tool-less turns)                             | per-turn `Vec` in `agent_turn` + `App` mirror | Esc pops          | 1 s (Ctrl+C / Ctrl+D)    | spinner + label + "Esc to interrupt"               |

## oxide-code Today

The TUI exposes Esc / Ctrl+C cancellation, dual-press Ctrl+C exit from idle, POSIX Ctrl+D, and mid-turn queued user prompts. Enter while busy appends to `App::pending_prompts: VecDeque<String>` (rendered as a dim preview above the input); `agent_turn` accumulates the same submits in a per-turn `Vec<String>` and splices each into `messages` as a trailing User message at every round boundary, so the model sees the new instructions in the very next API request without aborting in-flight work. Tool-less turns have no round boundary, so their queued prompts fall through to `App::finalize_idle()`'s drain. Cancellation drops the `agent_turn` future — reqwest closes the SSE stream, `kill_on_drop(true)` reaps subprocesses, and a dim italic `(interrupted)` marker lands in chat. The status bar colocates the active-key hint with the spinner: `Streaming · Esc to interrupt` / `Running {tool} · Esc to interrupt` / `Cancelling...` / `Press Ctrl+C again to exit`.

## Design Decisions for oxide-code

The picks lean toward the Codex precedent on most surfaces — it is the one Rust reference and its patterns map onto our async / actor architecture without reshaping. The wire-shape choice (3) is the deliberate exception.

1. **Drop-the-future cancellation, no in-loop checks.** Esc / Ctrl+C drops the `agent_turn` future from the agent loop; reqwest's SSE-close-on-drop and `kill_on_drop(true)` clean up automatically. Codex routes cancellation as a protocol message (`Op::Interrupt`); dropping the future is simpler for our shape and equally correct.
2. **Discard partial assistant state on cancel.** The cancelled assistant message and any orphan `tool_use` are not preserved; the TUI shows the streamed prefix with an `(interrupted)` suffix but the model sees a clean history. Claude Code's preserved tombstone gives the model continuity at the cost of a transcript-edit pass; we follow Codex (discard) until users report the model "forgetting".
3. **Mid-turn drain at the round boundary, raw consecutive User messages on the wire.** Each queued prompt becomes its own `User` message with one `Text` block; the request shape `[..., User(tool_results), User(text_1), User(text_2), ...]` is valid because Anthropic accepts consecutive same-role messages. Deliberately drops the `<system-reminder>The user sent the following message: ...</system-reminder>` envelope that Claude Code and OpenCode both wrap with — keeps persistence trivial (no display-layer envelope-stripping). May flip if observation shows the model ignoring mid-turn instructions.
4. **Queue authority across the cancel window.** Normal busy: agent buffer is canonical, TUI mirror confirms via `PromptDrained`. On cancel: agent buffer drops with the future, TUI mirror replays via `finalize_idle`. The TUI holds mid-turn submits locally while `Status::Cancelling` is showing — forwarding would race the cancel signal and let a fresh prompt slip ahead of the existing queue head.
5. **Cancellation does not auto-clear the queue.** A user who interrupts a wandering turn typically still wants their planned follow-up. Esc on idle pops the most-recent queued prompt into the textarea for editing; gated on the buffer being empty so a draft is never silently overwritten.
6. **`Status` is a projection of `App` state, not a separate machine.** `Idle | Streaming | ToolRunning { name } | Cancelling | ExitArmed { until }` — the tool name rides in the variant so the hint reads `Running bash · Esc to interrupt`. `set_active_status` short-circuits while `Cancelling` / `ExitArmed` so late-buffered events don't flip the bar back before the user reacts.
7. **Cancellation rides the existing `AgentEvent` channel.** `AgentEvent::Cancelled` covers both TUI (`(interrupted)` marker) and `StdioSink` (newline + marker on stderr). No new event channel, no new sink trait method.
8. **`agent_loop_task` fires `Error` _or_ `TurnComplete`, never both.** Both run the same TUI teardown (drain queue, re-enable input); emitting both double-drained the queue head on every API failure. `TurnAbort::Failed` sends `Error` only.

## Sources

- `crates/oxide-code/src/agent.rs` — `agent_turn` round loop, `await_unless_aborted` (biased race against `user_rx`), `record_drained_prompts` (splice queued prompts as trailing User messages + emit `PromptDrained`), `TurnAbort` enum (`Cancelled` / `Quit` / `Failed(anyhow)`).
- `crates/oxide-code/src/agent/event.rs` — `AgentEvent::{Cancelled, PromptDrained}`, `UserAction::{Cancel, ConfirmExit, Quit}`, `INTERRUPTED_MARKER`, `StdioSink::render` (writes `(interrupted)` on Cancelled), `inert_user_action_channel` (bare-REPL / headless inert receiver).
- `crates/oxide-code/src/agent/pending_calls.rs` — `clear()` evicts orphan tool calls at turn boundaries.
- `crates/oxide-code/src/main.rs` — `agent_loop_task` (one terminal event per `TurnAbort` arm), `bare_repl` / `headless` (inert `user_rx`, clean-exit translation).
- `crates/oxide-code/src/tool/bash.rs` — `kill_on_drop(true)` reaps subprocesses on cancel.
- `crates/oxide-code/src/tui/app.rs` — `App::pending_prompts`, `dispatch_user_action` / `apply_action_locally` (cancel-window FIFO hold), `finalize_idle` / `drain_pending_prompt` (turn-end fallback), `set_active_status` (sticky against `Cancelling` / `ExitArmed`), `expire_armed_exit` (1-second window).
- `crates/oxide-code/src/tui/components/chat/blocks/interrupted.rs` — `InterruptedMarker` block; shares `INTERRUPTED_MARKER`.
- `crates/oxide-code/src/tui/components/input.rs` — placeholder copy keyed off `(enabled, has_queued)`, Ctrl+D POSIX gate, Esc-pop refusal when buffer non-empty, `set_text` for queue-pop.
- `crates/oxide-code/src/tui/components/status.rs` — `Status::{Idle, Streaming, ToolRunning { name }, Cancelling, ExitArmed { until }}`, sentence-case busy hints, 8-dot Braille spinner.
