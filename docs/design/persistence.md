# Session Persistence

`ox` stores every session in one SQLite database, commits each write to a session under its state's lock, and rebuilds the transcript on resume from the chain its head names. JSONL is the interchange form that `ox export` writes and `ox -r <file>` imports.

## Storage Layout

```text
$XDG_DATA_HOME/ox/
├── sessions.db                   # The store, with sessions.db-wal and sessions.db-shm beside it
├── artifacts/{session-id}/       # Spill files under tool-results/, created by the first artifact
├── locks/{session-id}.lock       # Held shared while a process has the session open
└── locks/sessions.db.lock        # Held while a process sets up the store
```

The data directory is created with mode `0700`. On Unix the store creates `sessions.db` with mode `0600` before SQLite opens it, since SQLite gives the `-wal` and `-shm` files the main file's mode. The database must live on a local filesystem, because WAL [does not work over a network filesystem](https://www.sqlite.org/wal.html). SQLite cannot detect one reliably, so the [guide](../guide/sessions.md#where-sessions-live) states the requirement and the store does not probe for it.

A session's artifacts, such as [spilled tool output](truncation.md#spill-files), live in `artifacts/{session-id}/`, which `store::artifact_dir` names. The first artifact creates the directory, and `SessionStore::delete` removes it with the rows.

## Connection

- **Bundled SQLite.** `rusqlite` builds SQLite from source, so every platform gets the fix for the WAL-reset bug that corrupts a database when connections in separate processes write or checkpoint concurrently ([how to corrupt](https://www.sqlite.org/howtocorrupt.html), fixed in [3.51.3](https://www.sqlite.org/releaselog/3_51_3.html)). That is the multi-process case `ox` runs in, and the system SQLite on macOS and older distributions predates the fix.
- **Open.** Each process opens the store once at startup, and the TUI's `/resume` picker reuses that handle. `SessionStore::open` holds an exclusive lock on `locks/sessions.db.lock` while `schema::open` switches a fresh file to WAL and creates its schema, because leaving rollback mode upgrades a read lock to a write lock, which SQLite fails at once without calling the busy handler. A store already at this revision opens without taking the write lock, and only a migration runs under `BEGIN IMMEDIATE`. A file SQLite cannot read fails the open with the recovery steps in the order the [sessions guide](../guide/sessions.md#where-sessions-live) gives them: quit every `ox`, run `sqlite3 .recover` while the WAL still sits beside the database, then move the database aside together with its `-wal` and `-shm` files. The open reads only the header and the schema, so damage deeper in the file surfaces at the first write or listing that reaches it.
- **Per connection.** `synchronous = NORMAL`, `checkpoint_fullfsync = ON`, a 5 s busy timeout, and `foreign_keys = ON`. In WAL mode, `NORMAL` survives a process crash, and power loss may roll back the last commits but leaves the file intact as long as each checkpoint reaches the disk in order. macOS orders a sync only with `F_FULLFSYNC`, which `checkpoint_fullfsync` turns on for checkpoints at the cost of one full flush per checkpoint, and other systems ignore it.
- **Transactions.** Writes run under `BEGIN IMMEDIATE`, so a commit's head check and the writes that depend on it happen under the write lock. Reads use deferred transactions and never write.

## Schema

Columns hold what SQL filters, sorts, joins, or compares. JSON text holds the payloads only Rust reads, through their serde types, so content blocks evolve under the rules in [Compatibility](#compatibility). Timestamps are Unix milliseconds, and every table is `STRICT`.

| Table      | Row                    | Columns                                                                                                                                                                                                                        |
| ---------- | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `sessions` | One per session        | ID, `forked_from`, project root, launch directory, Git branch, model, content `format`, created and last-active times, title and whether `/rename` set it, archive time, `head_id`, message count, and the file snapshots JSON |
| `messages` | One per message        | Commit-order `seq`, session ID, message ID, `parent_id`, role, content JSON, and the JSON of the prompt `context` it first carries, the `compact` boundary it opens as a root, and its results' `tool_metadata`                |
| `usage`    | One per billed request | Request ID, session ID, the usage record JSON, and its time                                                                                                                                                                    |

1. **Immutable rows and a head pointer.** Rows never change once written, and only deleting their session removes them. `head_id` names the tip of the active branch, so rewind, compaction, and fork move the head while abandoned rows stay for export. The chain from the head back to the nearest NULL parent, the session start or the last compaction root, is the transcript resume reads.
2. **Parents first.** `messages` keys on `(session_id, id)`, and a composite foreign key on `(session_id, parent_id)` makes "a message never precedes its parent, and both belong to one session" a database invariant.
3. **Last activity.** `updated_at` changes only when the head moves, so renaming or archiving a session keeps its place in listings.

## Compatibility

The store follows the [stability surface](../../RELEASING.md#stability-surface) through two version numbers:

- **Revision** (`PRAGMA user_version`). A revision only adds tables, indexes, and nullable or defaulted columns. A newer binary migrates under `BEGIN IMMEDIATE`, and an older binary keeps working on a newer revision, since nothing it relies on changed. An incompatible schema change would write a new file, such as `sessions-2.db`, so an older binary keeps the file it understands.
- **Content format** (`sessions.format`). Every message commit stores the binary's `CONTENT_VERSION`. It bumps when an older binary would misread a newer payload: when a closed serde enum, such as a content block `type`, a text `origin`, a reasoning `wire`, or a task `status`, gains a value, or when a field older binaries require goes away. Listings and `ox -c` hide sessions of a newer format, and loading or importing one fails with an upgrade message.

Within a format, a newer binary reads an older payload as long as each change is an addition that deserializes to a default when absent, a removal of a field the loader ignores, or a changed shape with a loader that still reads the old one.

A reasoning block stores the `wire` and `model` that produced it alongside a `payload` that only that wire can read, such as the `signature` of an Anthropic `thinking` block or the `data` of a `redacted_thinking` block. The Anthropic wire sends every Anthropic block back unchanged whichever model produced it, as Anthropic's guide to [switching models](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models) asks, so the recorded `model` changes no request. Which blocks a model reads depends on the platform, and the API drops or ignores the rest, as the [research note](../research/api/extended-thinking.md#constraints) details.

A server tool's result, such as a `web_search_tool_result`, is stored as a `server_tool_result` block that keeps the API's block type in `kind` and every other field unchanged, so the Anthropic wire sends it back as the API returned it.

Generated text blocks carry an optional `origin`: `system_reminder`, `interruption`, `compaction_summary`, or `compaction_truncation`. The provider request keeps the original text and omits this field. Transcript displays use it to hide reminders, render interruptions, and show a compact summary once. Text without an origin stays visible even when it contains a reminder tag or interruption marker.

Tests in `session/handle.rs` enforce the guarantee for stable releases, using two fixtures per release starting with `v0.1.0`, in `crates/ox-core/src/session/fixtures/` and named after the tag:

- **Fixture source.** One test writes a session holding every row kind and content block, with messages, prompt contexts, and metadata after its compaction so a resume reads them. `session_format::writes_every_row_kind` snapshots a deterministic SQL dump of the store, its schema and then its rows with per-run UUIDs and timestamps masked, and `session_format::exports_every_row_kind` snapshots the session's masked export. For each stable tag, the release PR copies the two snapshots as `{tag}.sql` and `{tag}.jsonl`, as [Prepare a Release](../../RELEASING.md#prepare-a-release) describes.
- **Resume check.** `session_format::resumes_from_every_release_fixture` fills each fixture's masks with fixed values, loads the dump or imports the export into a fresh store, then lists and resumes the session. It snapshots what the listing and the resume recovered: the title, branch, archive state, compaction, prompt context, transcript, tool metadata, recorded usage, and file snapshots. It then appends a message and checks that the message continues the resumed chain. The same check runs on the dump this build writes, so a pull request that changes the format also shows how its sessions will resume.
- **Changed behavior.** If a change alters how an old release's session resumes, that fixture's snapshot fails. Fixtures are frozen release output, so never edit them. Accept a changed fixture snapshot only when this section allows the change.

## Session Lifecycle

### Locked Writes

Each open session keeps one `SessionState` behind a mutex that every `SessionHandle` clone shares. A write runs on a `spawn_blocking` thread, where it takes the lock, applies its change to the state, and commits what the change queued as one transaction, so waiting for another process's write lock leaves the async runtime available. The state owns one connection, opened by its first commit. The engine and the title task are the only writers, and each awaits its own writes in order, so writes from the two run in lock order, which the in-state manual-title check makes safe. A caller that stops waiting leaves its write to finish on the blocking thread. A write that panics while holding the lock stops persistence as a failed commit does, and the panic also reaches the caller, so a panicking engine write ends the engine and `ox` exits with the panic. The recovered lock mainly keeps the conversation going after a panicking title-task write.

`agent_turn` records a tool round with one `SessionHandle::record_round` write, which saves the assistant row and then the row of the tool results answering it, carrying their metadata. Because one commit holds the whole round, a resumed session always reads the assistant turn before the results that answer it. Resume sanitization drops a tool call whose result is missing.

### Lazy Materialization

A fresh session's row is inserted from the in-memory state in the transaction of its first message, title included. A session that exits before any message leaves no row, and a `/rename` before the first message lands with it.

### Write Failures

A failed commit rolls back its whole transaction and stops persistence for the rest of the process's session, while the conversation continues in memory. Nothing is retried, since a later commit would chain onto rows that never landed and the failures SQLite still reports, such as a full disk, an I/O error, or a lock held past the busy timeout, do not clear on a blind retry. Every later commit with records fails with a stopped-persistence error that names the first failure, and resuming later continues from the last commit. Each failed commit is logged on its blocking thread, including one whose caller stopped waiting.

Each write returns a `WriteReport`, whose `failure` the engine turns into one `Event::Error`:

- A failure is reported once per session, by the first write that fails, and the writes after it report none.
- An archive reports every failure, since a bare `/archive` rolls to a fresh session only once its archive commits.
- A compaction always takes effect in memory, and its boundary is on disk exactly when its commit landed. The engine reports the compaction's write error after `SessionCompacted`, which clears the TUI's chat, so the notice stays visible.

### Resume and Sanitization

`ox -c`, `ox -r`, and `/resume` take the session's liveness lock, read its row, and walk the chain from `head_id` with a recursive CTE until a NULL parent. The newest `context` on the chain is replayed, the root's `compact` gives the boundary, `tool_metadata` merges along the chain, and every `usage` row of the session is summed. The cost is proportional to the chain since the last compaction. The sanitization pipeline then drops tool calls without results and results without calls, and injects head / tail sentinels. It preserves message boundaries, including adjacent messages with the same role, which the [Messages API](https://platform.claude.com/docs/en/api/messages/create) accepts. A client call pairs with a user `tool_result`, and a server call with the result block in the same or a later reply. A server call that a `pause_turn` left waiting is dropped, because the next prompt closes the turn and the API rejects a call without its result.

### Refused Turns

The API keeps refusing while a refused turn stays in the conversation, so when the model stops with `refusal`, the engine rolls the whole turn back: its prompt, any queued prompts it took in, and its tool rounds. It truncates the live transcript to its length before the prompt, and it restores what the model had seen of the user context and the file tracker. It also moves the session's head back to the message the prompt chained from, so a resume leaves the turn out too. The next prompt then chains from that message and records the prompt context again if the refused prompt was the first to carry it. A rewind to before the first prompt stops the title request that prompt started, so the next prompt titles the session unless the user named it manually. The engine emits `TurnRolledBack` ahead of the turn's `TurnFailed`, and the TUI hands the prompts back: the refused one into the empty input for rephrasing, and the queued ones the turn took in to the head of the queue, which waits for the user. A turn that compacted partway through rolls back only to the compaction's synthetic message. The summary already covers the prompt and the queued prompts the turn took in before it, so the TUI hands back only the queued prompts the turn took in after the compaction. The turn's requests stay in the session's cost estimate, and its tool calls' effects remain, such as the files it changed and the background shells it started.

Resume derives the first-prompt title opportunity from what it read: the opportunity is used once the chain holds a user prompt or starts at a compaction root. A manual title skips it, as [AI-Generated Titles](#ai-generated-titles) describes.

### Prompt Context

A session records its prompt context: the environment section, the first message's rendered instructions, date, model identity, and what the model has seen of them. [System Prompt](prompt.md) covers what the context holds and how later changes reach the model.

- **Record with the carrying message.** The context is stored on the row of the first user message that carries it. A compaction records the refreshed context on its synthetic root the same way.
- **Replay the resolved branch.** On resume, the newest context on the chain is replayed verbatim, so the request sends the same cached system prompt and first message.

### Task List

The session's [task list](tasks.md) needs no column of its own. On resume, the loader takes it from the last successful `tasks` call on the chain, or else from the `tasks` field of the root's compaction record, which holds the list the summarized transcript left.

### Archiving

`archived_at` sets whether default listings and `ox -c` skip the session, and archiving leaves `updated_at` alone. One conditional update moves a session only out of the other state, so an archive keeps its first archive time, and the update's row count tells two processes archiving one session which of them changed it. Bare `/archive` writes the archive to the live session before the roll that finalizes it. `/archive <prefix>`, `/unarchive <prefix>`, and Ctrl+X in the `/resume` picker update another session's row. Each of these names the session by an ID or a prefix, and the engine resolves it, owns the archive through completion, and runs the update on a blocking worker, keeping input and rendering available while another process holds the write lock. Later session operations wait for that completion, and the result reaches chat even after the picker closes. Resuming an archived session unarchives it.

### Liveness Locks

A process that opens a session for writing takes a shared lock on `locks/{session-id}.lock` and holds it until its last handle on the session drops, which finalizing the session on a roll or at exit does. The kernel releases the lock when the process dies, so no stale-lock cleanup exists. `/delete` takes a non-blocking exclusive lock on the same file and refuses a session another process has open. Resume takes the lock before it reads the row, so a racing delete either sees the lock or finishes first and the resume finds no session. Delete commits the removal of the rows first and then removes the artifacts and the lock file, so a failure leaves at most orphaned files.

The lock serves delete alone. Two processes holding one session open is the normal case that the lazy fork handles.

### Lazy Fork

A commit that moves the head first compares `head_id` with the head this process last committed or loaded, its base. Rows are immutable, so a matching head means the active branch is exactly the one this process holds in memory, and the commit lands in place. A different head means another process moved it since this process last looked, so the commit forks. The check runs under `BEGIN IMMEDIATE` with the writes that follow, and it needs no owner row, lease, or clock.

| Sequence on session S                                     | Result                                               |
| --------------------------------------------------------- | ---------------------------------------------------- |
| A and B open S, only B writes                             | B continues S                                        |
| A and B open S, B writes, then A writes                   | A forks to S', and S keeps B's line                  |
| A writes, B opens S and writes, then A writes again       | A forks, since B wrote after A's last view           |
| B writes and exits, then A writes from its older view     | A forks                                              |
| B renames or archives S, then A writes                    | A continues S, since metadata does not move the head |
| B rewinds S to exactly the head A last saw, then A writes | A continues S, since that head names A's branch      |

In the same transaction, the forking process takes the fork's liveness lock, inserts the fork's row from its in-memory state with `forked_from` set to S and the title and manual flag copied from S, copies its base chain back to the nearest NULL parent into the fork with the message IDs unchanged, and writes its records there. The process then takes the fork's title and manual flag as the commit left them, and releases S's lock. Pre-compaction rows, other branches, and usage stay in S, so every request is counted once, in the session that made it. Keeping the message IDs keeps the process's chain tip and rewind checkpoints valid in the fork.

The handle's shared state holds the live session ID and artifact directory, so `session_id()` and `artifact_dir()` follow the fork, and the next request and tool call use them. New spills land under the fork, while spill paths in copied tool results still name S's directory. The report of the write whose commit forked carries `Fork { from, to }`, and the engine emits `Event::SessionForked`. The TUI shows `Another ox process wrote to session {from8}, so this conversation continues as session {to8}` and moves its status bar to the fork, the stdio drivers print the notice to stderr, and the headless result reports the fork's ID. Listings label the fork `fork of {from8}` beside its title. File snapshots at finish are written only while the session's head still equals the process's base, since they describe the files as of that head.

### Export and Import

`ox export <id-prefix>` writes a session as JSONL to stdout: a `session` record first, then every `message` record in commit order, every branch and pre-compaction message included, then every `usage` record, with RFC 3339 timestamps. `ox -r <file>` imports an export and resumes the session that holds its head. Import checks that the input starts with a `session` record of a valid ID and a supported format and that each parent precedes its children, and its errors name the line. Rows already present are kept once. An export carries every message its session ever had, so the head lands by which messages each side holds, a lookup that compaction roots and rewinds cannot hide the way they cut the chain:

| Local store                                  | Result                                                         |
| -------------------------------------------- | -------------------------------------------------------------- |
| No session with the ID                       | Inserted with the export's metadata                            |
| Local head among the exported messages       | Fast-forward: head and metadata from the import                |
| Exported head among the local session's rows | The import is older: the local head and metadata stay          |
| Neither, and a fork of the session relates   | The newest such fork receives the import by the same two rules |
| Neither, and no fork relates                 | The import becomes a fork with `forked_from` set to the ID     |

A session the import creates, new or a fork, records the importing store's project and launch directory, as a fresh session does, so it lists in the project that imported it. A process holding the session open while an import moves its head forks at its next write, as it would for any other writer.

### AI-Generated Titles

In the TUI, the first user prompt of a fresh session spawns a tokio task that asks Haiku through `Client::complete` for a 3-7 word sentence-case title, constrained by a structured-outputs JSON schema unless `CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS` turns it off, and keeps the first line of the reply under the same 80-column cap as other titles. The request wraps the prompt in `<first_prompt>` tags, cutting a prompt over 2,000 characters to its first 1,500 and as much of its end as fits around a `... [N characters omitted] ...` line, so a request that leads a long paste survives. The turn ends with an instruction to title the prompt, so that an imperative prompt reads as text to title even without the schema. The session owns the task, and a rename, a rewind to before the first prompt, or finalizing the session on a roll or at exit aborts it first, so a late title never reports on a session that was renamed, rewound, or has ended. A write the task already started still commits, possibly after the session ends, and after a rename the manual flag refuses a late AI title. Once the API answers and reports usage, the task records the request's usage before parsing the reply, since a reply without a usable title is billed too. Generation failures only log a warning, while a failed title or usage write surfaces like any other session write error.

The session state's row owns the manual flag, which `/rename`, a resume, or a fork's copy of its source sets, and while it is set, neither a first-prompt nor a generated title replaces the manual one. The title commit also refuses to replace a manual title with a generated one, and every commit that writes a title or forks reads the stored title back into the row, so a `/rename` from another process reaches this process at its next title write. The task announces its title only when the title is still the session's once its write commits.

### Session Cost

Each request behind a turn, a `/compact` summary, an auto-compaction summary, or an AI title adds a `usage` row. Its record holds the request's `model` ID and `cache_ttl` (`"5m"` or `"1h"`) and the four provider counts `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, and `output_tokens`, so the engine prices each request at the rates it ran with, even after a `/model` swap or a TTL change. Rows are keyed by a per-request ID, so a request is stored once.

On resume, the loader returns every usage row of the session, including those from abandoned branches and before compaction boundaries, since the session paid for each. The engine sums their estimated cost into the session total, which `/clear` resets with the fresh session. A fork's resumed cost covers only its own requests, while the live status bar keeps the process's running total.

## Listing

- **Scope.** `ox --list`, bare `ox -c`, and the `/resume` picker list the current project's sessions, and `--all` widens to every project. A prefix lookup that matches nothing in the current project searches every project.
- **Query.** A page is one indexed query ordered by `updated_at` and then ID, newest first, and the picker's next page continues after the last row through a keyset cursor, so pages never overlap. Sessions of a newer content format stay hidden. The picker reads pages on a worker thread, off the UI thread.
- **Prefix lookup.** A prefix holding a character no session ID holds matches nothing, so GLOB metacharacters never reach SQL, and the lookup uses the primary key.
- **Archive view.** `--list` and the `/resume` picker show unarchived sessions, or only archived ones with `--list --archived` and the picker's Ctrl+A.

Listings render rows as follows:

- Sessions without a title are shown as `(untitled)`, and a fork's title is followed by `(fork of {from8})`.
- `Msgs` counts the active branch's messages since its last compaction.
- Under `--all`, a `Project` column shows the tildified project root.
- Titles are truncated to the terminal width with a trailing `...`, before a fork's label, which stays whole.
- Control characters in titles and project paths, line breaks included, are escaped, so a row stays on one line and cannot change the terminal's state.

## Design Decisions

1. **One store.** SQLite holds every session and JSONL is only interchange, so no second copy can drift and no read path reconciles two stores.
2. **Bundled SQLite.** Every supported platform gets the WAL-reset fix, which multi-process use needs.
3. **JSON payload columns.** Message content, context, compaction info, tool metadata, and usage keep their serde types, so their evolution rules stay as they are and only searchable or sortable fields become columns.
4. **Immutable rows and a head pointer.** Rewind, compaction, and fork move the head, history stays for export, and the head names its chain exactly, which makes the head check a complete fork detector.
5. **Lazy fork by head check.** A session forks only after two processes both wrote to it, never on open, and the check needs no owner, lease, or clock. Codex refuses a second writer at open and oh-my-pi gives the first writer a lease, and both decide before anyone writes.
6. **Import by membership.** An export holds every message, so whether it saw the local head is a lookup that compaction and rewind leave exact, and reusing a related fork keeps repeated imports of a diverged session on one fork.
7. **Messages keyed by session and ID.** A fork or diverged import copies rows with their UUIDs, so live checkpoints stay valid without remapping.
8. **Usage keyed by request and never copied.** Each request is stored once, so totals across sessions stay exact.
9. **Liveness locks for delete only.** Kernel-held locks vanish with their process, so delete safety needs no stale-lock recovery.
10. **No retry.** Without torn writes, the failures left are not transient, so a failed commit stops persistence with one report.
11. **Writes under a lock.** Every caller needs its write's checkpoint, title seed, and report before it goes on, so a write channel would buy no pipelining. A lock taken on a blocking thread keeps one write at a time, and since there is no writer task, there is no path for a writer that is gone. Codex queues its rollout writes without an ack, which suits a loop that never waits on disk, and pi appends in the caller, the shape this lock keeps off the async runtime.
12. **Derived title latch and a persisted manual flag.** The title opportunity follows from the chain, and `title_manual` alone gates every automatic title, so no second copy can disagree with either.
13. **Fork labels from `forked_from`.** The fork keeps the user's title unchanged, and the label tells the two rows apart.
14. **Search views preserve native result rows.** A result's `tool_metadata` records the call's title and, when the tool sets one, a `result_view`, which is either the structured view or a marker that the result reads as plain text. Grep in content mode, Glob, and Edit store their structured view, so complete filenames and line numbers survive even when a path contains colons, newlines, or trailing whitespace, and stored paths retain their search-time display spelling on replay. Search results that carry notes, and output capped into a spill file, store the text marker, so their warnings and spill locations render with the result text. Errors always render their error text. Read and Tasks store no view and derive it on every render, Read from the result text and Tasks from the call's input, since storing it would repeat the file content or the task list. A result without a stored view uses the tool's own derivation.

## Out of Scope

- **Prompt history across sessions** ([#421](https://github.com/hakula139/oxide-code/issues/421)). The store makes it a query over user messages joined to their project, and full-text search would arrive as an FTS5 revision.
- **Rewind history** ([#293](https://github.com/hakula139/oxide-code/issues/293)). Abandoned rows already stay in the store, so browsing them needs no format change.
- **Subagent sessions** ([#136](https://github.com/hakula139/oxide-code/issues/136), [#285](https://github.com/hakula139/oxide-code/issues/285)). A parent session column would arrive as an additive revision.

## Sources

- `crates/ox-core/src/engine/history.rs`: the resumed transcript, whose tool results take their views from `ToolRegistry::result_view`.
- `crates/ox-core/src/engine/post_compact.rs`: the synthetic post-compact root.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, which records each tool round with one `record_round` write.
- `crates/ox-core/src/engine/usage.rs`: `UsageLedger::for_session`, which sums the session cost from the resumed usage rows.
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
- `crates/ox-core/src/session/store/schema.rs`: the pragmas, the schema, and its revisions.
- `crates/ox-core/src/session/title_generator.rs`: the background AI title generator.
- `crates/ox-core/src/tool.rs`: `ToolMetadata`, the fields a result's `tool_metadata` records.
- `crates/ox/src/session_list.rs`: the `ox --list` table.
