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

Sessions live in `$XDG_DATA_HOME/ox/sessions/{project}/`, falling back to `~/.local/share/ox/sessions/{project}/`. `{project}` is a stable fingerprint of the working directory at session creation time — path separators and reserved characters become `-`, and very long paths are truncated with an inline FNV-1a 64-bit hash suffix so two long paths sharing a prefix cannot collide.

File naming: `{unix_timestamp}-{session_id}.jsonl`. Session ID is still a UUID v4; the timestamp prefix gives chronological directory-order listings without fighting the `--list` mtime sort. Lookup by session ID is by suffix (`-{id}.jsonl`), so the prefix stays invisible to callers.

Two migrations run once at store open time and are idempotent:

1. **Flat → project-scoped** — any legacy `{uuid}.jsonl` file at the top of the sessions dir is moved into its project subdir based on the header's `cwd`, picking up the new timestamp prefix in the same pass.
2. **Unprefixed → prefixed** — any remaining `{id}.jsonl` inside a project subdir is renamed to `{epoch}-{id}.jsonl` by reading the header.

On Unix, session files are created with mode `0o600` (user-only read / write) so conversation logs — which may contain verbatim tool output and secrets from bash commands — are not world-readable on multi-user systems.

## Session Listing

`ox --list` walks the current project subdirectory by default; `--all` / `-a` widens the scope to every project. For each file we read the header (line 1), optionally an `Entry::Title` on line 2 (see tail-scan note), and scan the last ~4 KB for the latest re-appended `Title` and `Summary`.

- **Header** → session ID, CWD, model, created timestamp, format version.
- **Title** → head scan catches the first-prompt title; tail scan overrides with any later AI-generated / user-provided title. The newer `updated_at` wins.
- **Latest summary** → message count + updated timestamp.
- Sessions without a title line show `(untitled)` — happens when the session was started but no user prompt was recorded.
- Sessions without a summary line still list — they were interrupted before clean exit, so `Msgs` displays `-`.
- Sort order is by file mtime (most recently active first), with session_id as a tiebreak. Resumed sessions therefore bubble back to the top of the list.

## Session Resume

`ox -c` resumes the most recent session in the current project. `ox -c <prefix>` resumes by session ID prefix match. `--all` / `-a` extends either to every project; a specific session ID also resolves cross-project automatically via the `find_session_path` fallback.

Resume reopens the **existing** session file in append mode. Messages are loaded into memory, sanitized (see below), and sent to the model as context. New messages are appended to the same file. The `parent_uuid` of the first new message references the UUID of the last loaded message, keeping the conversation chain intact.

An advisory file lock (`flock` via the `fs2` crate) prevents two processes from writing to the same session simultaneously. The lock is released automatically on process exit, so a stuck lock always indicates a live peer — `ox` retries the acquisition up to 5 times with a 1 s interval before giving up, so accidental back-to-back invocations succeed once the first has finished.

The original session ID flows through to the `x-claude-code-session-id` API header.

### Resume Sanitization

A session can crash between writing an assistant's `tool_use` block and the corresponding user `tool_result`. Resuming such a session naively would send an unresolved `tool_use` to the API, which rejects the request.

`SessionManager::resume` runs these sanitization passes on the loaded conversation:

1. Strip trailing `thinking` / `redacted_thinking` blocks on the last assistant (API rejects trailing thinking).
2. Drop assistant `tool_use` blocks that have no matching `tool_result` anywhere in the log.
3. Drop user `tool_result` blocks whose `tool_use_id` has no surviving assistant `tool_use`. Symmetric to (2); covers the case where the paired `tool_use` line was corrupted during load or dropped in (2), leaving an orphan `tool_result` the API would reject.
4. Drop messages that became empty after (2) or (3).
5. If the last remaining message is a user turn containing only `tool_result` blocks (crash between `tool_result` write and the next assistant response), append a synthetic assistant sentinel so role alternation stays valid for the next API call.

This keeps the next API call safe without requiring the user to manually repair the transcript.

### Write-error reporting

Session I/O runs alongside the agent loop but must not abort it — the user's turn should not fail because the disk is full. `main::log_session_err` logs every failure at `warn!` and, the first time a write fails in a session, surfaces an `AgentEvent::Error` through the active sink so the TUI / REPL can show it inline. Subsequent failures within the same session warn-log only; the `write_failed` flag on `SessionManager` prevents repeated UI errors for the same root cause.

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

| Decision              | Choice                                             | Rationale                                                                                                                                           |
| --------------------- | -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Storage format        | JSONL                                              | Proven, append-only, no dependencies                                                                                                                |
| Location              | XDG_DATA_HOME                                      | Proper XDG separation from config                                                                                                                   |
| Directory layout      | Per-project subdir (`sanitize(cwd)`)               | Listings stay scoped to the project you are working in; `--all` opts into a cross-project view                                                      |
| File naming           | `{epoch}-{UUID}.jsonl`                             | Timestamp prefix keeps `ls` chronological; lookup by UUID suffix stays O(readdir)                                                                   |
| Migration             | Idempotent flat → project → prefix sweep on open   | Users upgrading from the flat layout land on the final layout in one `ox` invocation without manual steps                                           |
| File permissions      | `0o600` on Unix                                    | Session logs may contain secrets from bash output; restrict to owner on multi-user systems                                                          |
| Entry discriminator   | Tagged union with `Unknown` catch-all              | Forward-compat: new variants land additively without breaking old readers                                                                           |
| Message identity      | `uuid` per message + optional `parent_uuid`        | Foundation for future forking / partial replay; chain is verifiable                                                                                 |
| Title lifecycle       | Separate `Title` entry, re-appendable              | Supports AI-generated titles and future `/title` command without rewriting the file                                                                 |
| Summary               | `Summary` on exit (message_count only)             | Latest wins on tail scan; splitting title out kept summary as a pure exit marker                                                                    |
| Format versioning     | `version` field on header (`default = 1`)          | Explicit version lets future bumps reject old readers cleanly                                                                                       |
| Session resume        | Append to existing file                            | Same file is self-contained; matches claude-code / Codex                                                                                            |
| Resume sanitization   | Symmetric filter + sentinel                        | Drop unresolved `tool_use`s AND orphan `tool_result`s, then inject sentinel if needed; covers both halves of a tool turn lost to crashes/corruption |
| Write-error surfacing | First failure only (via `AgentEvent::Error`)       | Balance visibility (user knows persistence broke) against spam (no re-report on every write) after the initial notification                         |
| Listing scan          | Head (line 1 header + line 2 title) + tail (4 KiB) | First-prompt title lives at line 2 and is never re-appended, so a pure tail scan misses it once the file exceeds the window                         |
| Listing sort key      | File mtime, session_id tiebreak                    | Reflects "last used" — resumed sessions bubble up — and is free (no extra I/O beyond the stat already needed)                                       |
| Concurrent access     | Advisory flock with 5×1 s retry                    | Prevents interleaved writes; released on process exit, so retry handles accidental back-to-back invocations. Small TOCTOU between read and lock OK  |
| Write batching        | None (immediate flush)                             | CLI workload is low-frequency; revisit if profiling shows `fsync` is a bottleneck (tracked in `.claude/plans/session-follow-ups.md` #1)             |
| Compression           | Deferred                                           | Separate phase per roadmap; new entry type added without migration via `Unknown` catch-all                                                          |
