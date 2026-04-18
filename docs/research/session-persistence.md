# Session Persistence

Research notes on session persistence design: storage format, listing strategy, and reference implementations.

## Storage Format

### JSONL (newline-delimited JSON)

Every session is a single `.jsonl` file — one JSON object per line.

- **Append-only** — crash-safe with write-then-flush. No need to rewrite the file on each message.
- **Streamable** — can read / write incrementally without loading the entire file into memory.
- **Universal** — used by claude-code, OpenAI Codex, and learn-claude-code.

### Entry Types

Each line is a discriminated union with a `type` field. The schema is
designed for forward compatibility so new features land additively:

| Type      | Position     | Purpose                                                                                 |
| --------- | ------------ | --------------------------------------------------------------------------------------- |
| `header`  | First line   | Session metadata: ID, CWD, model, created timestamp, format `version`                   |
| `message` | Middle lines | Conversation message with `uuid`, optional `parent_uuid`, timestamp                     |
| `title`   | Re-appended  | Session title + source (`first_prompt` / `ai_generated` / `user_provided`). Latest wins |
| `summary` | Last line    | Exit marker: `message_count` + `updated_at`. Latest wins                                |
| `unknown` | Any          | Catch-all absorbed by `#[serde(other)]` when readers see unrecognized types             |

### Forward Compatibility

Three mechanisms keep the schema open for future features without
migration:

1. **`Entry::Unknown` catch-all** — `#[serde(other)]` on a unit variant
   means any unrecognized `type` discriminator parses silently rather
   than aborting the whole file. New writers can emit additional entry
   types (compaction boundaries, tags, PR links, agent metadata) and old
   readers skip them.
2. **`parent_uuid` chain on messages** — each message carries its own
   UUID and a link to its predecessor. Today resume always appends to
   the tail, but a future fork feature can branch from an arbitrary
   message without rewriting the parent file.
3. **`version` field on the header** — bumped on incompatible changes;
   readers refuse files with a newer version to avoid silent data loss.

### Example file

```jsonl
{"type":"header","session_id":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","cwd":"/home/user/project","model":"claude-opus-4-6","created_at":"2026-04-16T12:00:00Z","version":1}
{"type":"title","title":"hello","source":"first_prompt","updated_at":"2026-04-16T12:00:01Z"}
{"type":"message","uuid":"b1...","message":{"role":"user","content":[{"type":"text","text":"hello"}]},"timestamp":"2026-04-16T12:00:01Z"}
{"type":"message","uuid":"c2...","parent_uuid":"b1...","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-04-16T12:00:02Z"}
{"type":"summary","message_count":2,"updated_at":"2026-04-16T12:00:02Z"}
```

### Why Not Raw Messages

The `message` entries wrap `Message` objects (already `Serialize` / `Deserialize`) with a UUID, parent chain, and timestamp. The header, title, and summary entries carry metadata that would otherwise require parsing every message to reconstruct.

## Storage Location

Sessions live in `$XDG_DATA_HOME/ox/sessions/`, falling back to `~/.local/share/ox/sessions/`. This follows XDG conventions — config in `XDG_CONFIG_HOME`, data in `XDG_DATA_HOME`.

File naming: `{session_id}.jsonl` where session ID is a UUID v4.

On Unix, session files are created with mode `0o600` (user-only read / write) so conversation logs — which may contain verbatim tool output and secrets from bash commands — are not world-readable on multi-user systems.

## Session Listing

`ox --list` reads each session file's first line (header) and last ~4 KB (tail scan). The tail scan finds the **latest** `title` and `summary` entries independently, so a re-titled session (e.g., after AI-title generation) shows its current title without rewriting the file.

- **Header** → session ID, CWD, model, created timestamp, format version.
- **Latest title** → display title + source (first-prompt, AI-generated, user-provided).
- **Latest summary** → message count + updated timestamp.
- Sessions without a title line show `(untitled)` — happens when the session was started but no user prompt was recorded, or when an older format without titles is loaded.
- Sessions without a summary line still list — they were interrupted before clean exit, so `Msgs` displays `-`.

## Session Resume

`ox -c` resumes the most recent session. `ox -c <prefix>` resumes by session ID prefix match.

Resume reopens the **existing** session file in append mode. Messages are loaded into memory, sanitized (see below), and sent to the model as context. New messages are appended to the same file. The `parent_uuid` of the first new message references the UUID of the last loaded message, keeping the conversation chain intact.

An advisory file lock (`flock` via the `fs2` crate) prevents two processes from writing to the same session simultaneously. The lock is held for the lifetime of the writer and released automatically on process exit or crash.

The original session ID flows through to the `x-claude-code-session-id` API header.

### Resume Sanitization

A session can crash between writing an assistant's `tool_use` block and the corresponding user `tool_result`. Resuming such a session naively would send an unresolved `tool_use` to the API, which rejects the request.

`SessionManager::resume` runs these sanitization passes on the loaded conversation:

1. Strip trailing `thinking` / `redacted_thinking` blocks on the last assistant (API rejects trailing thinking).
2. Drop `tool_use` blocks that have no matching `tool_result` anywhere in the log.
3. Drop messages that became empty after (2).
4. If the last remaining message is a user turn containing only `tool_result` blocks (crash between `tool_result` write and the next assistant response), append a synthetic assistant sentinel so role alternation stays valid for the next API call.

This keeps the next API call safe without requiring the user to manually repair the transcript.

## Reference Implementations

### claude-code (TypeScript)

- JSONL at `~/.claude/projects/{SANITIZED_PATH}/{UUID}.jsonl` — project-scoped.
- 18+ entry types (messages, tags, titles, agents, PR links, context compression, file history, attribution, worktree state).
- Head / tail extraction for listing. 32 concurrent reads, pagination.
- Async write queue batched at 100 ms (`FLUSH_INTERVAL_MS`).
- `parentUuid` chain for conversation forking.
- File permissions `0o600`, parent dir `0o700`.

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

| Decision               | Choice                                       | Rationale                                                                                                              |
| ---------------------- | -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Storage format         | JSONL                                        | Proven, append-only, no dependencies                                                                                   |
| Location               | XDG_DATA_HOME                                | Proper XDG separation from config                                                                                      |
| File naming            | `{UUID}.jsonl`                               | Simple, globally unique, no path encoding                                                                              |
| File permissions       | `0o600` on Unix                              | Session logs may contain secrets from bash output; restrict to owner on multi-user systems                             |
| Entry discriminator    | Tagged union with `Unknown` catch-all        | Forward-compat: new variants land additively without breaking old readers                                              |
| Message identity       | `uuid` per message + optional `parent_uuid`  | Foundation for future forking / partial replay; chain is verifiable                                                    |
| Title lifecycle        | Separate `Title` entry, re-appendable        | Supports AI-generated titles and future `/title` command without rewriting the file                                    |
| Summary                | `Summary` on exit (message_count only)       | Latest wins on tail scan; splitting title out kept summary as a pure exit marker                                       |
| Format versioning      | `version` field on header (`default = 1`)    | Explicit version lets future bumps reject old readers cleanly                                                          |
| Session resume         | Append to existing file                      | Same file is self-contained; matches claude-code / Codex                                                               |
| Resume sanitization    | Drop unresolved `tool_use`s, inject sentinel | API rejects unresolved tool calls or consecutive user turns; sanitization keeps resume robust after mid-turn crashes   |
| Listing                | Head + tail extraction                       | O(n_sessions) but avoids full-file parse                                                                               |
| Concurrent access      | Advisory flock (`fs2`)                       | Prevents interleaved writes; released on crash. Small TOCTOU between read and lock in resume (acceptable for CLI tool) |
| Write batching         | None (immediate flush)                       | CLI workload is low-frequency; revisit if profiling shows `fsync` is a bottleneck                                      |
| Project-scoped listing | Deferred                                     | Sessions all in one flat dir today; per-project subdirs tracked in `.claude/plans/session-follow-ups.md`               |
| Compression            | Deferred                                     | Separate phase per roadmap; new entry type added without migration via `Unknown` catch-all                             |
