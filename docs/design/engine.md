# Engine

`Engine` owns the live session and runs one operation at a time. Every frontend, the TUI and a headless run, sends `Op` values on one channel and renders the `Event` values that come back, so behavior added in the engine reaches both without frontend-specific code. This page defines the operation and event contract. [Terminal UI](tui.md), [Headless Mode](../guide/headless.md), and [Cancellation](cancellation.md) describe how each frontend consumes it.

## Operations

An `Op` is one of three kinds:

- **A turn.** `Prompt` starts one, `Queue` adds a prompt typed while it runs, `Interrupt` cancels it, `Resolve` answers an outstanding approval, and `AnswerQuestion` answers a running `ask` call.
- **A session operation.** `Compact`, `Clear`, `Archive`, `Resume`, and `Rename` change the session, `SetArchive`, `FindSaved`, and `DeleteSaved` look up or change a saved one, and `SetModel` and `SetMode` change the settings the next turn uses.
- **Sign-in and shutdown.** `Login`, `LoginCode`, and `Logout` manage the oxide-code login, and `Shutdown` ends the process.

A frontend submits each op through its `OpSender`, which hands back the `OpId` the op took. Clones of a sender share one numbering, so each ID is unique among the engine's submissions, and the event that ends the op carries it.

`Op::is_idle_only` names the ops that require an idle engine. A frontend waits for the preceding operation to end before sending one, and the engine drops an idle-only op that arrives during another operation without ending it, since the frontend still owns it. The other ops act on the operation in flight, so they arrive on the same channel and race the running stream and tool futures in one biased `select!`.

## Events

Events report progress, then end an operation. `Event` carries streamed text and thinking, tool calls and their results, usage, rate limits, session and configuration changes, retrying notices, and warnings.

Each op the engine takes while idle ends with one `OpEnded { id, end }`, the last event the op emits while it runs, unless a shutdown stops the engine first. Background work the op started, such as the AI title or a background shell, may report afterward. An op the engine takes during another operation, such as `Queue` or `Interrupt`, acts on that operation and has no end of its own, and neither has an idle-only op the engine drops.

- **Failures.** `end` is an `OpEnd` of `Completed`, `Cancelled`, or `Failed`, and a failure reports its cause first, through `TurnFailed`, `RenameFailed`, a failed result such as `ArchiveUpdated`, or the `ConfigChanged` notice for an `/effort` pick the model has no level for. A turn the model refused reports `TurnRolledBack` before its `TurnFailed`, and a turn that sign-in or automatic compaction stopped before the prompt was recorded reports `PromptUnsent`.
- **Non-fatal reports.** `Error` reports a failure that leaves the operation running, such as a failed session write. `Notice` warns about a condition the session works around, such as a drifted file, and `Retrying` is a dim notice for a request the client sends again.
- **Frontends.** The headless driver renders until the `OpEnded` that carries the ID its submission returned, and its output keeps the documented event names, reporting a completed turn's end as `turn_complete`. The TUI holds input from the start of an operation until its `OpEnded`, then runs the teardown that submits the queued prompt, except where [**One operation in flight, ended by its own `OpEnded`**](cancellation.md#design-decisions) holds the queue.

## Turn Pipeline

A turn is a loop of rounds. Each round sends the transcript to the model, streams the reply, and runs the tool calls it asked for. The engine gates each call through [Permissions](permissions.md), runs the parallel-safe reads together, and appends each result as a tool result. The loop ends when the reply has no tool call, the model refuses, `max_tool_rounds` is reached, or the estimate for the next request crosses the compaction threshold, which hands the turn to [Compaction](compaction.md). A queued prompt is spliced in as a user message at the next round boundary, so the model sees it in the next request without aborting the work in flight.

## Design Decisions

1. **The engine owns every session change, through ops and events.** The TUI and a headless run differ only in input and output, so behavior that changes the session or the transcript lives in the engine, and each frontend renders the same events. A slash command that changes session state forwards an `Op`, while one that only opens a modal stays local.
2. **One end event per operation, under the op's ID.** A frontend learns that an operation ended from one event, so a lost or reordered progress event cannot leave the input held. The event is the last one the operation emits while it runs, and a shutdown is the only way an operation ends without one. The ID ties the end to its submission, so a frontend never takes the end of an op it did not wait on, such as a mode switch sent while idle, for the end of its own, and no table of which event ends which op has to track the engine.
3. **An idle-only op is dropped.** Queueing it would apply a model or session change at a moment the user did not choose, such as after a turn they already interrupted. Dropping it keeps the frontend's own copy authoritative, and the frontend resubmits when it is ready.
4. **Mid-turn controls share the op channel.** `Queue`, `Interrupt`, `Resolve`, `AnswerQuestion`, `LoginCode`, `SetMode`, and `Shutdown` race the running futures in one `select!`, so a cancel, an answer, or a mode switch does not wait for a stream to finish. The `ask` tool delivers its question through the tool context, and dispatch waits for the matching frontend answer on the same cancellation path. Dispatch owns the configured question and approval deadlines and emits `InteractionClosed` when the wait resolves, and frontends then close the matching prompt and discard its pending input. A timeout answer keeps its source and never becomes user-authored transcript text.
5. **Question answers retain user intent.** User-entered answers are ordinary user text beside the round's tool results, so the model receives the question and exact answer. History uses the tool result's display, and prompt recall skips messages that carry tool results.

## Out of Scope

- **An ACP frontend for editors.** It would drive the same contract ([#150](https://github.com/hakula139/oxide-code/issues/150)).

## Sources

- `crates/ox-core/src/engine.rs`: `Engine`, `EngineHandle`, and the op loop.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round` and `Dispatcher`.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, the round loop, and `interrupt_turn`.
- `crates/ox-core/src/protocol.rs`: `Op`, `OpSender`, `OpId`, `Event`, and `OpEnd`.
- `crates/ox-tui/src/app/dispatch.rs`: `dispatch_op`, which sends each frontend action as an op.
- `crates/ox-tui/src/app/events.rs`: the TUI's event handling and operation teardown.
- `crates/ox/src/driver/stdio.rs`: `run_op`, which sends an op and renders until it ends.
