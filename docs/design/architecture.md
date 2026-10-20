# Architecture

The `ox` binary consists of two crates organized into layers of top-level modules: the library `ox-core` in `crates/ox-core`, which holds every layer up to the engine, and the binary crate `ox` in `crates/ox`, which holds the frontends. This map lists those modules and shows the direction of their imports. The first `//!` line in each file describes that file, so the map covers only the top level. To list the first doc line from every file:

```bash
grep -rm1 '^//!' crates/*/src
```

## Module Map

Layers run from the foundation to the frontends.

| Crate     | Layer      | Module         | Responsibility                                                                                                                                  |
| --------- | ---------- | -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `ox-core` | Foundation | `file_tracker` | Per-session Read-before-Edit gate with xxh64 staleness detection                                                                                |
| `ox-core` | Foundation | `message`      | Conversation message and content-block types shared by the client, the engine, the session transcript, and the tools                            |
| `ox-core` | Foundation | `util`         | Shared helpers for environment variables, the filesystem, git probes, logging, paths, mutex recovery, text width, local time, and TLS           |
| `ox-core` | Domain     | `config`       | Layered configuration with per-value provenance, credentials, gateway settings, and stored OAuth logins with browser sign-in                    |
| `ox-core` | Domain     | `model`        | Supported-model catalog, per-model request capabilities, token usage, and pricing                                                               |
| `ox-core` | Domain     | `permission`   | Tool permission gate: modes, deny / ask / allow rules, shipped defaults, path resolution, and shell-command parsing                             |
| `ox-core` | Domain     | `prompt`       | System prompt assembly, environment detection, and instruction-file discovery                                                                   |
| `ox-core` | Services   | `client`       | The `Wire` streaming contract and the Anthropic Messages API client: request bodies and headers, a retrying transport, SSE, and one-shots       |
| `ox-core` | Services   | `command`      | The slash-command line parser, the command aliases, and the typed forms parsed into the `Op` each one runs, shared by the TUI and the bare REPL |
| `ox-core` | Services   | `protocol`     | The serializable `Op` / `Event` contract between the engine and its frontends, its payloads, and the `EventSink` trait                          |
| `ox-core` | Services   | `session`      | JSONL session persistence behind an actor task, plus listing, archiving, resume, transcript repair, and AI titles                               |
| `ox-core` | Services   | `tool`         | The `Tool` trait, the registry, and the built-in `bash`, `bash_output`, `bash_stop`, `edit`, `glob`, `grep`, `read`, `tasks`, and `write` tools |
| `ox-core` | Engine     | `engine`       | The live conversation as a task that every frontend drives through `EngineHandle`, with its turn loop, tool dispatch, and compaction            |
| `ox`      | Frontend   | `driver`       | The TUI, bare REPL, and headless drivers, which drive the `Engine` through `EngineHandle`, plus exit status and shutdown signals                |
| `ox`      | Frontend   | `slash`        | Slash-command registry and built-in commands                                                                                                    |
| `ox`      | Frontend   | `tui`          | Terminal UI: the `App` event loop, chat, input, status, and task panel components, modals, markdown rendering, and themes                       |
| `ox`      | Frontend   | `main.rs`      | CLI parsing, config and session resolution, and handing a new `Engine` to a driver                                                              |

## Dependency Direction

A module imports only from its own layer and lower layers, so `ox` depends on `ox-core` and never the reverse. `ox-core` depends on no terminal UI crate, such as `ratatui` or `crossterm`, and `deny.toml` allows only `ox` to depend on each frontend crate directly.
