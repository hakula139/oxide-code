# Architecture

The `ox` binary is a single crate, `crates/oxide-code`, organized into layers of top-level modules. This map lists those modules and shows the direction of their imports. The first `//!` line in each file describes that file, so the map covers only the top level. To list the first doc line from every file:

```bash
grep -rm1 '^//!' crates/oxide-code/src
```

## Module Map

Layers run from the foundation to the frontends.

| Layer      | Module         | Responsibility                                                                                                                                      |
| ---------- | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Foundation | `file_tracker` | Per-session Read-before-Edit gate with xxh64 staleness detection                                                                                    |
| Foundation | `message`      | Conversation message and content-block types shared by the client, the session transcript, and the TUI                                              |
| Foundation | `util`         | Shared helpers for environment variables, the filesystem, git probes, logging, paths, mutex recovery, text width, local time, and TLS               |
| Domain     | `config`       | Layered configuration with per-value provenance, credentials, gateway settings, and stored OAuth logins with browser sign-in                        |
| Domain     | `model`        | Supported-model catalogue, per-model request capabilities, and pricing                                                                              |
| Domain     | `permission`   | Tool permission gate: modes, allow / ask / deny rules, and path resolution                                                                          |
| Domain     | `prompt`       | System prompt assembly, environment detection, and instruction-file discovery                                                                       |
| Services   | `client`       | The `Wire` streaming contract and the Anthropic Messages API client: request bodies and headers, a retrying transport, SSE, and one-shots           |
| Services   | `protocol`     | The serializable `Op` / `Event` contract between the engine and its frontends, its payloads, and the `EventSink` trait                              |
| Services   | `session`      | JSONL session persistence behind an actor task, plus listing, archiving, resume, transcript repair, and AI titles                                   |
| Services   | `tool`         | The `Tool` trait, the registry, and the built-in `bash`, `edit`, `glob`, `grep`, `read`, and `write` tools                                          |
| Agent      | `agent`        | The turn loop that streams a response and dispatches tool calls, plus compaction                                                                    |
| Engine     | `engine`       | The live conversation (transcript, prompt, usage, auto-compaction, session allow rules) as a task that every frontend drives through `EngineHandle` |
| Frontend   | `driver`       | The TUI, bare REPL, and headless drivers, which drive the `Engine` through `EngineHandle`, plus exit status and shutdown signals                    |
| Frontend   | `slash`        | Slash-command parser, registry, and built-in commands                                                                                               |
| Frontend   | `tui`          | Terminal UI: the `App` event loop, chat, input, and status components, modals, markdown rendering, and themes                                       |
| Frontend   | `main.rs`      | CLI parsing, config and session resolution, and handing a new `Engine` to a driver                                                                  |

## Dependency Direction

A module imports only from its own layer and lower layers.
