# Architecture

The `ox` binary consists of three crates organized into layers of top-level modules. The library `ox-core` in `crates/ox-core` holds the layers from the foundation through the engine. The frontends sit on top, with the terminal UI in the library `ox-tui` in `crates/ox-tui`, and the CLI, the bare REPL, and headless mode in the binary crate `ox` in `crates/ox`. This map lists those modules and shows the direction of their imports. The first `//!` line in each file describes that file, so the map covers only the top level. To list the first doc line from every file:

```bash
grep -rm1 '^//!' crates/*/src
```

## Module Map

Layers run from the foundation to the frontends.

| Crate     | Layer      | Module          | Responsibility                                                                                                                                                                          |
| --------- | ---------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ox-core` | Foundation | `file_tracker`  | Per-session Read-before-Edit gate with xxh64 staleness detection                                                                                                                        |
| `ox-core` | Foundation | `message`       | Conversation message and content-block types shared by the client, the engine, the session transcript, and the tools                                                                    |
| `ox-core` | Foundation | `util`          | Shared helpers for environment variables, the filesystem, git probes, logging, paths, server error responses, mutex recovery, text width, local time, TLS, TOML, and enum variant names |
| `ox-core` | Domain     | `config`        | Layered configuration with per-value provenance, credentials, gateway settings, and stored OAuth logins with browser sign-in                                                            |
| `ox-core` | Domain     | `model`         | Supported-model catalog, per-model request capabilities, effort and prompt cache TTL, token usage, and pricing                                                                          |
| `ox-core` | Domain     | `permission`    | Tool permission gate: modes, deny / ask / allow rules, shipped defaults, path resolution, shell-command parsing, and project file containment                                           |
| `ox-core` | Domain     | `prompt`        | System prompt assembly, environment detection, and instruction-file discovery                                                                                                           |
| `ox-core` | Services   | `client`        | The `Wire` streaming contract and the Anthropic Messages API client: request bodies and headers, a retrying transport, SSE, and side queries                                            |
| `ox-core` | Services   | `command`       | The slash-command line parser, the command aliases, and the typed forms parsed into the `Op` each one runs, shared by the TUI and the bare REPL                                         |
| `ox-core` | Services   | `protocol`      | The serializable `Op` / `Event` contract between the engine and its frontends, its payloads, the notices that word events, and the `EventSink` trait                                    |
| `ox-core` | Services   | `session`       | JSONL session persistence behind an actor task, plus listing, archiving, resume, transcript repair, and AI titles                                                                       |
| `ox-core` | Services   | `tool`          | The `Tool` trait, the registry, and the built-in `bash`, `bash_output`, `bash_stop`, `edit`, `glob`, `grep`, `read`, `tasks`, and `write` tools                                         |
| `ox-core` | Engine     | `engine`        | The live conversation as a task that every frontend drives through `EngineHandle`, with its turn loop, tool dispatch, and compaction                                                    |
| `ox-tui`  | Frontend   | `action`        | What the input box, slash commands, modals, and named keys ask the `App` to do                                                                                                          |
| `ox-tui`  | Frontend   | `app`           | The root `App`, whose `tokio::select!` loop multiplexes terminal input, engine events, background reports, and the render tick                                                          |
| `ox-tui`  | Frontend   | `clipboard`     | Clipboard writes for chat selections through the system clipboard, a tmux paste buffer, or OSC 52                                                                                       |
| `ox-tui`  | Frontend   | `components`    | The chat, input, status, task panel, and welcome views                                                                                                                                  |
| `ox-tui`  | Frontend   | `cursor`        | Terminal-cursor placement for the chat input and the modal line editors                                                                                                                 |
| `ox-tui`  | Frontend   | `git_probes`    | Background git and `gh` probes behind the status bar's branch and pull-request segments                                                                                                 |
| `ox-tui`  | Frontend   | `glyphs`        | Visual glyphs shared by the TUI and the stdio drivers                                                                                                                                   |
| `ox-tui`  | Frontend   | `keymap`        | Named key actions, each binding a key to its footer hint                                                                                                                                |
| `ox-tui`  | Frontend   | `labels`        | Labels for the resolved settings that `/status`, `/config`, and the welcome screen show                                                                                                 |
| `ox-tui`  | Frontend   | `markdown`      | Markdown rendering with syntax highlighting                                                                                                                                             |
| `ox-tui`  | Frontend   | `modal`         | The modal stack and the shared pickers, overviews, and editors built on it                                                                                                              |
| `ox-tui`  | Frontend   | `pending_calls` | Tool-call correlation shared by live streaming and transcript resume                                                                                                                    |
| `ox-tui`  | Frontend   | `session_info`  | The `App`'s snapshot of the running session, which slash commands and the welcome screen read                                                                                           |
| `ox-tui`  | Frontend   | `slash`         | Slash-command registry and built-in commands                                                                                                                                            |
| `ox-tui`  | Frontend   | `terminal`      | Terminal setup and restore, the panic hook, escape-sequence output, and the stdout width query                                                                                          |
| `ox-tui`  | Frontend   | `theme`         | Theme palette, built-in themes, and theme file loading                                                                                                                                  |
| `ox-tui`  | Frontend   | `viewport`      | Wrapping list navigation and modal scroll windows                                                                                                                                       |
| `ox-tui`  | Frontend   | `wrap`          | Word wrap with continuation indent for styled lines                                                                                                                                     |
| `ox-tui`  | Frontend   | `lib.rs`        | `run`, which drives the `Engine` through `EngineHandle` in the TUI, and the `Startup` state it opens with                                                                               |
| `ox`      | Frontend   | `driver`        | The bare REPL and headless drivers, which drive the `Engine` through `EngineHandle`, plus exit status and shutdown signals                                                              |
| `ox`      | Frontend   | `session_list`  | The `ox --list` table of the active or archived sessions in this project or every project                                                                                               |
| `ox`      | Frontend   | `main.rs`       | CLI parsing, config and session resolution, and handing a new `Engine` to a driver or the TUI                                                                                           |

## Dependency Direction

A module imports only from its own layer and lower layers. `ox-tui` depends on `ox-core`, `ox` depends on both, and no dependency runs the other way. Only `ox-tui` depends on terminal UI crates, such as `ratatui` and `crossterm`. The crate manifests encode this direction and the terminal UI rule, and review of manifest changes enforces both, so `deny.toml` holds no crate-layering bans.
