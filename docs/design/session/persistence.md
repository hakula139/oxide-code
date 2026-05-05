# Session Persistence

JSONL append-only storage, actor-owned writes, resume semantics.

## Storage Format

Every session is a single `.jsonl` file — one JSON object per line, append-only (crash-safe with write-then-flush).

### Entry Types

| Type      | Position     | Purpose                                                             |
| --------- | ------------ | ------------------------------------------------------------------- |
| `header`  | First line   | Session metadata: ID, CWD, model, created timestamp, format version |
| `message` | Middle lines | Conversation message with `uuid`, optional `parent_uuid`, timestamp |
| `title`   | Re-appended  | Session title + source. Latest wins                                 |
| `summary` | Last line    | Exit marker: `message_count` + `updated_at`. Latest wins            |
| `unknown` | Any          | Catch-all via `#[serde(other)]` for forward-compat                  |

### Forward Compatibility

1. **`Entry::Unknown` catch-all** — unrecognized `type` discriminators parse silently. New entry types land additively.
2. **`parent_uuid` chain** — each message links to its predecessor. Future fork feature branches from an arbitrary message without rewriting.
3. **`version` field** — bumped on incompatible changes; readers refuse newer versions.

## Storage Layout

Sessions live in `$XDG_DATA_HOME/ox/sessions/{project}/`. `{project}` is a filesystem-safe subdirectory name derived from the working directory (path separators become `-`, long paths truncated with xxh64 hash suffix).

File naming: `{unix_timestamp}-{session_id}.jsonl`. Timestamp prefix gives chronological listings; lookup by session ID is by suffix.

On Unix, session files are created with mode `0o600`.

## Session Lifecycle

### Actor + batched writes

The on-disk file is owned by a single `tokio::spawn`-ed actor task; the rest of the program holds a `SessionHandle` that forwards operations as `SessionCmd` over a bounded mpsc channel. Each cmd carries a oneshot ack. The actor's loop: `recv().await` for the first cmd, then `try_recv()` until empty, then one buffered flush over the batch.

`agent_turn` queues a tool round's three writes through one `tokio::join!` so the actor's drain coalesces them into a single buffered flush. The text-only branch records its single assistant message via the sequential path. Side effect of joining: the on-disk file is iteration-atomic — a crash mid-tool leaves the file at the previous turn's tail.

### Writer recovery on flush error

`WriterStatus` is three variants: `Pending { header }` (file not yet on disk), `Active(SessionWriter)` (steady state), `Broken` (last batch errored — next batch reopens via `SessionStore::open_append`).

### Lazy materialization

Starting a session stages the header in memory; the on-disk file is created by the first batch flush carrying real content. A session that exits before any message leaves no artifact.

### Resume and sanitization

`ox -c` reopens the existing session file. The loader walks `Entry::Message` lines into a UUID-indexed DAG, picks the newest-timestamped leaf as the tip, and walks back via `parent_uuid` to produce a linear chain. The sanitization pipeline: strip trailing thinking, drop unresolved tool_use / orphan tool_result, merge adjacent same-role survivors, inject head / tail sentinels.

### Fork-on-conflict concurrency

No file-level lock. Two processes resuming the same session both append; the newest-leaf rule resolves forks deterministically. Writes smaller than `PIPE_BUF` are atomic under POSIX `O_APPEND`.

### AI-generated titles

First user prompt of a fresh session seeds a detached tokio task calling Haiku via `Client::complete` with a 3-7 word sentence-case prompt, structured-outputs JSON schema. Failures warn-log only.

### Write-error surfacing

First failure populates the handle's `RecordOutcome::failure` slot so the agent loop emits a one-shot `AgentEvent::Error`. Two independent sticky-once flags (`flush_failure_surfaced`, `actor_gone_surfaced`) prevent different failure kinds from masking each other.

## Listing

`ox --list` walks the current project subdirectory by default; `--all` widens to every project. For each file: read header (line 1), stream the rest tracking latest `Entry::Title` and `Entry::Summary`. A cheap prefix pre-filter on the `"type"` discriminator keeps message lines off the full-parse hot path.

- Sessions without a title show `(untitled)`.
- Sessions without a summary still list (`Msgs` displays `-`).
- Sort by file mtime (session_id tiebreak).
- Under `--all`, a `Project` column shows the tildified stored `cwd`.
- Titles truncated to terminal width with trailing `...`.

## Design Decisions

1. **JSONL, append-only, one file per session** — crash-safe, streamable, universal.
2. **Per-project subdirectory** — listings stay scoped; `--all` opts into cross-project view.
3. **`{epoch}-{uuid}.jsonl` naming** — timestamp prefix for `ls` order; UUID suffix for lookup.
4. **Forward-compat entry schema** — tagged union + `#[serde(other)]` + `parent_uuid` DAG + `version` field.
5. **Lazy file creation** — empty sessions leave no artifact.
6. **Resume + symmetric sanitization** — reopen, walk DAG, sanitize, keep every transcript API-valid.
7. **Fork-on-conflict concurrency, no file lock** — newest-leaf rule resolves forks; large interleaved writes are warn-skipped.
8. **Fire-and-forget AI titles** — one-shot Haiku call, warn-log on failure, tail-scan's `updated_at` auto-supersedes.
9. **First-failure-only write-error surfacing** — visibility without spam via single `AgentEvent::Error` + warn-logs.
10. **Head + tail listing scan, mtime sort** — head catches first-prompt title; tail picks up later titles and summary.
11. **Actor-owned writes, receive-and-drain batching** — one task owns the writer; `tokio::join!` coalesces a tool round's writes into one flush.
