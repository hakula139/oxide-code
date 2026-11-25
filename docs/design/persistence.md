# Session Persistence

`ox` stores every session in one SQLite database, commits each write under the session state's lock, and rebuilds the transcript on resume from the chain its head names. JSONL is the interchange format for `ox export` and `ox -r <file>`.

## Storage Layout

```text
$XDG_DATA_HOME/ox/
├── sessions.db                   # The store, with sessions.db-wal and sessions.db-shm beside it
├── artifacts/{session-id}/       # Spill files under tool-results/, created by the first artifact
├── locks/{session-id}.lock       # Held shared while a process has the session open
└── locks/sessions.db.lock        # Held while a process sets up the store
```

The data directory has mode `0700`. On Unix, the store creates `sessions.db` with mode `0600` before SQLite opens it so `-wal` and `-shm` inherit that mode. WAL [does not work over a network filesystem](https://www.sqlite.org/wal.html), and SQLite cannot detect one reliably, so the [guide](../guide/sessions.md#where-sessions-live) documents the local-filesystem requirement without runtime probing. `store::artifact_dir` locates [spilled tool output](truncation.md#spill-files), and `SessionStore::delete` removes the directory with the rows.

## Connection

- **Bundled SQLite.** `rusqlite` compiles SQLite from source so every platform gets the WAL-reset corruption fix for concurrent multi-process writes and checkpoints ([how to corrupt](https://www.sqlite.org/howtocorrupt.html), fixed in [3.51.3](https://www.sqlite.org/releaselog/3_51_3.html)), which macOS and older system SQLite builds lack.
- **Open.** Each process opens the store once at startup and reuses that handle in `/resume`. `SessionStore::open` takes an exclusive lock on `locks/sessions.db.lock` while `schema::open` switches a fresh file to WAL and creates the schema (leaving rollback mode upgrades a read lock to a write lock and fails immediately without invoking the busy handler). An existing store at the current revision opens without the write lock, and migrations run under `BEGIN IMMEDIATE`. Unreadable files fail to open with the recovery steps from the [sessions guide](../guide/sessions.md#where-sessions-live). Because open inspects only the header and schema, deeper corruption surfaces on the first write or listing that hits it.
- **Per connection.** Sets `synchronous = NORMAL`, `checkpoint_fullfsync = ON`, a 5 s busy timeout, and `foreign_keys = ON`. In WAL mode, `NORMAL` survives process crashes and keeps the database intact across power loss (at most rolling back recent commits) as long as checkpoints reach disk in order. `checkpoint_fullfsync` enables `F_FULLFSYNC` on macOS checkpoints and is a no-op elsewhere.
- **Transactions.** Writes run under `BEGIN IMMEDIATE` so the head check and dependent writes share one write lock. Reads use deferred transactions and never write.

## Schema

Columns store fields SQL filters, sorts, joins, or compares, while JSON text columns store serde payloads evolving under [Compatibility](#compatibility). Timestamps are Unix milliseconds, and every table is `STRICT`.

| Table      | Row                    | Notable columns                                                                                                   |
| ---------- | ---------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `sessions` | One per session        | `forked_from`, project root, content `format`, title and its manual flag, archive time, `head_id`, file snapshots |
| `messages` | One per message        | Commit-order `seq`, `parent_id`, content JSON, and the `context`, `compact` boundary, and `tool_metadata` JSON    |
| `usage`    | One per billed request | Request ID and the usage record JSON                                                                              |

1. **Immutable rows and a head pointer.** Written rows never change and leave only when their session is deleted. `head_id` marks the active branch tip, so rewind, compaction, and fork move the head while preserving abandoned rows for export. Resume walks back from `head_id` to the nearest NULL parent (session start or last compaction root).
2. **Parents first.** `messages` keys on `(session_id, id)` with a composite foreign key on `(session_id, parent_id)`, enforcing that a parent precedes its child within the same session.
3. **Last activity.** `updated_at` changes only when the head moves, keeping listing order stable across renames and archives.

Prompt-history recall walks user-role rows newest-first in `(created_at, id, seq)` order via bounded batches on the revision 2 partial index `messages_by_prompt_time`. It filters by format and project before decoding JSON, collects only ordinary text blocks, and advances past every examined row (so a batch may be empty before exhaustion). Logical message IDs deduplicate fork and import copies while keeping separate identical submissions distinct. The walk includes archived and detached rows, and deleting a session removes only its own copies.

## Compatibility

The store enforces the [stability surface](../../RELEASING.md#stability-surface) with two version numbers:

- **Revision** (`PRAGMA user_version`). Additive only (new tables, indexes, and nullable or defaulted columns). Newer binaries migrate under `BEGIN IMMEDIATE`, and older binaries continue working unchanged. An incompatible schema change would use a new file such as `sessions-2.db`.
- **Content format** (`sessions.format`). Every message commit records `CONTENT_VERSION`, bumped whenever an older binary would misread a payload (such as a new closed serde enum variant or a dropped required field). Listings and `ox -c` hide newer-format sessions, and loading or importing one fails with an upgrade prompt.

Within one format version, newer binaries read older payloads by defaulting added fields, ignoring removed fields, or accepting old shapes.

Reasoning blocks store `wire`, `model`, and wire-specific `payload` (such as an Anthropic `thinking` signature). The Anthropic wire replays every Anthropic block unchanged regardless of `model`, following Anthropic's [switching models](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models) guidance, letting the API drop unreadable blocks ([extended-thinking research](../research/api/extended-thinking.md#constraints)). Server tool results (such as `web_search_tool_result`) persist as `server_tool_result` with the API block type in `kind` and all other fields intact for verbatim replay. Generated text blocks carry an optional `origin` (omitted on the wire) so displays hide reminders, render interruptions, and show compact summaries once, while unattributed text stays visible even if it contains reminder tags or interruption markers.

Release fixtures in `crates/ox-core/src/session/fixtures/` lock compatibility since `v0.1.0` via tests in `session/handle.rs`:

- **Fixture source.** One test writes a session covering every row kind and content block, including post-compaction messages, prompt contexts, and metadata. `session_format::writes_every_row_kind` and `session_format::exports_every_row_kind` snapshot masked deterministic SQL dumps and JSONL exports, which each stable release copies to `{tag}.sql` and `{tag}.jsonl` ([Release Preparation](../../RELEASING.md#release-preparation)).
- **Resume check.** `session_format::resumes_from_every_release_fixture` unmasks each fixture into a fresh store, lists and resumes it, snapshots all recovered state (title, branch, archive state, compaction, prompt context, transcript, tool metadata, usage, tasks, file snapshots), and appends a message continuing the chain. Running on the current dump also verifies new format changes.
- **Changed behavior.** Never edit frozen release fixtures. Accept a changed resume snapshot only when allowed by this section.

## Session Lifecycle

### Locked Writes

Each open session shares one mutex-guarded `SessionState` across all `SessionHandle` clones. Writes run on `spawn_blocking`, locking the state, applying changes, and committing queued records in one transaction (opening the state's connection on first commit) so lock waits stay off the async runtime. Only the engine and title task write, each awaiting its own writes in order, with the in-state manual-title check guarding interleavings. Dropping a caller leaves its write to finish on the blocking thread. A panic under the lock stops persistence like a failed commit and propagates to the caller (ending `ox` for engine writes, while recovering the lock lets conversation continue after a title-task panic).

A fresh session's row (including any early `/rename`) inserts lazily in the transaction of its first message, so empty sessions leave no row. `agent_turn` persists each tool round via one `SessionHandle::record_round` commit storing the assistant message followed by its tool-result message and metadata, keeping calls and results atomic.

### Write Failures

A failed commit rolls back its transaction and disables persistence for the rest of the process's session while conversation continues in memory. Nothing retries because later rows cannot chain onto missing parents and post-busy-timeout errors (such as a full disk) are persistent. Later commits fail with a stopped-persistence error naming the first failure, resume continues from the last committed row, and every failure logs on its blocking thread even if the caller dropped.

Each write returns a `WriteReport` whose `failure` the engine emits as `Event::Error`:

- Reported once per session on the first failed write.
- Archive reports every failure, since bare `/archive` rolls to a new session only after committing.
- Compaction always takes effect in memory, and its write error emits after `SessionCompacted` (which clears chat) so the notice stays visible.

### Resume and Sanitization

`ox -c`, `ox -r`, and `/resume` take the session's liveness lock, read its row, and walk `head_id` via recursive CTE back to a NULL parent (cost proportional to the chain since last compaction). The loader replays the newest `context`, reads the root's `compact` boundary, merges `tool_metadata`, sums all `usage` rows, and restores the [task list](tasks.md) from the last successful `tasks` call on the chain (falling back to the compaction root's `tasks` field).

Sanitization drops unmatched tool calls and orphan results, injects head / tail sentinels, and preserves message boundaries including adjacent same-role messages accepted by the [Messages API](https://platform.claude.com/docs/en/api/messages/create). Client calls pair with user `tool_result` blocks, server calls pair with result blocks in the same or a later reply, and server calls left waiting by `pause_turn` are dropped because the next prompt closes the turn.

Resume marks the first-prompt title opportunity spent once the chain has a user prompt or starts at a compaction root, and a manual title skips it ([AI-Generated Titles](#ai-generated-titles)).

### Refused Turns

Because the API keeps refusing while a refused turn remains in history, a `refusal` stop rolls back the entire turn (prompt, drained queued prompts, and tool rounds):

- Truncates the live transcript to its pre-prompt length and restores seen user context and file tracker state.
- Rewinds `head_id` to the pre-prompt parent so resume excludes the turn and the next prompt re-records prompt context if the refused prompt first carried it.
- Aborts the first-prompt title task if rewinding before the first prompt, leaving auto-titling active unless manually named.
- Emits `TurnRolledBack` before `TurnFailed`, returning the prompt to the empty input and drained prompts to the queue head (held for the user).

A turn that compacted partway rolls back only to the compaction root, returning only prompts drained after compaction. Request costs and external tool effects (file edits, started background shells) remain.

### Prompt Context

Sessions record the environment section, first rendered user context, and seen state ([System Prompt](prompt.md)) on the first carrying user message (and refreshed context on each compaction root). Resume replays the newest context on the chain verbatim to preserve the cached system prompt and prefix.

### Archiving

`archived_at` hides sessions from default listings and `ox -c` without changing `updated_at`. A conditional update transitions only from the opposite state, preserving the first archive timestamp and letting concurrent archivers see which call changed the row.

Bare `/archive` archives the live session before rolling. `/archive <prefix>`, `/unarchive <prefix>`, and Ctrl+X in `/resume` resolve another session in the engine and run the update on a blocking worker so UI input stays responsive while another process holds the write lock. Later session ops wait on completion, results reach chat even after the picker closes, and resuming an archived session unarchives it.

### Liveness Locks

Opening a session for writing acquires a shared lock on `locks/{session-id}.lock` until the last handle drops on roll or exit (automatically released by the kernel on crash, needing no stale-lock cleanup). `/delete` acquires a non-blocking exclusive lock and refuses open sessions. Resume acquires the lock before reading the row so a racing delete either sees the lock or completes first. Delete commits row removal before deleting artifacts and the lock file, so crashes leave at most orphaned files. The lock guards delete alone, since concurrent writers are handled by lazy fork.

### Lazy Fork

Any commit that moves `head_id` checks whether the stored head still matches this process's base head (the last head it committed or loaded) inside `BEGIN IMMEDIATE`. Because rows are immutable, a match commits in place while a mismatch forks, requiring no owner row, lease, or clock.

| Sequence on session S                                     | Result                                                |
| --------------------------------------------------------- | ----------------------------------------------------- |
| A and B open S, only B writes                             | B continues S                                         |
| A and B open S, B writes, then A writes                   | A forks to S', and S keeps B's line                   |
| A writes, B opens S and writes, then A writes again       | A forks because B wrote after A's last view           |
| B writes and exits, then A writes from its older view     | A forks                                               |
| B renames or archives S, then A writes                    | A continues S because metadata does not move the head |
| B rewinds S to exactly the head A last saw, then A writes | A continues S, as that head names A's branch          |

In the same transaction, the forking process takes the new lock, inserts the fork row (`forked_from = S`, copying title and manual flag), copies its base chain back to the nearest NULL parent with original message IDs intact (preserving chain tip and rewind checkpoints), writes its new records, adopts the fork's title and manual flag, and releases S's lock. Pre-compaction rows, other branches, and usage stay in S so each request is billed once.

Shared handle state updates `session_id()` and `artifact_dir()` to the fork for later requests and spills (copied tool results still reference S's spills). The `WriteReport` carries `Fork { from, to }`, emitting `Event::SessionForked` (updating the TUI status bar and printing a stderr notice in headless runs, as [Several Processes on One Session](../guide/sessions.md#several-processes-on-one-session) describes). Finish-time file snapshots write only if `head_id` still equals the process's base.

### Export and Import

`ox export` streams JSONL to stdout: one `session` record, all `message` records in commit order (including branches and pre-compaction rows), and all `usage` records with RFC 3339 timestamps. `ox -r <file>` imports and resumes ([Moving a Session Between Machines](../guide/sessions.md#moving-a-session-between-machines)), validating a leading supported-format `session` record with valid ID and parent-before-child ordering (reporting line numbers on error) while deduplicating existing rows.

Because exports include every message, head resolution checks message membership directly so compaction roots and rewinds cannot hide shared ancestry:

| Local store                                  | Result                                                         |
| -------------------------------------------- | -------------------------------------------------------------- |
| No session with the ID                       | Inserted with the export's metadata                            |
| Local head among the exported messages       | Fast-forward: head and metadata from the import                |
| Exported head among the local session's rows | The import is older: the local head and metadata stay          |
| Neither, and a fork of the session relates   | The newest such fork receives the import by the same two rules |
| Neither, and no fork relates                 | The import becomes a fork with `forked_from` set to the ID     |

Newly created or forked imports record the local store's project and launch directory so they appear in the importing project. Any process holding the target session open forks on its next write.

### AI-Generated Titles

In the TUI, a fresh session's first prompt spawns a tokio task calling Haiku via `Client::complete` for a 3-7 word sentence-case title (using structured JSON output unless `CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS` disables it), keeping the first line within the 80-column cap:

- **Request.** Wraps the prompt in `<first_prompt>` tags (cutting prompts over 2,000 chars to the first 1,500 plus tail around `... [N characters omitted] ...`) and ends with a titling instruction so imperative prompts are titled even without a schema.
- **Lifetime.** Owned by the session and aborted on rename, rewind before the first prompt, or session finalization. An already-started write still commits, gated by the manual flag.
- **Usage and failures.** Records usage before parsing so unusable replies are still billed. Generation errors only log a warning, while title or usage write errors surface as session write errors.

`/rename` sets `title_manual` (and resume or fork copies the source's flag), blocking automatic overwrites both in state and at commit while set. Every title write or fork refreshes the stored title into state so cross-process renames propagate, and the task announces its title only if that title still holds after commit.

### Session Cost

Each turn, compaction summary, or AI title request inserts a `usage` row keyed by per-request ID (deduplicating storage) with `model`, `cache_ttl` (`"5m"` or `"1h"`), and the four token counts, allowing accurate pricing across model or TTL changes. Resume sums every usage row of the session (including abandoned branches and pre-compaction requests) into the session total, `/clear` resets it, and a fork's resumed cost counts only its own requests while the live status bar keeps the process total.

## Listing

[Listing Sessions](../guide/sessions.md#listing-sessions) covers CLI flags, columns, and archive views. Pages use indexed keyset queries ordered by `(updated_at, id)` newest-first (hiding newer content formats, running on a worker thread in the picker) so pages never overlap. Prefix lookups reject characters invalid in session IDs so GLOB metacharacters never reach SQL and queries hit the primary key. Display escapes control characters and line breaks in titles and project paths so each row stays single-line and terminal-safe.

## Design Decisions

1. **One store.** SQLite is the single source of truth and JSONL is interchange only, avoiding drift or reconciliation between two stores.
2. **Bundled SQLite.** Gives every supported platform the multi-process WAL-reset fix.
3. **JSON payload columns.** Serde payloads evolve without schema churn while SQL-queried fields remain columns.
4. **Immutable rows and a head pointer.** Moves the head cleanly across rewind, compaction, and fork while keeping history for export and making the head check an exact fork detector.
5. **Lazy fork by head check.** Forks only on an actual concurrent write conflict, needing no owner, lease, or clock (where Codex refuses a second writer at open and oh-my-pi assigns a first-writer lease).
6. **Import by membership.** Message membership stays exact across compaction and rewind, and reusing a related fork keeps repeated diverged imports on one fork.
7. **Messages keyed by session and ID.** Preserving UUIDs in forks and diverged imports keeps live checkpoints valid without remapping.
8. **Usage keyed by request and never copied.** Stores each request once so cross-session totals never double-count.
9. **Liveness locks for delete only.** Kernel locks vanish on process exit, eliminating stale-lock recovery.
10. **No retry.** With atomic transactions, remaining SQLite failures are persistent, so stopping persistence with one report avoids broken parent chains.
11. **Writes under a lock.** Callers need checkpoints, title seeds, and write reports synchronously before continuing, so a write channel adds no pipelining. Locking on a blocking thread serializes writes off the async runtime without a separate writer task (Codex queues rollout writes without an ack, while pi appends inline).
12. **Derived title latch and a persisted manual flag.** Deriving the first-prompt title opportunity from the chain and gating overwrites solely on `title_manual` prevents state drift.
13. **Fork labels from `forked_from`.** Distinguishes forks while keeping the user's title intact.
14. **Search views preserve native result rows.** `tool_metadata` records the call's title and optional `result_view` (structured view or plain-text marker). Content-mode Grep, Glob, and Edit persist structured views so filenames with colons, newlines, or trailing whitespace and search-time path spellings survive replay. Results with notes or spilled output store the text marker so warnings and spill paths render, and errors always render error text. Read and Tasks omit stored views (deriving from result text and call input) to avoid duplicating file bodies or task lists, and missing views fall back to each tool's derivation.

## Out of Scope

- **Rewind history.** Abandoned rows already stay in the store, so browsing them needs no format change ([#293](https://github.com/hakulab/oxide-code/issues/293)).
- **Subagent sessions.** A parent session column would arrive as an additive revision ([#136](https://github.com/hakulab/oxide-code/issues/136), [#285](https://github.com/hakulab/oxide-code/issues/285)).

## Sources

- `crates/ox-core/src/message.rs`: the message and content-block types that the message rows store.
- `crates/ox-core/src/session/display.rs`: the session ID and title formatting that listings use, including the fork label.
- `crates/ox-core/src/session/handle.rs`: `SessionHandle`, which commits each write under the state's lock, `WriteReport`, and `Fork`, and the session-format tests.
- `crates/ox-core/src/session/resolver.rs`: the `--continue` and `--resume` targets, `ox export`, and session-ID prefix resolution.
- `crates/ox-core/src/session/sanitize.rs`: resume-time transcript repair.
- `crates/ox-core/src/session/saved.rs`: archiving and deleting saved sessions.
- `crates/ox-core/src/session/state.rs`: the in-memory session state that builds each commit.
- `crates/ox-core/src/session/store.rs`: the data directory, the setup lock, connections, liveness locks, archive, and delete.
- `crates/ox-core/src/session/store/commit.rs`: the write transaction, with its head check and lazy fork.
- `crates/ox-core/src/session/store/export.rs`: the JSONL interchange records, export, and import by message membership.
- `crates/ox-core/src/session/store/listing.rs`: the page, latest, and prefix queries.
- `crates/ox-core/src/session/store/load.rs`: the chain query and the decoding of a resumed session.
- `crates/ox-core/src/session/store/prompt_history.rs`: the batched prompt-history walk.
- `crates/ox-core/src/session/store/schema.rs`: the pragmas, the schema, and its revisions.
- `crates/ox-core/src/session/title_generator.rs`: the background AI title generator.
