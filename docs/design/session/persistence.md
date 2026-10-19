# Session Persistence

JSONL append-only storage, actor-owned writes, resume semantics.

## Storage Format

Every session is a single append-only `.jsonl` file with one JSON object per line.

### Entry Types

| Type                   | Position       | Purpose                                                                           |
| ---------------------- | -------------- | --------------------------------------------------------------------------------- |
| `header`               | First line     | Session metadata: ID, working directory, model, created timestamp, format version |
| `message`              | Middle lines   | Conversation message with `uuid`, optional `parent_uuid`, timestamp               |
| `title`                | Re-appended    | Session title + source. Latest wins                                               |
| `tool_result_metadata` | Sidecar        | Display metadata for replaying tool results                                       |
| `file_snapshot`        | Sidecar        | File-tracker state restored on resume when disk state still matches               |
| `usage`                | After request  | One billed request's token usage, model, and prompt-cache TTL                     |
| `context`              | Before message | Prompt context of that message. Latest on the resolved chain wins                 |
| `compact`              | Boundary       | Summary, pre-compact count, and optional instructions and tasks                   |
| `archive`              | Re-appended    | Archive state: `archived` + `at`. Latest wins                                     |
| `summary`              | Last line      | Exit marker: `message_count` + `updated_at`. Latest wins                          |

### Schema Invariants

1. **Explicit entry schema.** The loader treats unknown discriminators as malformed lines, so schema compatibility stays explicit.
2. **`parent_uuid` chain.** Each message links to its predecessor. Concurrent resumes can fork, and the newest-leaf rule resolves the active branch without rewriting history.
3. **`version` field.** Bump on incompatible changes so older binaries fail fast on session state they cannot read.

## Compatibility

Session files follow the [stability surface](../../../RELEASING.md#stability-surface): within a minor version, each stable release can resume session files written by earlier stable releases of that minor version. Before 0.1.0, a release may stop reading files from earlier versions, and an earlier prerelease binary may skip lines in a newer file without a header `version` bump. The loader treats a line in a shape it no longer reads as malformed and skips it. Since a skipped message breaks the chain that resume follows back from its newest leaf, the session resumes with only the messages after the last skipped one.

Within the stability surface, a newer binary can read an older file as long as every format change is one of the following:

- **Additions.** A new entry type, or a new field that deserializes to a default when absent.
- **Removals.** A removed field, since the loader ignores fields it does not know. The data the field held is lost on resume.
- **Changed shapes with a loader.** Renaming a field or changing its type or meaning requires a loader that still reads the old shape.

An older binary reading a newer file skips entry types it does not know as malformed lines and ignores unknown fields. Bump the header's `version` only when an older binary would misread a newer file, since an older binary refuses any file whose `version` is newer than it supports.

A reasoning block stores the `wire` and `model` that produced it alongside a `payload` that only that wire can read, such as the `signature` of an Anthropic `thinking` block or the `data` of a `redacted_thinking` block. The Anthropic wire sends every Anthropic block back unchanged whichever model produced it, as Anthropic's guide to [switching models](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models) asks, so the recorded `model` changes no request. Which blocks a model reads depends on the platform, and the API drops or ignores the rest, as the [research note](../../research/api/extended-thinking.md#constraints) details.

A server tool's result, such as a `web_search_tool_result`, is stored as a `server_tool_result` block that keeps the API's block type in `kind` and every other field unchanged, so the Anthropic wire sends it back as the API returned it.

A `wire` value or content block `type` that an older binary does not know fails to parse, and that binary skips the whole message as a malformed line. From 0.1.0 on, adding either needs a header `version` bump. A task `status` works the same way inside a `compact` entry: an unknown one makes an older binary skip the boundary, so adding a status needs a bump too.

Tests in `session/handle.rs` enforce this guarantee for stable releases, using one fixture per release starting with `v0.1.0`. Each fixture lives in `crates/oxide-code/src/session/fixtures/` and is named after its tag:

- **Fixture source.** One test writes a session containing every entry type and content block, then adds messages, prompt contexts, and sidecars after compaction so a resume reads them. It snapshots the file after masking its per-run UUIDs, timestamps, and working directory. For each stable tag, the release PR copies that snapshot as the fixture for the new release, as described in [Prepare a Release](../../../RELEASING.md#prepare-a-release).
- **Resume check.** Another test fills each fixture's masks with fixed values, then lists and resumes it. It snapshots what the listing and the resume recovered: the title, branch, archive state, compaction, prompt context, transcript, sidecars, and recorded usage. It then appends a message and checks that the message continues the resumed chain. The same check runs on the session this build writes, so a pull request that changes the format also shows how its files will resume.
- **Changed behavior.** If a change alters how an old file resumes, that fixture's snapshot will fail. Fixtures capture frozen release output, so never edit them. Accept a changed fixture snapshot only when the change is allowed by this section.

## Storage Layout

Sessions live in `$XDG_DATA_HOME/ox/sessions/{project}/`. `{project}` is a filesystem-safe subdirectory name derived from the working directory (path separators become `-`, long paths truncated with xxh64 hash suffix).

File naming: `{unix_timestamp}-{session_id}.jsonl`. Timestamp prefix gives chronological listings, while lookup by session ID uses the suffix.

On Unix, session files are created with mode `0600`.

## Session Lifecycle

### Actor and Batched Writes

The on-disk file is owned by a single `tokio::spawn`-ed actor task. The rest of the program holds a `SessionHandle` that forwards operations as `SessionCmd` over a bounded mpsc channel, with each cmd carrying a oneshot ack. The actor's loop awaits the first cmd, drains any queued commands with `try_recv()`, then appends the batch in one write.

`agent_turn` queues a tool round's three writes through one `tokio::join!`, while the text-only branch records its single assistant message via the sequential path. The joined writes usually land in one append, and resume sanitization drops a tool call whose result was torn off.

### Writer Recovery on Write Error

- `WriterStatus` is either `Pending` (file not yet on disk, staged header plus an optional deferred title) or `Active(SessionWriter)`.
- Entries stay queued in `SessionState` until a write succeeds, and each batch retries the queue ahead of its own entries, so a message never reaches disk before its parent.
- The flush that creates the file writes the header and its batch in one write and unlinks the file if that write fails, so the session stays `Pending` with nothing on disk.
- A failed append drops the writer's file handle, and the next append reopens the same path, including for sessions resumed from an explicit path.
- A failed flush with more than 8 MiB to write drops the queue and stops persistence for the rest of the session.

Every append first terminates a torn last line, so a crash mid-write loses only the batch it interrupted, none of which was acknowledged. A failed append truncates the bytes it wrote unless another process has appended since. When it cannot truncate, the retry repeats the lines that landed, and the loader keeps one copy of each message and `usage` entry by its `uuid`. A compact whose write fails is unqueued and never committed, matching the agent, which keeps its pre-compact transcript. The loader applies a compact boundary only once its post-compact root follows, so a boundary whose root never landed leaves the old chain in place.

### Lazy Materialization

Starting a session stages the header in memory. The on-disk file is created by the first batch flush carrying real content. A session that exits before any message leaves no artifact.

### Resume and Sanitization

`ox -c` reopens the existing session file. The loader walks `Entry::Message` lines into a UUID-indexed DAG, picks the newest-timestamped leaf as the tip, and walks back via `parent_uuid` to produce a linear chain. The sanitization pipeline then drops tool calls without results and results without calls, merges adjacent same-role survivors, and injects head / tail sentinels. A client call pairs with a user `tool_result`, and a server call with the result block in the same or a later reply. A server call that a `pause_turn` left waiting is dropped, because the next prompt closes the turn and the API rejects a call without its result.

### Prompt Context

A session records its prompt context: the environment section, the first message's rendered instructions, date, model identity, and what the model has seen of them.

- **Record with the carrying message.** A `context` entry is written in the same batch as the first user message that carries it, immediately before that message and with the same timestamp. The loader binds the entry to the next message with a matching timestamp, so a context entry stranded by a torn write cannot attach to another writer's message.
- **Replay the resolved branch.** On resume, the latest context entry on the resolved chain is replayed verbatim, so the request sends the same cached system prompt and first message. A forked session replays the context recorded on its own branch.
- **Refresh changed context.** If the session resumes in another directory, the environment section is redetected. Any later instruction, date, or model change, including a `/model` swap, is attached to the next user message as a `<system-reminder>` block, which transcript displays omit, so the system prompt stays byte-identical across a swap.
- **Rebuild missing context.** A transcript lacking a `context` entry, or one where no user message followed the last compaction, rebuilds the context and records it with the next message.

### Task List

The session's [task list](../tools/tasks.md) needs no entry of its own. On resume, the loader takes it from the last successful `tasks` call on the resolved chain, or else from the `tasks` field of the last `compact` entry, which records the list the summarized transcript left. The field is omitted when the list was empty.

### Archiving

An `archive` entry sets whether default listings and bare `ox -c` skip the session, and the latest one wins. Bare `/archive` queues one through the actor before the roll that finalizes the session. `/archive <prefix>`, `/unarchive <prefix>`, and Ctrl+X in the `/resume` picker append an entry to another session's file and restore its mtime, so toggling its archive status does not change its position in listings. Resuming an archived session appends an entry that unarchives it. Files never move, so path-based resume and every other reference to a session file keep working.

### Fork-on-Conflict Concurrency

There is no file-level lock: two processes resuming the same session both append, and the newest-leaf rule resolves forks deterministically. Each batch lands in one append write (`O_APPEND` on Unix, an end-of-file-offset write on Windows), so concurrent appends interleave only between batches.

### AI-Generated Titles

In the TUI, the first user prompt of a fresh session spawns a tokio task that asks Haiku through `Client::complete` for a 3-7 word sentence-case title, constrained by a structured-outputs JSON schema. The session owns the task, and finalizing the session on a roll or at exit aborts it first, so a late title never writes to or reports on a session that has ended. Generation failures only log a warning, while a failed title write surfaces like any other session write error.

### Session Cost

Each request behind a turn, a `/compact` summary, or an auto-compaction summary appends a `usage` entry, while the background title request goes unrecorded. The entry holds a `uuid`, the request's `model` ID and `cache_ttl` (`"5m"` or `"1h"`), the four provider counts `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, and `output_tokens`, and a `timestamp`. The model and TTL let the engine price the request at the rates it ran with, even after a `/model` swap or a TTL change.

On resume, the loader collects every `usage` entry in the file, including those on abandoned branches and before compact boundaries, since the session paid for each. The engine sums their estimated cost into the session total, which `/clear` resets with the fresh session. A file without `usage` entries resumes with a cost of zero. Older binaries skip `usage` lines as unknown entries and still resume the transcript.

### Write-Error Surfacing

Write failures surface through the session handle:

- First failure populates the handle's `RecordOutcome::failure` slot so the agent loop emits a one-shot `Event::Error`.
- Compact and archive acknowledgments include every failure.
- The agent swaps in the summary only after the boundary reaches disk, and bare `/archive` rolls to a fresh session only after the archive entry is written.
- Failed compact and archive operations are also removed from the retry queue, so a later flush cannot persist them.
- Three independent sticky-once flags (`flush_failure_surfaced`, `persistence_stop_surfaced`, `actor_gone_surfaced`) keep different failure kinds from masking each other, so stopping persistence surfaces even after an earlier flush failure.

## Listing

Session listing separates file discovery from metadata parsing:

- **Scope.** `ox --list` walks the current project subdirectory by default, and `--all` widens to every project.
- **Discovery.** The walk only stats files, so bare `ox -c` parses files only until it finds the newest readable unarchived one, and a prefix lookup parses only files whose name carries a matching ID.
- **Metadata.** A parsed file yields its header, the latest `Entry::Title` and `Entry::Summary`, and the archive state. A prefix pre-filter on the `"type"` discriminator keeps message lines off the full-parse hot path.
- **Archive view.** `--list` and the `/resume` picker show unarchived sessions, or only archived ones with `--list --archived` and the picker's Ctrl+A.
- **Page budget.** Archive state is stored in each file, so a listing parses sessions in order until it finds one beyond the page, then reports only whether more sessions follow. When few sessions are archived, the archived view parses further.

Listings sort rows and render them as follows:

- Sessions without a title are shown as `(untitled)`.
- Sessions without a summary still list (`Msgs` displays `-`).
- Sort by file mtime, with the file name breaking ties.
- Under `--all`, a `Project` column shows the tildified stored `cwd`.
- Titles truncated to terminal width with trailing `...`.

## Design Decisions

1. **JSONL, append-only, one file per session.** A process crash loses at most the batch in progress, and readers can stream the file line by line.
2. **Per-project subdirectory.** Listings stay scoped, and `--all` opts into cross-project view.
3. **`{epoch}-{uuid}.jsonl` naming.** Timestamp prefix for `ls` order, UUID suffix for lookup.
4. **Explicit entry schema.** A tagged union of entries, a `parent_uuid` DAG, and a header `version`.
5. **Lazy file creation.** Empty sessions leave no artifact.
6. **Resume with symmetric sanitization.** Resume walks the DAG and drops orphans on both sides of each tool call, so every transcript stays API-valid.
7. **Fork-on-conflict concurrency, no file lock.** Newest-leaf rule resolves forks, and one write per batch keeps concurrent appends from splitting a line.
8. **Best-effort AI titles scoped to their session.** A one-shot Haiku call logs a warning on failure, and finalizing the session aborts it. The title with the newest `updated_at` wins.
9. **First-failure-only write-error surfacing.** One `Event::Error` per failure kind plus warn-logs gives visibility without spam.
10. **Listing scan with message pre-filter, mtime sort.** Titles and summaries can appear anywhere in the file, so the scan reads every line and the prefix check keeps message lines out of JSON parsing.
11. **Actor-owned writes, receive-and-drain batching.** One task owns the writer, and `tokio::join!` coalesces a tool round's records into one append.
12. **Archive state as an appended entry.** A session keeps the same path whether it is archived or active. Because filtering listings by archive state requires parsing files, listings indicate whether more sessions follow the current page without counting them.
