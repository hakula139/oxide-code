# Session Persistence

JSONL append-only storage, actor-owned writes, resume semantics.

## Storage Format

Every session is a single append-only `.jsonl` file with one JSON object per line.

### Entry Types

| Type                   | Position     | Purpose                                                             |
| ---------------------- | ------------ | ------------------------------------------------------------------- |
| `header`               | First line   | Session metadata: ID, CWD, model, created timestamp, format version |
| `message`              | Middle lines | Conversation message with `uuid`, optional `parent_uuid`, timestamp |
| `title`                | Re-appended  | Session title + source. Latest wins                                 |
| `tool_result_metadata` | Sidecar      | Display metadata for replaying tool results                         |
| `file_snapshot`        | Sidecar      | File-tracker state restored on resume when disk state still matches |
| `context`              | Sidecar      | Prompt context a resume replays. Latest since the last compact wins |
| `compact`              | Boundary     | Summary boundary plus pre-compact count and optional instructions   |
| `summary`              | Last line    | Exit marker: `message_count` + `updated_at`. Latest wins            |

### Schema Invariants

1. **Explicit entry schema.** The loader treats unknown discriminators as malformed lines, so schema compatibility stays explicit.
2. **`parent_uuid` chain.** Each message links to its predecessor. Concurrent resumes can fork, and the newest-leaf rule resolves the active branch without rewriting history.
3. **`version` field.** Bump on incompatible changes so older binaries fail fast on session state they cannot read.

## Storage Layout

Sessions live in `$XDG_DATA_HOME/ox/sessions/{project}/`. `{project}` is a filesystem-safe subdirectory name derived from the working directory (path separators become `-`, long paths truncated with xxh64 hash suffix).

File naming: `{unix_timestamp}-{session_id}.jsonl`. Timestamp prefix gives chronological listings, while lookup by session ID uses the suffix.

On Unix, session files are created with mode `0o600`.

## Session Lifecycle

### Actor + batched writes

The on-disk file is owned by a single `tokio::spawn`-ed actor task. The rest of the program holds a `SessionHandle` that forwards operations as `SessionCmd` over a bounded mpsc channel, with each cmd carrying a oneshot ack. The actor's loop awaits the first cmd, drains any queued commands with `try_recv()`, then appends the batch in one write.

`agent_turn` queues a tool round's three writes through one `tokio::join!`, while the text-only branch records its single assistant message via the sequential path. The joined writes usually land in one append, and resume sanitization drops a tool call whose result was torn off.

### Writer recovery on write error

`WriterStatus` is either `Pending` (file not yet on disk, staged header plus an optional deferred rename) or `Active(SessionWriter)`. Entries stay queued in `SessionState` until a write succeeds, and each batch retries the queue ahead of its own entries, so a message never reaches disk before its parent. The flush that creates the file writes the header and its batch in one write and unlinks the file if that write fails, so the session stays `Pending` with nothing on disk. A failed append drops the writer's file handle, and the next append reopens the same path, including for sessions resumed from an explicit path. A failed flush with more than 8 MiB to write drops the queue and stops persistence for the rest of the session.

Every append first terminates a torn last line, so a crash mid-write loses only the batch it interrupted, none of which was acknowledged. A failed append truncates the bytes it wrote unless another process has appended since, so a retry never duplicates a line. A compact whose write fails is unqueued and never committed, matching the agent, which keeps its pre-compact transcript. The loader applies a compact boundary only once its post-compact root follows, so a boundary whose root never landed leaves the old chain in place.

### Lazy materialization

Starting a session stages the header in memory. The on-disk file is created by the first batch flush carrying real content. A session that exits before any message leaves no artifact.

### Resume and sanitization

`ox -c` reopens the existing session file. The loader walks `Entry::Message` lines into a UUID-indexed DAG, picks the newest-timestamped leaf as the tip, and walks back via `parent_uuid` to produce a linear chain. The sanitization pipeline then strips trailing thinking, drops unresolved tool_use / orphan tool_result, Merges adjacent same-role survivors and injects head and tail sentinels.

### Prompt context

A session records its prompt context (the environment section, rendered instructions with the date, and what the model has seen of both) in a `context` entry placed ahead of the user message that first carries it. Resuming replays this entry verbatim, sending the same cached system prompt and first message. If the session resumes in another directory, the environment section is redetected, and any subsequent instruction or date changes are attached to the next user message as a `<system-reminder>` block, which transcript displays omit. A transcript lacking a `context` entry, or one where no user message followed the last compaction, rebuilds the context and records it with the next message.

### Fork-on-conflict concurrency

There is no file-level lock: two processes resuming the same session both append, and the newest-leaf rule resolves forks deterministically. Each batch lands in one `O_APPEND` write, so concurrent appends interleave only between batches.

### AI-generated titles

In the TUI, the first user prompt of a fresh session spawns a detached tokio task that asks Haiku through `Client::complete` for a 3-7 word sentence-case title, constrained by a structured-outputs JSON schema. Failures only log a warning.

### Write-error surfacing

First failure populates the handle's `RecordOutcome::failure` slot so the agent loop emits a one-shot `AgentEvent::Error`. Compact acks carry every failure, since the agent swaps in the summary only when the boundary reached disk. Three independent sticky-once flags (`flush_failure_surfaced`, `persistence_stop_surfaced`, `actor_gone_surfaced`) keep different failure kinds from masking each other, so stopping persistence surfaces even after an earlier flush failure.

## Listing

`ox --list` walks the current project subdirectory by default. `--all` widens to every project. For each file it reads the header, then streams the rest while tracking latest `Entry::Title` and `Entry::Summary`. A cheap prefix pre-filter on the `"type"` discriminator keeps message lines off the full-parse hot path.

- Sessions without a title show `(untitled)`.
- Sessions without a summary still list (`Msgs` displays `-`).
- Sort by file mtime (session_id tiebreak).
- Under `--all`, a `Project` column shows the tildified stored `cwd`.
- Titles truncated to terminal width with trailing `...`.

## Design Decisions

1. **JSONL, append-only, one file per session.** Crash-safe, streamable, universal.
2. **Per-project subdirectory.** Listings stay scoped, and `--all` opts into cross-project view.
3. **`{epoch}-{uuid}.jsonl` naming.** Timestamp prefix for `ls` order, UUID suffix for lookup.
4. **Explicit entry schema.** Tagged union + `parent_uuid` DAG + `version` field.
5. **Lazy file creation.** Empty sessions leave no artifact.
6. **Resume + symmetric sanitization.** Reopen, walk DAG, sanitize, keep every transcript API-valid.
7. **Fork-on-conflict concurrency, no file lock.** Newest-leaf rule resolves forks, and one write per batch keeps concurrent appends from splitting a line.
8. **Fire-and-forget AI titles.** One-shot Haiku call, warn-log on failure, tail-scan's `updated_at` auto-supersedes.
9. **First-failure-only write-error surfacing.** One `AgentEvent::Error` per failure kind plus warn-logs gives visibility without spam.
10. **Head + tail listing scan, mtime sort.** Head catches first-prompt title, and tail picks up later titles and summary.
11. **Actor-owned writes, receive-and-drain batching.** One task owns the writer, and `tokio::join!` coalesces a tool round's records into one append.
