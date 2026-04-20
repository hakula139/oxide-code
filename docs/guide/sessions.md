# Sessions

oxide-code automatically saves every conversation to disk. You can list past sessions and resume where you left off.

## How It Works

Each time you send a message in `ox`, the conversation is appended to a JSONL file under `$XDG_DATA_HOME/ox/sessions/{project}/`, where `{project}` is a filesystem-safe subdirectory name derived from the current working directory. The file is created lazily on the first message, so launching `ox` and quitting without typing leaves no trace behind. When you quit, a summary entry is written so future listings can display the session title without reading the full conversation.

The storage directory follows XDG conventions:

| Variable         | Default          | Path                                                                 |
| ---------------- | ---------------- | -------------------------------------------------------------------- |
| `$XDG_DATA_HOME` | `~/.local/share` | `$XDG_DATA_HOME/ox/sessions/{project}/{unix_timestamp}-{uuid}.jsonl` |

The project subdirectory is derived from the CWD by replacing path separators and other reserved characters with `-`; very long paths are truncated and suffixed with a stable hash so two distinct projects never collide. The per-file timestamp prefix keeps `ls` output chronological; `ox --list` uses the file's mtime for sort order so recently-resumed sessions bubble to the top.

## Listing Sessions

```bash
ox --list        # sessions in the current project
ox -la           # same, across every project
```

Prints a table of recent sessions (most recently active first, local time):

```text
ID         Last Active         Msgs   Title
a1b2c3d4   2026-04-18 09:20    12     Fix authentication bug
e5f6a7b8   2026-04-17 17:30    5      Add session persistence
```

With `--all`, a `Project` column is inserted so cross-project rows stay disambiguable (paths under `$HOME` are rendered as `~/...`):

```text
ID         Last Active         Msgs   Project          Title
a1b2c3d4   2026-04-18 09:20    12     ~/work/oxide     Fix authentication bug
9a0b1c2d   2026-04-18 08:05    3      ~/scratch        Investigate UTF-8 truncation
```

Titles that would overflow the terminal width are truncated with `...`. When output is piped (e.g., into `less`), titles render untruncated so downstream tools can wrap at their own width.

## Resuming a Session

Resume the most recent session in the current project:

```bash
ox -c              # short form
ox --continue      # long form
```

Resume a specific session by ID prefix (searches the current project):

```bash
ox -c a1b2
```

Widen the search to every project with `--all` / `-a`:

```bash
ox -c --all        # latest session anywhere
ox -c a1b2 -a      # prefix match across all projects
```

Resume from an explicit file path — useful for sessions copied between machines or living outside the configured store root:

```bash
ox -c ./conversation.jsonl
ox -c /home/me/archive/2026-04/session.jsonl
```

An argument is treated as a path when it contains a path separator or ends with `.jsonl`; otherwise it's matched against session IDs.

When resuming, the full conversation history is loaded and sent to the model as context. New messages are appended to the existing session file, so the conversation keeps its original session ID.

Two processes can resume the same session at the same time — this is intentional. Each process picks up the transcript as it exists at load time and appends new messages; the recorded messages form a UUID chain, so on the next resume `ox` walks the chain and follows the newest branch. A "losing" branch still lives in the file (you can inspect it manually) but is invisible to subsequent replays.

`ox` also sanitizes the loaded conversation before the next API call. If the previous run crashed mid-turn, unresolved tool calls are dropped, orphan tool results are dropped, any adjacent same-role turns left behind are merged, and synthetic user / assistant sentinels are injected at the edges when needed, so the API accepts the resumed state.

If the session file ever fails to write (disk full, permission change, etc.), `ox` reports the first failure inline and keeps the conversation going in memory. Further write errors are logged but not re-surfaced, so a temporary disk hiccup does not flood the UI.

If no sessions exist, or if the prefix matches zero or multiple sessions, `ox` prints an error and exits.

## Session Files

Session files are plain JSONL (one JSON object per line). You can inspect them directly:

```bash
head -1 ~/.local/share/ox/sessions/*/*.jsonl   # view session headers
```

On Unix, session files are created with user-only permissions (`0o600`) because they contain verbatim tool output that may include secrets from bash commands.

Each file contains these line types (tagged by `type`):

1. A **header** on the first line — session metadata (ID, working directory, model, timestamp, format version).
2. **Message** lines with the full conversation (user / assistant turns, tool calls, tool results), each with a stable `uuid` and chain link (`parent_uuid`) to the previous message.
3. **Title** lines — the session title. A first-prompt title is written immediately on the first user message. In the TUI, a background Haiku call also generates a concise AI title (3-7 words) and appends it shortly after; the latest-updated title wins in the tail scan, so listings and resumes surface the AI-generated one once it lands. Failures keep the first-prompt title in place.
4. A **summary** line on clean exit with the final message count. Missing if the session was interrupted.

## Headless and REPL Modes

Sessions are recorded across all modes:

- **TUI** (`ox`): session saved automatically.
- **Bare REPL** (`ox --no-tui`): session saved automatically.
- **Headless** (`ox -p "prompt"`): single-turn session saved (useful for audit trails).
