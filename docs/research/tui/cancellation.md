# Cancellation and Queued Input (Reference)

Research on TUI cancellation, exit, and input queueing across reference projects. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Claude Code (TypeScript)

Cancellation rides an `AbortController`; Esc cancels, Ctrl+C is overloaded for cancel-then-exit.

- **Cancel**: Esc aborts via `useCancelRequest.ts` -> `abortController.abort('interrupt')`. Partial assistant turn is preserved with an `"Interrupted by user"` synthetic `tool_result` against dangling `tool_use` ids. On resume, `conversationRecovery.ts` transforms to an `interrupted_prompt` + synthetic user message.
- **Dual-press exit**: `useDoublePress.ts` — 800 ms window for Ctrl+C / Ctrl+D. Esc never exits.
- **Queue**: Module-level `commandQueue` with priorities (`now > next > later`). Default is `next` (drain between tool waves, not turn-boundary). After every tool wave, queue is snapshotted and converted to `<system-reminder>`-wrapped user messages spliced into `toolResults`. Default Enter does not abort — only `priority === 'now'` triggers abort. Up-arrow / Esc on non-empty queue calls `popAllEditable()`.

## OpenAI Codex (Rust)

State distributed across `ChatWidget` flags. No monolithic enum — the protocol layer carries `Op::Interrupt` and `TurnAbortReason`.

- **Cancel**: Esc interrupts and submits pending steers (only if a queued steer exists). Ctrl+C is general interrupt — defers to bottom-pane first, then escalates to `AppCommand::interrupt()`. Routes as `Op::Interrupt` through the protocol.
- **Tombstone**: None. On interrupt the partial assistant message is discarded; queued / pending / rejected steers merge back into the composer.
- **Queue**: Two mechanisms. Enter -> `steer_input` -> `pending_input` on the active turn state (drained at sampling-iteration boundary, non-disruptive). Tab -> `queued_user_messages: VecDeque` (drained at turn boundary via `maybe_send_next_queued_input()`). Alt+Up pops the most recent queued message.
- **Dual-press exit**: Infrastructure exists (1 s timeout) but currently disabled. Ctrl+D is single-press exit.
- **Status**: `StatusIndicatorWidget` shows spinner + elapsed time + "Esc to interrupt" hint + tool name.

## opencode (TypeScript)

Server-side state machine (`idle | busy | retry`). Each session owns a `Runner` (Effect-TS).

- **Cancel**: Dual-press Esc (5-second window). `Runner.cancel` abort-signals the HTTP stream and tool subprocesses. No tombstone — partial assistant message stays mid-render.
- **Queue**: User-selectable `general.followup` setting. **Steer mode** (default): Enter while busy fires `prompt_async` immediately; new user row persisted to transcript, the long-lived `runLoop` reloads transcript and wraps in `<system-reminder>`. **Queue mode** (opt-in): drafts held client-side until session goes idle, then auto-sent FIFO.
- **Exit**: `app_exit` is single-press when prompt is empty. Dual-press is specifically for interrupt.
- **Status**: 8-frame braille spinner at 80 ms, muted gray. No tool name or elapsed time.

## Comparison

| Repo        | Cancel keys                         | Cancel transport        | Tombstone                         | Queue drain timing                                                                         | Pop / edit queued | Dual-press exit          | Busy hint                                          |
| ----------- | ----------------------------------- | ----------------------- | --------------------------------- | ------------------------------------------------------------------------------------------ | ----------------- | ------------------------ | -------------------------------------------------- |
| Claude Code | Esc; Ctrl+C overloaded              | `AbortController`       | `"Interrupted by user"` synthetic | mid-turn between tool waves (`next`); `later` = turn-end                                   | Esc / Up pop      | 800 ms (Ctrl+C / Ctrl+D) | spinner only                                       |
| Codex       | Esc (steer-only); Ctrl+C            | `Op::Interrupt` message | none; queued restored to composer | mid-turn at sampling boundary (Enter) + Tab at turn-end                                    | Alt+Up pops       | 1 s, currently disabled  | spinner + elapsed + tool name + "Esc to interrupt" |
| opencode    | dual-press Esc within 5 s           | `session.abort(id)`     | none                              | mid-turn via persisted user row + transcript reload (steer default); turn-end (queue mode) | n/a               | none (single-press exit) | spinner only                                       |
| oxide-code  | Esc / Ctrl+C cancel; Ctrl+C x2 exit | drop-the-future         | `(interrupted)` marker            | mid-turn at round boundary (turn-end fallback for tool-less turns)                         | Esc pops          | 1 s (Ctrl+C / Ctrl+D)    | spinner + label + "Esc to interrupt"               |
