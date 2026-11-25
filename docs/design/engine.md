# Engine

`Engine` owns the live session and runs one operation at a time. Frontends (TUI and headless) send `Op` values on one channel and render returned `Event` values so engine behavior reaches both uniformly. [Terminal UI](tui.md), [Headless Mode](../guide/headless.md), and [Cancellation](cancellation.md) cover frontend consumption.

## Operations

An `Op` falls into three groups:

- **Turns.** `Prompt` starts one, `Queue` appends mid-turn input, `Interrupt` cancels it, `Resolve` answers an approval, and `AnswerQuestion` answers an `ask` call.
- **Sessions and settings.** `Compact`, `Clear`, `Archive`, `Resume`, and `Rename` modify the live session, `SetArchive`, `FindSaved`, and `DeleteSaved` operate on saved sessions, and `SetModel` and `SetMode` update settings for subsequent turns.
- **Sign-in and shutdown.** `Login`, `LoginCode`, and `Logout` manage the oxide-code login, and `Shutdown` exits.

`OpSender` assigns each submitted op a unique `OpId` from a counter shared across sender clones, echoed on the ending event. `Op::is_idle_only` marks ops requiring an idle engine. Frontends wait for the active op to end before sending one, and the engine drops idle-only ops arriving while busy without emitting `OpEnded` (leaving ownership with the frontend). Non-idle ops share the same channel and race in-flight streams and tools inside one biased `select!`.

## Events

Events report streamed text and thinking, tool calls and results, usage, rate limits, session and config updates, retry notices, and warnings.

Every op accepted while idle ends with one `OpEnded { id, end }` as its final in-band event unless `Shutdown` stops the engine first. Background work the op started (such as AI titles or background shells) may emit later. Ops handled mid-operation (such as `Queue` or `Interrupt`) and dropped idle-only ops emit no `OpEnded`.

- **Operation ends.** `OpEnd` is `Completed`, `Cancelled`, or `Failed`. Failures emit their cause first (such as `TurnFailed`, `RenameFailed`, a failed `ArchiveUpdated`, or `ConfigChanged` for unsupported effort). Refused turns emit `TurnRolledBack` before `TurnFailed`, and pre-record stops (sign-in or auto-compaction) emit `PromptUnsent`.
- **Non-fatal reports.** `Error` reports a non-fatal fault (such as a session write error), `Notice` warns about handled conditions (such as file drift), and `Retrying` reports client retries.
- **Frontends.** Headless runs render until the matching `OpEnded` (mapping completed turns to `turn_complete`). The TUI holds input until `OpEnded`, then runs teardown and submits the queue head unless [**One operation in flight, ended by its own `OpEnded`**](cancellation.md#design-decisions) holds the queue.

## Turn Pipeline

Each round sends the transcript, streams the reply, gates tool calls through [Permissions](permissions.md), runs parallel-safe reads concurrently, and appends results. The loop stops when the reply has no tool calls, the model refuses, `max_tool_rounds` is hit, or the next-request estimate triggers [Compaction](compaction.md). Queued prompts splice in as user messages at the next round boundary without interrupting in-flight work.

## Design Decisions

1. **The engine owns every session change, through ops and events.** Keeping session and transcript mutations in the engine keeps TUI and headless behavior identical while UI-only modals stay local.
2. **One end event per operation, under the op's ID.** A single terminal event prevents lost or reordered progress events from wedging input (only `Shutdown` skips it). Matching by `OpId` ensures frontends never confuse an unawaited op (such as an idle mode switch) with their own and removes per-op end-event tables.
3. **An idle-only op is dropped.** Queueing would apply model or session changes at unintended times (such as after an interrupted turn). Dropping leaves the frontend authoritative to resubmit when ready.
4. **Mid-turn controls share the op channel.** Racing `Queue`, `Interrupt`, `Resolve`, `AnswerQuestion`, `LoginCode`, `SetMode`, and `Shutdown` in one `select!` avoids waiting for streams to finish. `ask` delivers questions through its tool context and awaits answers on the same cancellation path. Dispatch owns question and approval deadlines and emits `InteractionClosed` so frontends dismiss prompts and discard pending input, keeping timeout answers distinct from user-authored text.
5. **Question answers retain user intent.** User-entered answers sit as ordinary user text beside the round's tool results so the model sees both question and exact answer, while history displays the tool result and prompt recall skips tool-result messages.

## Out of Scope

- **An ACP frontend for editors.** Would drive the same contract ([#150](https://github.com/hakulab/oxide-code/issues/150)).

## Sources

- `crates/ox-core/src/engine.rs`: `Engine`, `EngineHandle`, and the op loop.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round` and `Dispatcher`.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, the round loop, and `interrupt_turn`.
- `crates/ox-core/src/protocol.rs`: `Op`, `OpSender`, `OpId`, `Event`, and `OpEnd`.
- `crates/ox-tui/src/app/dispatch.rs`: `dispatch_op`, which sends each frontend action as an op.
- `crates/ox-tui/src/app/events.rs`: the TUI's event handling and operation teardown.
- `crates/ox/src/driver/stdio.rs`: `run_op`, which sends an op and renders until it ends.
