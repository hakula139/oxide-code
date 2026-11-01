# Architecture

The `ox` binary runs the same agent engine through a terminal UI, a bare REPL, or a headless driver.

## Crates

| Crate                                        | Owns                                                                        | Depends On             |
| -------------------------------------------- | --------------------------------------------------------------------------- | ---------------------- |
| [`ox-core`](../../crates/ox-core/src/lib.rs) | Agent behavior, model requests, tools, permissions, and saved sessions      | Neither frontend crate |
| [`ox-tui`](../../crates/ox-tui/src/lib.rs)   | Terminal interaction and presentation                                       | `ox-core`              |
| [`ox`](../../crates/ox/src/main.rs)          | CLI parsing, startup, frontend selection, and the stdio drivers in `driver` | `ox-core`, `ox-tui`    |

Only `ox-tui` depends on terminal UI libraries such as ratatui and crossterm.

## Core Module Map

Layers are ordered from shared primitives to orchestration. A module imports from its own layer or a lower layer.

| Layer      | Modules                                                  | Ownership                                                                                                           |
| ---------- | -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Foundation | `message`, `file_tracker`, `util`                        | Conversation types, read-before-write tracking, and shared filesystem, text, and runtime primitives                 |
| Domain     | `auth`, `config`, `git`, `model`, `permission`, `prompt` | Credential and configuration resolution, repository probes, model settings, access policy, and prompt construction  |
| Services   | `client`, `command`, `protocol`, `session`, `tool`       | Model transport, shared slash-command parsing, the frontend contract, persistent sessions, and tool implementations |
| Engine     | `engine`                                                 | Operation scheduling, agent turns, tool dispatch, compaction, and the live transcript                               |

`permission` decides whether an access is allowed. `engine::dispatch` applies that policy to calls, obtains approvals, and invokes tools. `tool` owns their preparation and execution.

`session` owns storage and saved-session queries. `engine::transcript::LiveTranscript` coordinates changes to live messages with the writes that record them. Session-changing behavior belongs in the engine so every frontend observes the same result.

Layer placement follows responsibility. `git` applies the credential policy to every repository probe, withholding ox's secrets because repository configuration can select programs Git executes. `auth` belongs beside `config` because credential resolution is part of session configuration, even when it needs Keychain access or HTTP.

## Execution Flow

1. `ox::main` resolves configuration, credentials, and the selected session, constructs an `Engine`, and starts the frontend.
2. The frontend submits `protocol::Op` values through `EngineHandle::ops`. The engine runs independently and returns `protocol::Event` values. The operation channel is bounded, while events are unbounded so rendering never blocks engine progress.
3. `engine::turn` sends the transcript through `client`, streams the response, and passes requested tools to `engine::dispatch`. Tool results enter the recorded transcript before the next model request.
4. The frontend renders progress and session changes. It waits for the active operation's `OpEnded` before submitting another idle-only operation.

[Engine](engine.md) defines operation ordering and turn behavior. [Client](client.md), [Permissions](permissions.md), and [Persistence](persistence.md) describe the corresponding service boundaries.

## Terminal UI

[`App`](../../crates/ox-tui/src/app.rs) owns the frontend's operation lifecycle and composes the chat, input, status, and modal state. Its event loop handles terminal input, engine events, background results, and frame deadlines. Components own their local interaction and rendering state.

Background work returns through [`AppEvent`](../../crates/ox-tui/src/app_event.rs), keeping host probes, clipboard operations, and picker queries off the interaction path. State updates request a redraw, and the frame clock coalesces rendering.

For example, [`prompt_history`](../../crates/ox-tui/src/prompt_history.rs) owns prompt search, preview, and recall into the composer. `session::store::prompt_history` supplies the saved-prompt query, while `modal::pages` runs page reads and delivers their results through `AppEvent`.

[Terminal UI](tui.md) covers component composition, rendering, and input handling. [Modals](modals.md) describes picker interaction and background paging.
