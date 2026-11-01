# Architecture

List module responsibilities from their rustdoc:

```bash
grep -rm1 '^//!' crates/*/src
```

## Module Map

Layers run from the foundation to the frontends.

| Crate     | Layer      | Module           |
| --------- | ---------- | ---------------- |
| `ox-core` | Foundation | `file_tracker`   |
| `ox-core` | Foundation | `message`        |
| `ox-core` | Foundation | `util`           |
| `ox-core` | Domain     | `auth`           |
| `ox-core` | Domain     | `config`         |
| `ox-core` | Domain     | `git`            |
| `ox-core` | Domain     | `model`          |
| `ox-core` | Domain     | `permission`     |
| `ox-core` | Domain     | `prompt`         |
| `ox-core` | Services   | `client`         |
| `ox-core` | Services   | `command`        |
| `ox-core` | Services   | `protocol`       |
| `ox-core` | Services   | `session`        |
| `ox-core` | Services   | `tool`           |
| `ox-core` | Engine     | `engine`         |
| `ox-tui`  | Frontend   | `action`         |
| `ox-tui`  | Frontend   | `app`            |
| `ox-tui`  | Frontend   | `app_event`      |
| `ox-tui`  | Frontend   | `clipboard`      |
| `ox-tui`  | Frontend   | `components`     |
| `ox-tui`  | Frontend   | `cursor`         |
| `ox-tui`  | Frontend   | `git_probes`     |
| `ox-tui`  | Frontend   | `glyphs`         |
| `ox-tui`  | Frontend   | `grapheme`       |
| `ox-tui`  | Frontend   | `keymap`         |
| `ox-tui`  | Frontend   | `labels`         |
| `ox-tui`  | Frontend   | `markdown`       |
| `ox-tui`  | Frontend   | `modal`          |
| `ox-tui`  | Frontend   | `pending_calls`  |
| `ox-tui`  | Frontend   | `prompt_history` |
| `ox-tui`  | Frontend   | `session_info`   |
| `ox-tui`  | Frontend   | `slash`          |
| `ox-tui`  | Frontend   | `terminal`       |
| `ox-tui`  | Frontend   | `theme`          |
| `ox-tui`  | Frontend   | `viewport`       |
| `ox-tui`  | Frontend   | `wrap`           |
| `ox-tui`  | Frontend   | `lib.rs`         |
| `ox`      | Frontend   | `driver`         |
| `ox`      | Frontend   | `session_list`   |
| `ox`      | Frontend   | `main.rs`        |

## Dependency Direction

A module imports only from its own layer and lower layers. A helper that applies domain policy therefore belongs in the domain layer, which is why `git` sits beside `config`: it starts every `git` and `gh` probe with ox's secrets withheld, so no caller can leave them in the child's environment. `auth` sits in the Domain layer beside `config`, although it makes network and Keychain calls, because a resolved config holds the session's credential. `ox-tui` depends on `ox-core`, `ox` depends on both, and no dependency runs the other way. Only `ox-tui` depends on terminal UI crates, such as `ratatui` and `crossterm`.
