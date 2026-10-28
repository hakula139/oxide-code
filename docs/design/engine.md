# Engine

`Engine` owns the live session and runs one operation at a time. Every frontend (the TUI, the bare REPL, and a headless run) sends `Op` values on one channel and renders the `Event` values that come back, so behavior added in the engine reaches all three without frontend-specific code. This page defines the operation and event contract. [Terminal UI](tui.md), [Headless Mode](../guide/headless.md), and [Cancellation](cancellation.md) describe how each frontend consumes it.

## Operations

An `Op` is one of three kinds:

- **A turn.** `Prompt` starts one, `Queue` adds a prompt typed while it runs, `Interrupt` cancels it, and `Resolve` answers an outstanding approval.
- **A session operation.** `Compact`, `Clear`, `Archive`, `Resume`, and `Rename` change the session, `SetArchive`, `FindSaved`, and `DeleteSaved` look up or change a saved one, and `SetModel` and `SetMode` change the settings the next turn uses.
- **Sign-in and shutdown.** `Login`, `LoginCode`, and `Logout` manage the oxide-code login, and `Shutdown` ends the process.

`Op::is_idle_only` names the ops that require an idle engine. A frontend waits for the preceding operation's terminal event before sending one, and the engine drops an idle-only op that arrives during another operation without a terminal event, since the frontend still owns it. The other ops act on the operation in flight, so they arrive on the same channel and race the running stream and tool futures in one biased `select!`.

## Events

Events report progress, then end an operation. `Event` carries streamed text and thinking, tool calls and their results, usage, rate limits, session and configuration changes, retrying notices, and warnings.

Each attended operation ends with exactly one terminal event, unless a shutdown stops it first. A turn ends with `TurnComplete`, `TurnFailed`, or `Cancelled`, or with `TurnRolledBack` before a `TurnFailed` when the model refused, or with `PromptUnsent` before a `TurnFailed` or `Cancelled` when sign-in or automatic compaction stopped it before the prompt was recorded. `Event`'s rustdoc in `crates/ox-core/src/protocol.rs` names the terminal event of every other op, its failure case included. `TurnEnd::of` maps an op and an event to how the op ended, counting a `ConfigChanged` that rejects an `/effort` pick as a failure. The REPL and headless drivers render until the op they sent ends, and the TUI holds input from the start of an operation until its terminal event, then runs the teardown that submits the queued prompt. A failed `/resume` or `/archive` returns to idle with the queue still held, since its prompts were meant for the session it did not reach.

`Error` reports a failure that leaves the operation running, such as a failed session write. `Notice` warns about a condition the session works around, such as a drifted file, and `Retrying` is a dim notice for a request the client sends again.

## Turn Pipeline

A turn is a loop of rounds. Each round sends the transcript to the model, streams the reply, and runs the tool calls it asked for. The engine gates each call through [Permissions](permissions.md), runs the parallel-safe reads together, and appends each result as a tool result. The loop ends when the reply has no tool call, the model refuses, `max_tool_rounds` is reached, or the estimate for the next request crosses the compaction threshold, which hands the turn to [Compaction](compaction.md). A queued prompt is spliced in as a user message at the next round boundary, so the model sees it in the next request without aborting the work in flight.

## Design Decisions

1. **The engine owns every session change, through ops and events.** The TUI, the REPL, and a headless run differ only in input and output, so behavior that changes the session or the transcript lives in the engine, and each frontend renders the same events. A slash command that changes session state forwards an `Op`, while one that only opens a modal stays local.

2. **One terminal event per operation.** A frontend learns that an operation ended from one event, so a lost or reordered progress event cannot leave the input held. The event is the last one the operation emits, and a shutdown is the only way an operation ends without one.

3. **An idle-only op is dropped.** Queueing it would apply a model or session change at a moment the user did not choose, such as after a turn they already interrupted. Dropping it keeps the frontend's own copy authoritative, and the frontend resubmits when it is ready.

4. **Mid-turn controls share the op channel.** `Queue`, `Interrupt`, `Resolve`, `LoginCode`, `SetMode`, and `Shutdown` race the running futures in one `select!`, so a cancel, an approval, or a mode switch does not wait for a stream to finish, and the engine needs no second input path.

## Out of Scope

- **An ACP frontend for editors.** It would drive the same contract ([#150](https://github.com/hakula139/oxide-code/issues/150)).

## Sources

- `crates/ox-core/src/engine.rs`: `Engine`, `EngineHandle`, and the op loop.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round` and `Dispatcher`.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, the round loop, and `interrupt_turn`.
- `crates/ox-core/src/protocol.rs`: `Op`, `Event`, the per-op terminal-event docs, and `TurnEnd::of`.
- `crates/ox-tui/src/app/dispatch.rs`: `dispatch_op`, which sends each frontend action as an op.
- `crates/ox-tui/src/app/events.rs`: the TUI's event handling and operation teardown.
- `crates/ox/src/driver/stdio.rs`: `render_op`, which renders until the op ends.
