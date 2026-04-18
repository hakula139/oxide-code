# Sessions

oxide-code automatically saves every conversation to disk. You can list past sessions and resume where you left off.

## How It Works

Each time you start `ox`, a new session file is created under `$XDG_DATA_HOME/ox/sessions/` (see table below). Every message (yours and the assistant's) is appended to this file in real time. When you quit, a summary entry is written so future listings can display the session title without reading the full conversation.

The storage directory follows XDG conventions:

| Variable         | Default          | Path                          |
| ---------------- | ---------------- | ----------------------------- |
| `$XDG_DATA_HOME` | `~/.local/share` | `$XDG_DATA_HOME/ox/sessions/` |

## Listing Sessions

```bash
ox --list   # or ox -l
```

Prints a table of recent sessions (newest first, local time):

```text
ID         Created             Msgs   Title
a1b2c3d4   2026-04-16 20:00    12     Fix authentication bug
e5f6a7b8   2026-04-15 17:30    5      Add session persistence
```

## Resuming a Session

Resume the most recent session:

```bash
ox -c              # short form
ox --continue      # long form
```

Resume a specific session by ID prefix:

```bash
ox -c a1b2
```

When resuming, the full conversation history is loaded and sent to the model as context. New messages are appended to the existing session file, so the conversation keeps its original session ID. An advisory file lock prevents two processes from writing to the same session simultaneously.

`ox` also sanitizes the loaded conversation before the next API call: if the previous run crashed between a tool call and its result, the unresolved tool call is dropped so the API accepts the resumed state.

If no sessions exist, or if the prefix matches zero or multiple sessions, `ox` prints an error and exits.

## Session Files

Session files are plain JSONL (one JSON object per line). You can inspect them directly:

```bash
head -1 ~/.local/share/ox/sessions/*.jsonl   # view session headers
```

On Unix, session files are created with user-only permissions (`0o600`) because they contain verbatim tool output that may include secrets from bash commands.

Each file contains these line types (tagged by `type`):

1. A **header** on the first line — session metadata (ID, working directory, model, timestamp, format version).
2. **Message** lines with the full conversation (user / assistant turns, tool calls, tool results), each with a stable `uuid` and chain link (`parent_uuid`) to the previous message.
3. **Title** lines — the session title (truncated first user prompt by default; may be replaced later by an AI-generated or user-provided title). The latest occurrence wins.
4. A **summary** line on clean exit with the final message count. Missing if the session was interrupted.

## Headless and REPL Modes

Sessions are recorded across all modes:

- **TUI** (`ox`): session saved automatically.
- **Bare REPL** (`ox --no-tui`): session saved automatically.
- **Headless** (`ox -p "prompt"`): single-turn session saved (useful for audit trails).
