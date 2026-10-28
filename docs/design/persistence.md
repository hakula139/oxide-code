# Session Persistence

`ox` stores each session as an append-only JSONL file, writes it through a single actor task, and rebuilds the transcript on resume.

## Storage Format

Every session is a single append-only `.jsonl` file with one JSON object per line.

### Entry Types

| Type                   | Position       | Purpose                                                                                                     |
| ---------------------- | -------------- | ----------------------------------------------------------------------------------------------------------- |
| `header`               | First line     | Session metadata: ID, working directory, project root, Git branch, model, created timestamp, format version |
| `message`              | Middle lines   | Conversation message with `uuid`, optional `parent_uuid`, timestamp                                         |
| `rewind`               | After messages | Moves the active branch's tip back to an earlier message, or before the first one                           |
| `title`                | Re-appended    | Session title. Latest wins                                                                                  |
| `tool_result_metadata` | Sidecar        | Display metadata for replaying tool results                                                                 |
| `file_snapshot`        | Sidecar        | File-tracker state restored on resume when disk state still matches                                         |
| `usage`                | After request  | One billed request's token usage, model, and prompt-cache TTL                                               |
| `context`              | Before message | Prompt context of that message. Latest on the resolved chain wins                                           |
| `compact`              | Boundary       | Summary, pre-compact count, and optional instructions and tasks                                             |
| `archive`              | Re-appended    | Archive state: `archived` + `at`. Latest wins                                                               |
| `summary`              | Last line      | Exit marker: `message_count` + `updated_at`. Latest wins                                                    |

### Schema Invariants

1. **Explicit entry schema.** The loader treats unknown discriminators as malformed lines, so schema compatibility stays explicit.
2. **`parent_uuid` chain.** Each message links to its predecessor. Concurrent resumes can fork, and the newest-leaf rule resolves the active branch without rewriting history. A `rewind` entry moves the tip back the same way, by leaving the abandoned messages in place.
3. **`version` field.** Bump on incompatible changes so older binaries fail fast on session state they cannot read.

## Compatibility

Session files follow the [stability surface](../../RELEASING.md#stability-surface): within a minor version, each stable release can resume session files written by earlier stable releases of that minor version. Before 0.1.0, a release may stop reading files from earlier versions, and an earlier prerelease binary may skip lines in a newer file without a header `version` bump. The loader treats a line in a shape it no longer reads as malformed and skips it. Since a skipped message breaks the chain that resume follows back from its newest leaf, the session resumes with only the messages after the last skipped one.

Within the stability surface, a newer binary can read an older file as long as every format change is one of the following:

- **Additions.** A new entry type, or a new field that deserializes to a default when absent.
- **Removals.** A removed field, since the loader ignores fields it does not know. The data the field held is lost on resume.
- **Changed shapes with a loader.** Renaming a field or changing its type or meaning requires a loader that still reads the old shape.

An older binary reading a newer file skips entry types it does not know as malformed lines and ignores unknown fields. Bump the header's `version` only when an older binary would misread a newer file, since an older binary refuses any file whose `version` is newer than it supports. A prerelease binary that predates `rewind` entries skips them, so until a newer message follows a rewind it resumes the abandoned branch. Binaries through 0.1.0-alpha.5 require a `source` field on `title` entries, so they skip the titles later binaries write and list those sessions as untitled.

A reasoning block stores the `wire` and `model` that produced it alongside a `payload` that only that wire can read, such as the `signature` of an Anthropic `thinking` block or the `data` of a `redacted_thinking` block. The Anthropic wire sends every Anthropic block back unchanged whichever model produced it, as Anthropic's guide to [switching models](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models) asks, so the recorded `model` changes no request. Which blocks a model reads depends on the platform, and the API drops or ignores the rest, as the [research note](../research/api/extended-thinking.md#constraints) details.

A server tool's result, such as a `web_search_tool_result`, is stored as a `server_tool_result` block that keeps the API's block type in `kind` and every other field unchanged, so the Anthropic wire sends it back as the API returned it.

Generated text blocks carry an optional `origin`: `system_reminder`, `interruption`, `compaction_summary`, or `compaction_truncation`. The provider request keeps the original text and omits this field. Transcript displays use it to hide reminders, render interruptions, and show a compact summary once. Text without an origin stays visible even when it contains a reminder tag or interruption marker. For older files, the loader restores missing origins only on the synthetic root identified by a committed `compact` boundary, assigning the first text block to the summary and subsequent text blocks to reminders.

An unknown `wire`, content block `type`, or text `origin` value fails to parse in a reader that recognizes that field, so it skips the whole message as a malformed line. From 0.1.0 on, adding a value to one of these closed enums needs a header `version` bump. A task `status` works the same way inside a `compact` entry: an unknown one makes an older binary skip the boundary, so adding one needs a bump too. Removing a field that older binaries require makes them skip the entry in the same way, so from 0.1.0 on that needs a bump as well.

Tests in `session/handle.rs` enforce this guarantee for stable releases, using one fixture per release starting with `v0.1.0`. Each fixture lives in `crates/ox-core/src/session/fixtures/` and is named after its tag:

- **Fixture source.** One test writes a session containing every entry type and content block, then adds messages, prompt contexts, and sidecars after compaction so a resume reads them. It snapshots the file after masking its per-run UUIDs, timestamps, and working directory. For each stable tag, the release PR copies that snapshot as the fixture for the new release, as described in [Prepare a Release](../../RELEASING.md#prepare-a-release).
- **Resume check.** Another test fills each fixture's masks with fixed values, then lists and resumes it. It snapshots what the listing and the resume recovered: the title, branch, archive state, compaction, prompt context, transcript, sidecars, and recorded usage. It then appends a message and checks that the message continues the resumed chain. The same check runs on the session this build writes, so a pull request that changes the format also shows how its files will resume.
- **Changed behavior.** If a change alters how an old file resumes, that fixture's snapshot will fail. Fixtures capture frozen release output, so never edit them. Accept a changed fixture snapshot only when the change is allowed by this section.

## Storage Layout

Sessions live in `$XDG_DATA_HOME/ox/sessions/{project}/`. `{project}` is a filesystem-safe name derived by `project_dir_name` from the project root, which is the Git repository containing the working directory, or the working directory itself outside a repository. Characters other than ASCII letters, digits, `.`, `_`, and `-` become `-`, and a name longer than 80 characters is cut and given an xxh64 suffix over the raw path bytes.

Distinct paths can share a name, such as `/work/a b` and `/work/a-b`, so the header records `project_root`, and a project-scoped listing keeps only the files whose header names its project. Headers written by 0.1.0-alpha.5 and earlier have no `project_root`, and those files sit under the name of the directory the session started in. A project-scoped listing also reads the working directory's subdirectory, and keeps a file when its header records the project's root, when its `cwd` is the working directory, or when it records no root and its `cwd` is the project root. Files are never moved, so older binaries keep reading the sessions they wrote.

File naming: `{unix_timestamp}-{session_id}.jsonl`. Timestamp prefix gives chronological listings, while lookup by session ID uses the suffix, which `SessionStore::find_session_path` matches in every project. Listings skip a file whose name lacks its header's session ID, such as a renamed copy, so every listed ID leads back to its own file. `ox -r <file>` still resumes such a file by path.

A session's artifacts, such as [spilled tool output](truncation.md#spill-files), are stored in a `{session_id}/` directory beside its file wherever the file lives, so a session resumed from a file outside the store keeps them there. Named by `store::artifact_dir`, the directory is created by the first artifact and removed with the file by `SessionStore::delete`. Listings skip it because they read only `.jsonl` files.

On Unix, session files are created with mode `0600`.

## Session Lifecycle

### Actor and Batched Writes

Each process uses a single `tokio::spawn`-ed actor task for its session. The rest of the program holds a `SessionHandle` that forwards operations as `SessionCmd` over a bounded mpsc channel, with each cmd carrying a oneshot ack. The actor's loop awaits the first cmd, drains any queued commands with `try_recv()`, then moves its state and entries into a `spawn_blocking` operation. That operation flushes the batch and returns the state before the actor sends its acknowledgments, so waiting for another process's file lock leaves the async runtime available.

`agent_turn` records a tool round with one `SessionCmd::RecordRound`, which writes the assistant message, the tool results answering it, and their sidecars contiguously in that order. The text-only branch records its single assistant message through the same actor. Because one command owns the whole round, a resumed session always reads the assistant turn before the results that answer it, even when the runtime's cooperative budget runs out between awaits. Resume sanitization drops a tool call whose result was torn off.

### Writer Recovery on Write Error

- `WriterStatus` is either `Pending` (file not yet on disk, staged header plus an optional deferred title) or `Active(SessionWriter)`.
- Entries stay queued in `SessionState` until a write succeeds, and each batch retries the queue ahead of its own entries, so a message never reaches disk before its parent.
- The flush that creates the file writes the header and its batch into a private temporary file in the session directory, then publishes the complete file without replacing an existing session. A failed write removes only that private file, so the session stays `Pending` and a concurrent creator's session survives.
- Each append holds the session directory's exclusive writer lock from opening the transcript through length inspection, torn-tail repair, every short write and retry, and rollback. The lock lives in a private `.writer.lock` file beside the sessions, so Windows readers can keep reading the transcript. The file stays in place across session deletion and recreation. The writer guard explicitly releases the lock before the batch is acknowledged, including after a failure and for sessions resumed from an explicit path.
- A failed flush with more than 8 MiB to write drops the queue and stops persistence for the rest of the session.

Every append first terminates a torn last line. A process crash can leave complete entries from its unacknowledged batch, while prior acknowledged batches survive. A failed append truncates its bytes while still holding the lock, so another process can append only after rollback finishes. When truncation fails, the retry repeats the lines that landed, and the loader keeps one copy of each message and `usage` entry by its `uuid`. Until persistence stops, a compact whose write fails is unqueued and never committed, matching the agent, which keeps its pre-compact transcript. The loader applies a compact boundary only once its post-compact root follows, so a boundary whose root never landed leaves the old chain in place.

### Lazy Materialization

Starting a session stages the header in memory. The on-disk file is created by the first batch flush carrying real content. A session that exits before any message leaves no artifact.

### Resume and Sanitization

`ox -c` reopens the existing session file. The loader walks `Entry::Message` lines into a UUID-indexed DAG, picks the newest-timestamped leaf as the tip, and walks back via `parent_uuid` to produce a linear chain. A `rewind` entry moves the tip to the message it names and abandons the other leaves, so the newest-leaf rule skips them from then on. Its tip ends the chain until another message line follows it in the file. A rewind naming a message the loader has not read, such as one before the last compaction, is ignored. The sanitization pipeline then drops tool calls without results and results without calls, and injects head / tail sentinels. It preserves message boundaries, including adjacent messages with the same role, which the [Messages API](https://platform.claude.com/docs/en/api/messages/create) accepts. A client call pairs with a user `tool_result`, and a server call with the result block in the same or a later reply. A server call that a `pause_turn` left waiting is dropped, because the next prompt closes the turn and the API rejects a call without its result.

### Refused Turns

The API keeps refusing while a refused turn stays in the conversation, so when the model stops with `refusal`, the engine rolls the whole turn back: its prompt, any queued prompts it took in, and its tool rounds. It truncates the live transcript to its length before the prompt, and it restores what the model had seen of the user context and the file tracker. It also appends a `rewind` entry whose tip is the message the prompt chained from, so a resume leaves the turn out too. The next prompt then chains from that message and records the prompt context again if the refused prompt was the first to carry it. A rewind to before the first prompt stops the title request that prompt started, so the next prompt titles the session unless the user named it manually. The engine emits `TurnRolledBack` ahead of the turn's `TurnFailed`, and the TUI hands the prompts back: the refused one into the empty input for rephrasing, and the queued ones the turn took in to the head of the queue, which waits for the user. A turn that compacted partway through rolls back only to the compaction's synthetic message. The summary already covers the prompt and the queued prompts the turn took in before it, so the TUI hands back only the queued prompts the turn took in after the compaction. The turn's requests stay in the session's cost estimate, and its tool calls' effects remain, such as the files it changed and the background shells it started.

The rewind records `first_user_prompt_seen`, including whether a manual title consumed the opportunity, so title eligibility survives exit and resume. The loader applies that state in record order, and a later title or ordinary user prompt consumes it. An ordinary prompt or compact summary retained on the resolved chain also consumes the opportunity. Older rewinds without the field fall back to the retained chain and title, preserving manually named sessions even when they contain no prompt.

### Prompt Context

A session records its prompt context: the environment section, the first message's rendered instructions, date, model identity, and what the model has seen of them.

- **Record with the carrying message.** A `context` entry is written in the same batch as the first user message that carries it, immediately before that message and with the same timestamp. A compaction records the refreshed context with its synthetic message the same way. The loader binds the entry to the next message with a matching timestamp, so a context entry stranded by a torn write cannot attach to another writer's message.
- **Replay the resolved branch.** On resume, the latest context entry on the resolved chain is replayed verbatim, so the request sends the same cached system prompt and first message. A forked session replays the context recorded on its own branch.
- **Refresh changed context.** If the session resumes in another directory, the environment section is redetected. Any later instruction, date, or model change, including a `/model` swap, is attached to the next user message as a `<system-reminder>` block, which transcript displays omit, so the system prompt stays byte-identical across a swap.
- **Rebuild missing context.** A transcript lacking a `context` entry, such as one an earlier version compacted without recording it, rebuilds the context and records it with the next message.

### Task List

The session's [task list](tasks.md) needs no entry of its own. On resume, the loader takes it from the last successful `tasks` call on the resolved chain, or else from the `tasks` field of the last `compact` entry, which records the list the summarized transcript left. The field is omitted when the list was empty.

### Archiving

An `archive` entry sets whether default listings and `ox -c` skip the session, and the latest one wins. Bare `/archive` queues one through the actor before the roll that finalizes the session. `/archive <prefix>`, `/unarchive <prefix>`, and Ctrl+X in the `/resume` picker append an entry to another session's file and preserve its mtime within the same writer-lock transaction, so toggling its archive status does not erase concurrent activity. Each of these names the session by an ID or a prefix, and the engine resolves it, owns the archive through completion, and offloads its file I/O to a blocking worker, keeping input and rendering available while another process holds the lock. Later session operations wait for that completion, and the result reaches chat even after the picker closes. Resuming an archived session appends an entry that unarchives it. Files never move, so path-based resume and every other reference to a session file keep working.

### Fork-on-Conflict Concurrency

Two processes may resume the same session and append separate branches. The newest-leaf rule resolves forks deterministically, while the directory's writer lock serializes each complete append batch across processes. A rewind abandons every leaf it moves away from, including one another process wrote, so that branch ends the chain again only once a newer message extends it. A batch may require several writes (`O_APPEND` on Unix, end-of-file-offset writes on Windows), and concurrent appends interleave only between batches. A compaction starts a new chain, so once one process compacts, resume drops the messages another process appends to the old chain after the boundary, whatever their timestamps.

### AI-Generated Titles

In the TUI, the first user prompt of a fresh session spawns a tokio task that asks Haiku through `Client::complete` for a 3-7 word sentence-case title, constrained by a structured-outputs JSON schema unless `CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS` turns it off, and keeps the first line of the reply under the same 80-column cap as other titles. The request wraps the prompt in `<first_prompt>` tags, cutting a prompt over 2,000 characters to its first 1,500 and as much of its end as fits around a `... [N characters omitted] ...` line, so a request that leads a long paste survives. The turn ends with an instruction to title the prompt, so that an imperative prompt reads as text to title even without the schema. The session owns the task, and finalizing the session on a roll or at exit aborts it first, so a late title never writes to or reports on a session that has ended. Once the API answers and reports usage, the task appends the request's `usage` entry before parsing the reply, since a reply without a usable title is billed too. Generation failures only log a warning, while a failed title or usage write surfaces like any other session write error.

### Session Cost

Each request behind a turn, a `/compact` summary, an auto-compaction summary, or an AI title appends a `usage` entry. The entry holds a `uuid`, the request's `model` ID and `cache_ttl` (`"5m"` or `"1h"`), the four provider counts `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, and `output_tokens`, and a `timestamp`. The model and TTL let the engine price the request at the rates it ran with, even after a `/model` swap or a TTL change.

On resume, the loader collects every `usage` entry in the file, including those on abandoned branches and before compact boundaries, since the session paid for each. The engine sums their estimated cost into the session total, which `/clear` resets with the fresh session. A file without `usage` entries resumes with a cost of zero. Older binaries skip `usage` lines as unknown entries and still resume the transcript.

### Write-Error Surfacing

Write failures surface through the session handle:

- First failure populates the handle's `RecordOutcome::failure` slot so the agent loop emits a one-shot `Event::Error`.
- Archive acknowledgments include every failure, and compact acknowledgments do until persistence stops.
- Until persistence stops, the agent swaps in the summary only after the boundary reaches disk. Bare `/archive` always rolls to a fresh session only after the archive entry is written.
- Failed compact and archive operations are also removed from the retry queue, so a later flush cannot persist them.
- Once persistence stops, the file receives no further lines, so a compaction commits in memory alone, and its acknowledgment reports the stop only when no earlier write reported it. Bare `/archive` keeps failing, since its whole effect is the entry on disk. The engine reports a committed compaction's write error after `SessionCompacted`, which clears the TUI's chat, so the notice stays visible.
- Three independent sticky-once flags (`flush_failure_surfaced`, `persistence_stop_surfaced`, `actor_gone_surfaced`) keep different failure kinds from masking each other, so stopping persistence surfaces even after an earlier flush failure.

## Listing

Session listing separates file discovery from metadata parsing:

- **Scope.** `ox --list`, bare `ox -c`, and the `/resume` picker walk the current project's subdirectory, plus the working directory's subdirectory for older files, and `--all` widens to every project. A prefix lookup that matches nothing in the current project searches every project.
- **Discovery.** The walk only stats files, so bare `ox -c` parses files only until it finds the newest readable unarchived one, and a prefix lookup parses only files whose name carries a matching ID.
- **Metadata.** A parsed file yields its header, the latest `Entry::Title` and `Entry::Summary`, and the archive state. A prefix pre-filter on the `"type"` discriminator keeps message lines off the full-parse hot path.
- **Archive view.** `--list` and the `/resume` picker show unarchived sessions, or only archived ones with `--list --archived` and the picker's Ctrl+A.
- **Page budget.** Archive state is stored in each file, so a listing parses sessions in order until it finds one beyond the page, then reports only whether more sessions follow. When few sessions are archived, the archived view parses further.

Listings sort rows and render them as follows:

- Sessions without a title are shown as `(untitled)`.
- Sessions without a summary still list (`Msgs` displays `-`).
- Sort by file mtime, with the file name breaking ties.
- Under `--all`, a `Project` column shows the tildified project root, or the stored `cwd` for a header that records none.
- Titles truncated to terminal width with trailing `...`.
- Control characters in titles and project paths, line breaks included, are escaped, so a row stays on one line and cannot change the terminal's state.

## Design Decisions

1. **JSONL, append-only, one file per session.** A process crash loses at most the batch in progress, and readers can stream the file line by line.
2. **Per-project subdirectory keyed by the project root.** Listings stay scoped, `--all` opts into cross-project view, and the header's `project_root` keeps a colliding directory name from mixing projects. Older files stay where they are, since a listing reads both subdirectories.
3. **First-failure-only write-error surfacing.** One `Event::Error` per failure kind plus warn-logs gives visibility without spam.
4. **Listing scan with message pre-filter, mtime sort.** Titles and summaries can appear anywhere in the file, so the scan reads every line and the prefix check keeps message lines out of JSON parsing.
5. **Search views preserve native result rows.** The `tool_result_metadata` sidecar records a call's title and, when the tool sets one, a `result_view`, which is either the structured view or a marker that the result reads as plain text. Grep in content mode, Glob, and Edit store their structured view, so complete filenames and line numbers survive even when a path contains colons, newlines, or trailing whitespace, and stored paths retain their search-time display spelling on replay. Search notes, capped output, and unrecognized output store the text marker, so warnings and spill locations remain visible instead of parsing as rows. Errors always render their error text. Read and Tasks store no view and derive it on every render, Read from the result text and Tasks from the call's input, since storing it would repeat the file content or the task list. A result without a stored view uses the tool's own derivation. Sidecars from alpha.8 and alpha.9 stored the view as the bare type, which this shape rejects, so the loader skips those records as malformed, as the Compatibility section allows.

## Sources

- `crates/ox-core/src/engine/sessions.rs`: `adopt_session`, which sums the session cost from the resumed `usage` entries.
- `crates/ox-core/src/engine/turn.rs`: the joined writes that put a tool round's records in one batch.
- `crates/ox-core/src/message.rs`: the message and content-block types that the entry schema stores.
- `crates/ox-core/src/session/actor.rs`: the actor that owns the session state and flushes each wakeup's commands through a blocking operation.
- `crates/ox-core/src/session/chain.rs`: message chain reconstruction from the `parent_uuid` DAG.
- `crates/ox-core/src/session/display.rs`: the session ID and title formatting that listings use.
- `crates/ox-core/src/session/entry.rs`: the JSONL entry schema and its header `version`.
- `crates/ox-core/src/session/handle.rs`: `SessionHandle`, the public API that sends each write to the actor and surfaces the first write failure of each kind.
- `crates/ox-core/src/session/history.rs`: the resumed transcript, whose tool results take their views from `ToolRegistry::result_view`.
- `crates/ox-core/src/session/path.rs`: the sanitized per-project subdirectory names.
- `crates/ox-core/src/session/post_compact.rs`: the synthetic post-compact root and the origin restore on resume.
- `crates/ox-core/src/session/resolver.rs`: the `--continue` argument and session-ID prefix resolution.
- `crates/ox-core/src/session/sanitize.rs`: resume-time transcript repair.
- `crates/ox-core/src/session/saved.rs`: archiving and deleting saved sessions.
- `crates/ox-core/src/session/state.rs`: the in-memory session state the actor owns.
- `crates/ox-core/src/session/store.rs`: session file I/O and the `{epoch}-{uuid}.jsonl` file names.
- `crates/ox-core/src/session/store/listing.rs`: stat-only listing walks and metadata parsing.
- `crates/ox-core/src/session/title_generator.rs`: the background AI title generator.
- `crates/ox-core/src/tool.rs`: `ToolMetadata`, the fields the `tool_result_metadata` sidecar records.
- `crates/ox/src/session_list.rs`: the `ox --list` table.
