# Session Persistence

Research notes on session persistence design: storage format, listing strategy, and reference implementations.

## Storage Format

### JSONL (newline-delimited JSON)

Every session is a single `.jsonl` file — one JSON object per line.

- **Append-only** — crash-safe with write-then-flush. No need to rewrite the file on each message.
- **Streamable** — can read / write incrementally without loading the entire file into memory.
- **Universal** — used by claude-code, OpenAI Codex, and learn-claude-code.

### Entry Types

Each line is a discriminated union with a `type` field:

| Type      | Position     | Purpose                                                        |
| --------- | ------------ | -------------------------------------------------------------- |
| `header`  | First line   | Session metadata: ID, CWD, model, created timestamp            |
| `message` | Middle lines | Conversation messages (user / assistant) with timestamp        |
| `summary` | Last line    | Title, updated timestamp, message count (enables fast listing) |

Example file:

```jsonl
{"type":"header","session_id":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","cwd":"/home/user/project","model":"claude-opus-4-6","created_at":"2026-04-16T12:00:00Z"}
{"type":"message","message":{"role":"user","content":[{"type":"text","text":"hello"}]},"timestamp":"2026-04-16T12:00:01Z"}
{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-04-16T12:00:02Z"}
{"type":"summary","title":"Greeting conversation","updated_at":"2026-04-16T12:00:02Z","message_count":2}
```

### Why Not Raw Messages

The `message` entries wrap `Message` objects (already `Serialize` / `Deserialize`) with a timestamp. The header and summary entries carry metadata that would otherwise require parsing every message to reconstruct.

## Storage Location

Sessions live in `$XDG_DATA_HOME/ox/sessions/`, falling back to `~/.local/share/ox/sessions/`. This follows XDG conventions — config in `XDG_CONFIG_HOME`, data in `XDG_DATA_HOME`.

File naming: `{session_id}.jsonl` where session ID is a UUID v4.

## Session Listing

`ox --list` reads each session file's first line (header) and last ~4 KB (tail scan for summary). This avoids parsing the entire file.

- **Header** → session ID, CWD, model, created timestamp.
- **Summary** → title (derived from first user prompt), updated timestamp, message count.
- Sessions without a summary line (e.g., crash before exit) still list — they show "(untitled)" with no message count. Sessions with a summary but no user messages show "(empty session)" as the title.

## Session Resume

`ox -c` resumes the most recent session. `ox -c <prefix>` resumes by session ID prefix match.

Resume reopens the **existing** session file in append mode. Messages are loaded into memory and sent to the model as context. New messages are appended to the same file, and a new summary entry supersedes the old one (the tail scanner always finds the last summary).

An advisory file lock (`flock` via the `fs2` crate) prevents two processes from writing to the same session simultaneously. The lock is held for the lifetime of the writer and released automatically on process exit or crash.

The original session ID flows through to the `x-claude-code-session-id` API header.

## Reference Implementations

### claude-code (TypeScript)

- JSONL at `~/.claude/projects/{SANITIZED_PATH}/{UUID}.jsonl`.
- 18+ entry types (messages, tags, titles, agents, PR links, context compression).
- Head / tail extraction for listing. 32 concurrent reads, pagination.
- Async write queue batched at 100 ms.
- `parentUuid` chain for conversation forking.

### OpenAI Codex (Rust)

- Date-hierarchical paths: `~/.codex/sessions/YYYY/MM/DD/rollout-{TIMESTAMP}-{UUID}.jsonl`.
- SQLite index for fast metadata queries.
- `RolloutItem` replay model.
- Ephemeral flag to skip persistence for testing.

### learn-claude-code (Python)

- Project-local `.transcripts/transcript_{TIMESTAMP}.jsonl`.
- 3-layer context compression (micro, auto, manual).
- Auto-archive full transcript before summarization.

## Design Choices

| Decision          | Choice                  | Rationale                                                                                                              |
| ----------------- | ----------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Storage format    | JSONL                   | Proven, append-only, no dependencies                                                                                   |
| Location          | XDG_DATA_HOME           | Proper XDG separation from config                                                                                      |
| File naming       | `{UUID}.jsonl`          | Simple, globally unique, no path encoding                                                                              |
| Session resume    | Append to existing file | Same file is self-contained; matches claude-code / Codex                                                               |
| Listing           | Head + tail extraction  | O(n_sessions) but avoids full-file parse                                                                               |
| Concurrent access | Advisory flock (`fs2`)  | Prevents interleaved writes; released on crash. Small TOCTOU between read and lock in resume (acceptable for CLI tool) |
| Write batching    | None (immediate flush)  | CLI workload is low-frequency; premature optimization                                                                  |
| Compression       | Deferred                | Separate phase per roadmap                                                                                             |
