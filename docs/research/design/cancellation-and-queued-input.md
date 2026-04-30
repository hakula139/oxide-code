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
- `codex-rs/protocol/src/models.rs:911` — `Vec<UserInput> → ResponseInputItem::Message { role: "user", … }` conversion.
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

| Repo                 | Run state location                      | Cancel keys                        | Cancel transport              | Tombstone                         | Queue drain timing                                                                             | Queue location                                                   | Pop / edit queued | Dual-press exit          | Busy hint                                          |
| -------------------- | --------------------------------------- | ---------------------------------- | ----------------------------- | --------------------------------- | ---------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- | ----------------- | ------------------------ | -------------------------------------------------- |
| Claude Code          | implicit (`isLoading`)                  | Esc; Ctrl+C overloaded             | `AbortController`             | `"Interrupted by user"` synthetic | mid-turn between tool waves (default `next`); `later` = turn-end                               | module-level array, FIFO + priorities                            | Esc / Up pop      | 800 ms (Ctrl+C / Ctrl+D) | spinner only                                       |
| Codex                | distributed flags + protocol op         | Esc (steer-only); Ctrl+C           | `Op::Interrupt` actor message | none; queued restored to composer | mid-turn at sampling boundary (Enter steers) + Tab queue at turn-end                           | `pending_input` on TurnState + TUI VecDeque                      | Alt+Up pops       | 1 s, currently disabled  | spinner + elapsed + tool name + "Esc to interrupt" |
| opencode             | server actor (`idle/busy/retry`)        | dual-press Esc within 5 s          | `session.abort(id)` Effect    | none                              | mid-turn via persisted user row + transcript reload (`steer` default); turn-end (`queue` mode) | persisted `MessageV2` rows + Solid store                         | n/a               | none (single-press exit) | spinner only                                       |
| oxide-code (current) | `App::pending_prompts` + `Status` enum  | Esc / Ctrl+C cancel; Ctrl+C×2 exit | drop-the-future               | `(interrupted)` marker            | turn-end only (matches Codex's _secondary_ Tab path, opencode `queue` mode)                    | `App::pending_prompts: VecDeque<String>`                         | Esc pops          | 1 s (Ctrl+C / Ctrl+D)    | spinner + tool icon + "Esc to interrupt"           |
| oxide-code (target)  | `App` (display) + per-turn agent buffer | unchanged                          | unchanged                     | unchanged                         | mid-turn at round boundary inside `agent_turn`                                                 | per-turn `Vec<String>` in `agent_turn` + display mirror in `App` | unchanged         | unchanged                | unchanged                                          |

## oxide-code Today

Phase 1 has shipped: Esc / Ctrl+C cancel a busy turn by dropping the agent future, Ctrl+C / Ctrl+D from idle arm a 1-second double-press exit window, and the input area accepts keypresses while busy with `App::pending_prompts: VecDeque<String>` buffering them. The status bar surfaces `Streaming · Esc to interrupt` / `running tool · Esc to interrupt` / `cancelling...` / `Press Ctrl+C again to exit`, and a `(interrupted)` marker is appended to chat history when a cancel lands mid-stream. The queued-prompt preview renders above the input as dim user-prompt rows with a `+N more` overflow tag.

What is **not yet shipped** — and what this doc's `target` row covers — is the **mid-turn drain**. Today, `pending_prompts` is purely TUI-side: the head pops only when `App::finalize_idle()` runs, which fires from `AgentEvent::TurnComplete` / `Error` / `Cancelled`. The agent loop in `agent.rs::agent_turn` has no awareness that a follow-up has been typed; the user must wait for the multi-round turn to reach a text-only response (or hit Esc) before the next prompt fires. Among the three references this matches Codex's _secondary_ Tab-queues path and OpenCode's opt-in `queue` mode, but **none** of them ship that as the default UX — they all inject mid-turn at the round boundary.

Architecturally, the gap is that `agent_turn` does not see the user-action channel: that channel is owned by `main.rs::drive_turn`, which races `agent_turn` against `user_rx.recv()` purely for `Cancel` and explicitly drops mid-turn `SubmitPrompt` with a warn log. The agent loop body is `for _ in 0..MAX_TOOL_ROUNDS { stream_response → run tools → push tool_result_msg }` (`agent.rs:71-162`); the natural mid-turn injection seam is between the tool-result push and the next iteration's `stream_response`, but reaching that seam requires plumbing the user channel into `agent_turn` so it can drain a per-turn buffer at each round.

Cancellation, status state, and chat-block teardown are unchanged from phase 1: drop-the-future cancellation, `Status` projection from `App` state, `(interrupted)` markers via `ChatView::push_interrupted_marker`, and `pending_calls.clear()` at turn boundaries.

## Design Decisions for oxide-code

The shipping unit is three coupled features. Decisions span five surfaces: where cancellation lives, what happens to in-flight state, the key-to-action map, the queue model, and the visible feedback.

1. **Drop-the-future cancellation, no in-loop checks.** Race `agent_turn` against a per-turn `CancellationToken` in the agent loop, mirroring the bare REPL / headless `agent_turn` vs. `shutdown_signal` pattern (`main.rs:447-454`). Reqwest cleans up the SSE stream on drop, `kill_on_drop(true)` on `tokio::process::Child` (`tool/bash.rs:108`) kills bash subprocesses, and the actor-backed session writes are mpsc-queued so a dropped await still flushes. No `is_cancelled()` seams scattered through `agent_turn` itself — the loop stays straight-line. We use `tokio_util::sync::CancellationToken` rather than a bare `oneshot` because the token's `child_token()` lets a session-level cancel kill everything in flight when we ever need that (e.g., on `App` teardown).

2. **Discard partial assistant state on cancel; rely on resume-side sanitization for cross-session healing.** The cancelled assistant message and any orphan `tool_use` are not preserved in `messages`. The TUI commits the in-flight `StreamingAssistant` block with a dim italic `(interrupted)` suffix so the user sees what was on screen, but the model on the next turn sees a clean history. Claude Code's preserved-tombstone approach gives the model context but adds plumbing (synthesizing a `tool_result` against an orphan id, persisting through resume). Codex discards too, and the abort-and-retype workflow is fine when cancel is fast. Revisit only if users report the model "forgetting" what it was doing.

3. **Esc and Ctrl+C share cancel semantics while busy; Ctrl+C in idle is dual-press exit; Esc in idle pops queue.** Esc never exits — Claude Code's split. Ctrl+C overloads cancel-then-exit — Codex's split. While busy, both keys cancel; the user does not have to remember the difference. From idle, Ctrl+C arms a 1-second exit window (matches Codex; longer than Claude Code's 800 ms but a more comfortable double-tap). Ctrl+D is _not_ aliased to exit yet — defer until we know what users expect on macOS / SSH where Ctrl+D historically means EOF.

4. **Mid-turn drain at the round boundary, not turn-end.** Press Enter while busy appends to a FIFO and renders dim ghost user-messages above the input. The drain fires inside `agent_turn` between the tool-result push and the next `stream_response` — i.e. the same boundary at which Claude Code splices `queued_command` attachments and Codex drains `pending_input`. The model sees the new user text in the very next API request as part of the same multi-step turn, with no abort. This requires plumbing `user_rx` into `agent_turn` (replacing the current `drive_turn` shim that warns-and-drops mid-turn submits) and having the agent loop accumulate incoming `SubmitPrompt`s in a per-turn `Vec<String>`. If the assistant produces a text-only response with no tool calls in the very first round, there is no round boundary to drain at — the queued message falls through to the existing turn-end drain (still reachable via `App::finalize_idle()` after `AgentEvent::TurnComplete`). Phase 1 shipped only the turn-end half.

   The on-the-wire shape: extend the trailing tool-result `User` message with a single appended `ContentBlock::Text` block carrying the queued user text (joined with double newlines if multiple) wrapped in a `<system-reminder>The user sent the following message: …</system-reminder>` envelope. Anthropic accepts mixed-content `User` messages, the wrapper matches Claude Code and OpenCode and gives the model a reliable hook to acknowledge the mid-task interjection, and folding into the existing `User` message keeps the conversation cleaner than emitting a second consecutive same-role message.

5. **Cancellation does not auto-clear the queue.** A user who interrupts a wandering turn typically still wants their planned follow-up. Esc on a non-empty queue while idle pops the most recent queued back into the input textarea for editing — repeated Esc clears items one at a time. (Up arrow could share the popping affordance, matching Codex's Alt+Up; defer until Up's current behavior in the textarea is verified to not conflict.)

6. **Single source of truth: `RunState` enum on `App`.** Variants: `Idle | Busy(BusyKind) | Cancelling | ExitArmed { until: Instant }`, with `BusyKind = Streaming | Tool { name: String }`. Today's `Status` enum (`tui/components/status.rs:46-50`) becomes a _display projection_ of `RunState`, and `InputArea::enabled` becomes `state.allows_input()`. Two parallel state machines are exactly the kind of ad-hoc derivation we should consolidate before adding a third (queue-pending) on top.

7. **Status bar surfaces every state, with an actionable hint.** Codex's `(42s • Esc to interrupt)` is the model. `Idle` → "Idle". `Busy(Streaming)` → "Streaming · Esc to interrupt". `Busy(Tool { name })` → "{name} · Esc to interrupt". `Cancelling` → "Cancelling...". `ExitArmed` → "Press Ctrl+C again to exit". The hint colocates with the spinner so the user always knows the active key — no separate notification system.

8. **Input footer is dynamic.** `Idle` → `Enter: send · Shift+Enter: newline · Ctrl+C: quit`. `Busy(*)` → `Esc / Ctrl+C: interrupt · Enter: queue prompt`. `Idle` with non-empty queue → `Up / Esc: edit queued · Enter: send`. The footer is the most reliably-glanced surface for active controls; making it state-aware is cheap and avoids surprise.

9. **Cancellation flows through the existing `AgentEvent` channel.** Add `AgentEvent::Cancelled` (paired with a `Cancelled` arm in `StdioSink` that newlines like `TurnComplete`). The TUI handler treats it like `TurnComplete` plus the italic `(interrupted)` suffix on any in-flight `StreamingAssistant`. No new event channel, no new sink trait method.

10. **Bare REPL upgrades from "exit on Ctrl+C" to "abort the turn".** Today the bare REPL `break`s out (`main.rs:452`) on Ctrl+C, ending the session. Replace with a soft-cancel that returns the user to the prompt; only Ctrl+C-while-already-cancelling (or SIGTERM / SIGHUP) exits. Headless stays exit-on-cancel — there is no follow-up prompt to return to, and the existing Summary-write-on-signal path works.

11. **Per-turn token recreated by `agent_loop_task`, owned by `App` for cancellation.** Each iteration of `agent_loop_task` creates a fresh `CancellationToken` and hands a clone to `App` over the existing user-action channel (or a sibling channel — TBD during implementation). On Esc / Ctrl+C-while-busy, `App` calls `.cancel()` on the held clone. This ties cancellation lifetime to a turn, so a stale token from a prior turn cannot accidentally cancel the next one.

The decisions intentionally lean toward the _Codex_ model over the _Claude Code_ model on most surfaces (drop-and-discard cancellation, status-bar hint, FIFO queue with header preview, `pending_input` as a per-turn buffer drained at the round boundary) because Codex is the one Rust precedent and its patterns map onto our existing async / actor architecture without reshaping. The mid-turn drain shape on the wire is closer to Claude Code's `queued_command → UserMessage` with `<system-reminder>` wrapping — the `system-reminder` envelope is the part that the model is most reliably trained to acknowledge as a user interjection, and folding queued text into the trailing tool-result message avoids consecutive same-role messages. Claude Code's interrupted-tombstone preservation and `popAllEditable` are richer but require a transcript-edit pass we have not paid for; opencode's server-state machine is a cleaner separation than what we have today but would force the agent loop into a server-actor refactor we have not committed to.

## Sources

Shipped phase 1:

- `crates/oxide-code/src/agent.rs:61` — `agent_turn` round loop; the mid-turn drain seam will live just before each iteration's next `stream_response`.
- `crates/oxide-code/src/agent.rs:271` — `stream_response` SSE pump; drop-the-future cancellation works because reqwest closes the HTTP stream on drop and `mpsc::Receiver::recv` is cancel-safe.
- `crates/oxide-code/src/agent/event.rs` — `AgentEvent::Cancelled`, `UserAction::Cancel` / `ConfirmExit`.
- `crates/oxide-code/src/agent/pending_calls.rs` — `clear()` evicts orphan tool calls at turn boundaries.
- `crates/oxide-code/src/main.rs:447` — `drive_turn`: the `tokio::select!` shim that races `agent_turn` against `user_rx` for `Cancel`. Today it warns-and-drops mid-turn `SubmitPrompt`; the mid-turn refactor moves the receiver into `agent_turn` and accumulates submits into a per-turn buffer.
- `crates/oxide-code/src/tool/bash.rs` — `kill_on_drop(true)` reaps subprocesses on cancel.
- `crates/oxide-code/src/tui/app.rs:64,335,352` — `pending_prompts: VecDeque<String>`, `finalize_idle()`, `drain_pending_prompt()`. The mid-turn refactor changes `App` from "queue owner" to "display mirror": items are added on `Enter` and removed when an `AgentEvent::PromptDrained(uuid)` (or equivalent) lands.
- `crates/oxide-code/src/tui/components/chat/blocks/interrupted.rs` — `(interrupted)` marker; unchanged.
- `crates/oxide-code/src/tui/components/input.rs` — `InputArea` accepts keypresses while busy (placeholder hint flips to `"Type to queue a follow-up..."`); unchanged by the mid-turn refactor.
- `crates/oxide-code/src/tui/components/status.rs` — `Status::Cancelling` / `ExitArmed`, busy-state interrupt hint; unchanged by the mid-turn refactor.

Reference precedents (mid-turn drain mechanics):

- `claude-code/src/query.ts:1535-1643` — drain queued `next` priority items into `toolResults` before the next `callModel`; the `<system-reminder>` envelope on the appended user text.
- `codex-rs/core/src/session/turn.rs:373-469` — `can_drain_pending_input` flag, `get_pending_input` / `record_pending_input`, drain at sampling boundary.
- `opencode/packages/opencode/src/session/prompt.ts:1453-1468` — `<system-reminder>` wrapping for mid-loop user messages reloaded from the transcript.
