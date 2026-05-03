# Session Persistence

Research findings for oxide-code session persistence, based on analysis of reference projects ([Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex)), POSIX append-only semantics, and Anthropic Messages API ordering requirements.

## Reference Implementations

### Claude Code (TypeScript)

- JSONL at `~/.claude/projects/{SANITIZED_PATH}/{UUID}.jsonl` — project-scoped.
- 18+ entry types (messages, tags, titles, agents, PR links, context compression, file history, attribution, worktree state).
- Head / tail extraction for listing. 32 concurrent reads, pagination.
- Async write queue batched at 100 ms (`FLUSH_INTERVAL_MS`).
- `parentUuid` chain for conversation forking (`--fork-session`).
- File permissions `0o600`, parent dir `0o700`.

### OpenAI Codex (Rust)

- Date-hierarchical paths: `~/.codex/sessions/YYYY/MM/DD/rollout-{TIMESTAMP}-{UUID}.jsonl`.
- SQLite index for fast metadata queries.
- `RolloutItem` replay model.
- Ephemeral flag to skip persistence for testing.
- Bounded `mpsc` channel + `tokio::spawn`-ed `RolloutWriterTask`. Cmds: `AddItems`, `Persist`, `Flush`, `Shutdown` — each barrier carries an `oneshot::Sender<io::Result<()>>` ack. Terminal task failure is read post-mortem from a `Mutex<Option<Arc<IoError>>>` slot on the recorder.
- Receive-and-drain inside the writer loop — `recv().await` for the first cmd, then `try_recv()` non-blocking to coalesce queued cmds into a single batch flush. No interval timer (Rust + mpsc subsumes claude-code's `FLUSH_INTERVAL_MS = 100` JS-event-loop workaround).

## Storage Format

Every session is a single `.jsonl` file — one JSON object per line. Append-only (crash-safe with write-then-flush), streamable (incremental read / write), and universal (used by all reference implementations).

### Entry Types

Each line is a discriminated union with a `type` field:

| Type      | Position     | Purpose                                                                                 |
| --------- | ------------ | --------------------------------------------------------------------------------------- |
| `header`  | First line   | Session metadata: ID, CWD, model, created timestamp, format `version`                   |
| `message` | Middle lines | Conversation message with `uuid`, optional `parent_uuid`, timestamp                     |
| `title`   | Re-appended  | Session title + source (`first_prompt` / `ai_generated` / `user_provided`). Latest wins |
| `summary` | Last line    | Exit marker: `message_count` + `updated_at`. Latest wins                                |
| `unknown` | Any          | Catch-all absorbed by `#[serde(other)]` when readers see unrecognized types             |

### Forward Compatibility

Three mechanisms keep the schema open for future features without migration:

1. **`Entry::Unknown` catch-all** — `#[serde(other)]` on a unit variant means any unrecognized `type` discriminator parses silently rather than aborting the whole file. New writers can emit additional entry types (compaction boundaries, tags, PR links, agent metadata) and old readers skip them.
2. **`parent_uuid` chain on messages** — each message carries its own UUID and a link to its predecessor. Today's resume always appends to the tail, but a future fork feature can branch from an arbitrary message without rewriting the parent file.
3. **`version` field on the header** — bumped on incompatible changes; readers refuse files with a newer version to avoid silent data loss.

### Example file

```jsonl
{"type":"header","session_id":"a1b2c3d4-e5f6-7890-abcd-1234567890ef","cwd":"/home/user/project","model":"claude-opus-4-6","created_at":"2026-04-16T12:00:00Z","version":1}
{"type":"title","title":"hello","source":"first_prompt","updated_at":"2026-04-16T12:00:01Z"}
{"type":"message","uuid":"b1...","message":{"role":"user","content":[{"type":"text","text":"hello"}]},"timestamp":"2026-04-16T12:00:01Z"}
{"type":"message","uuid":"c2...","parent_uuid":"b1...","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-04-16T12:00:02Z"}
{"type":"summary","message_count":2,"updated_at":"2026-04-16T12:00:02Z"}
```

## Storage Layout

Sessions live in `$XDG_DATA_HOME/ox/sessions/{project}/`, falling back to `~/.local/share/ox/sessions/{project}/`. `{project}` is a filesystem-safe subdirectory name derived from the working directory at session creation time — path separators and reserved characters become `-`, and very long paths are truncated with an inline xxh64 hash suffix of the original path bytes so two long paths sharing a prefix cannot collide.

File naming: `{unix_timestamp}-{session_id}.jsonl`. Session ID is still a UUID v4; the timestamp prefix gives chronological directory-order listings without fighting the `--list` mtime sort. Lookup by session ID is by suffix (`-{id}.jsonl`), so the prefix stays invisible to callers.

Two migrations run once at store open time and are idempotent:

1. **Flat → project-scoped** — any legacy `{uuid}.jsonl` file at the top of the sessions dir is moved into its project subdir based on the header's `cwd`, picking up the new timestamp prefix in the same pass.
2. **Unprefixed → prefixed** — any remaining `{id}.jsonl` inside a project subdir is renamed to `{epoch}-{id}.jsonl` by reading the header.

On Unix, session files are created with mode `0o600` (user-only read / write) so conversation logs — which may contain verbatim tool output and secrets from bash commands — are not world-readable on multi-user systems.

## Session Lifecycle

### Actor + batched writes

The on-disk file is owned by a single `tokio::spawn`-ed actor task; the rest of the program holds a cheap-to-clone `SessionHandle` that forwards every operation as a `SessionCmd` over a bounded mpsc channel. Each cmd carries a oneshot ack so the caller awaits state integration without holding any lock across `await`. The actor's loop is `recv().await` for the first cmd, then `try_recv()` until empty, then one buffered flush over the absorbed batch. Isolated writes flush immediately because the drain returns `Empty` after the first cmd. No interval timer — Rust's mpsc absorbs claude-code's `FLUSH_INTERVAL_MS = 100` JS-event-loop workaround. The `SessionWriter` wraps a `BufWriter<File>` so the per-batch flush actually coalesces syscalls; `std::fs::File::flush` is a no-op.

For receive-and-drain to fire, the producer side has to queue multiple cmds before the actor's outer `recv` wakes. `agent_turn` does this for every tool round: the assistant message, the tool-result message, and the sidecar metadata batch ride a single `tokio::join!`. The polling order ensures each future reaches its `cmd_tx.send` before any reaches its ack-await, so all three cmds land in the channel before the actor processes the first; the drain absorbs them in one pass and flushes once. Awaiting each cmd serially would mean each ack returns only after that cmd's flush — the channel would be empty between cmds and the drain would degenerate into one flush per cmd. Sidecar metadata for one tool round travels as a single `SessionCmd::ToolMetadata { items: Vec<(String, ToolMetadata)> }` for the same reason: collapsing N sidecar cmds into one cmd that produces N entries is the cheapest way to keep them in the same batch even if cross-cmd queuing slips.

The text-only branch of `agent_turn` (no tool calls) records its single assistant message via the sequential `record_message().await` path — there's nothing to coalesce with, and one cmd through the actor is one buffered flush either way.

A side effect of joining the writes after the tools finish: the on-disk session file is iteration-atomic. A crash mid-tool leaves the file at the previous turn's tail rather than at a half-written tool round whose orphan tool_use the resume sanitizer would otherwise drop.

`SessionHandle` retains the actor's `JoinHandle` in an `Arc<Mutex<Option<JoinHandle<()>>>>` slot. `SessionHandle::shutdown(self)` sends `SessionCmd::Shutdown { ack }` so the actor flushes pending writes, acks, then breaks the loop — the first caller drains the join slot, subsequent calls (on other clones) no-op. Cmd-driven exit (rather than waiting for every `mpsc::Sender` clone to drop) keeps process-exit fast even when an orphaned clone — most importantly the detached title-generator task — is mid-HTTP and far from dropping its handle. The Anthropic client has no whole-request timeout, so a wait-for-clones-drop shape would block shutdown on whichever read / connect timeout the orphan is racing. Production exit paths (TUI, REPL, headless) call `shutdown().await` after `finish().await`. A blocking `Drop` impl is intentionally omitted: `Drop` is sync and tokio offers no portable way to await inside it without runtime hacks (`block_in_place` requires multi-thread runtime). The async-consume shape gives the same observable guarantee for callers who can `await`.

### Writer recovery on flush error

`std::io::BufWriter` is undefined after a partial-write error, so a transient mid-batch I/O failure could otherwise poison the next batch's flush with stale buffered bytes. The actor's writer state is therefore a three-variant `WriterStatus`:

- **`Pending { header }`** — file not yet on disk. First batch flush calls `SessionStore::create` to materialize, transitioning to `Active`. A header-write failure leaves `Pending` intact so the next batch retries.
- **`Active(SessionWriter)`** — steady state. A flush error drops the writer and flips to `Broken`.
- **`Broken`** — last batch errored. Next batch reopens the existing file via `SessionStore::open_append` and transitions back to `Active`. An open failure stays `Broken` and retries again on the next batch.

Implemented as a `take/restore` helper that pulls the writer out via `mem::replace` — on flush success we restore `Active(writer)`, on flush failure we transition to `Broken`. No `unreachable!()` arms.

### Lazy materialization

Starting a session allocates the session ID and stages the header in memory; the on-disk file is created by the first batch flush carrying real content. A session that exits before any message is recorded therefore leaves no artifact behind, keeping `ox --list` clear of empty `ox`-then-quit rows. The header's `created_at` is captured at start and persisted unchanged when the file finally materializes.

### Resume and sanitization

`ox -c` reopens the existing session file in append mode and spawns the actor in `WriterStatus::Active` from the start. The loader walks all `Entry::Message` lines into a UUID-indexed DAG, identifies leaves as UUIDs unreferenced by any other message's `parent_uuid`, picks the newest-timestamped leaf as the tip, and walks back via `parent_uuid` to produce a linear chain. The single load pass also surfaces the newest `Entry::Title` (max `updated_at`, so AI-generated titles supersede first-prompt ones) and hands it to the TUI status bar.

A session can crash mid-turn, leaving invariants the API rejects: unresolved `tool_use`, orphan `tool_result`, trailing `thinking` blocks, or consecutive same-role messages. The resume entry point runs a symmetric sanitization pipeline — strip trailing thinking, drop unresolved tool_use / orphan tool_result blocks, merge adjacent same-role survivors, and inject head / tail sentinels when the transcript begins assistant-first or ends on an orphan-only user turn. See the rustdoc on `sanitize_resumed_messages` for the full pass ordering.

### Fork-on-conflict concurrency

There is no file-level lock. Two processes resuming the same session both get append handles immediately; each reads the current contents, computes the tip, and appends with `parent_uuid` pointing at what it saw. When both peers appended between read and write, the new messages form a fork — two branches share an ancestor, both are leaves. The next load's newest-leaf walk picks the winner; the losing branch stays in the file for audit but is invisible to subsequent API calls.

Writes smaller than `PIPE_BUF` (typically 4 KiB) are atomic under POSIX `O_APPEND`, so short text messages never interleave. Larger writes (bash tool results dumping several KB) can interleave with a peer's write and produce malformed JSONL lines; the loader warn-skips any line that fails UTF-8 decoding or JSON parsing.

The session ID flows through to the `x-claude-code-session-id` API header for the full lifetime of the file.

### AI-generated titles

The first user prompt of a fresh session seeds a detached tokio task that calls `claude-haiku-4-5` via `Client::complete` with a 3-7 word sentence-case prompt, appends an `Entry::Title { source: AiGenerated, updated_at: now }`, and pushes `AgentEvent::SessionTitleUpdated(String)` through the sink so the TUI status bar refreshes live. Trigger is one-shot per fresh session — resumed sessions inherit whatever title the original run managed to write. Failures (network hiccup, rate-limit, Haiku returns non-JSON) warn-log only; the first-prompt title stays on disk and in the UI. AI title generation runs in TUI mode only.

The Haiku call ships a `{"title": string}` JSON schema via the `structured-outputs-2025-12-15` beta (`output_config.format` on the body), matching Claude Code's `sessionTitle.ts`. Prompt-only instruction is insufficient — for first messages phrased as direct questions ("see what's next to do in this repo"), Haiku reliably answers the question instead of titling it, and the response then fails `parse_title`. The schema forces the envelope shape regardless of how the prompt scans. Models outside the upstream structured-outputs allowlist (Opus 4 / Sonnet 4 / Haiku 4 bases, or unknown future families) fall back to free-form text and take whatever the post-fence JSON parse produces.

### Write-error surfacing

Session I/O runs alongside the agent loop but must not abort it — the user's turn should not fail because the disk is full. The actor warn-logs every failed batch flush, and the first failure populates the handle's `RecordOutcome::failure` (or `Outcome::failure`) slot so the agent loop can emit a one-shot `AgentEvent::Error`. The actor still warn-logs every subsequent failure, so the file log under `$XDG_STATE_HOME` retains the full history.

The handle's `SharedState` carries two independent sticky-once flags so qualitatively different failures don't mask each other:

- **`flush_failure_surfaced`** — flips on the first batch-flush error surfaced through a caller's ack.
- **`actor_gone_surfaced`** — flips on the first send / recv error indicating the actor task has stopped (panic, channel closed). When the actor died after recording an I/O error, the surfaced message includes that underlying cause: "Session writer task has stopped after I/O error: <...>" — read from the `last_flush_failure` slot the actor populates on every batch failure.

Sharing a single flag would let the milder per-batch failure mask the more severe actor-gone signal; keeping them independent ensures both fire exactly once.

## Listing

`ox --list` walks the current project subdirectory by default; `--all` / `-a` widens the scope to every project. For each file the scanner reads the header (line 1), then streams the rest of the file tracking the latest `Entry::Title` (by `updated_at`) and `Entry::Summary`. A cheap prefix pre-filter on the `"type"` discriminator keeps message lines off the full-parse hot path so listing stays effectively I/O-bound even on multi-megabyte transcripts. A head-plus-fixed-tail scan is faster on paper but drops AI-generated titles when the first user turn produces a large tool_result that pushes the mid-file title out of any bounded tail window.

- Head scan catches the first-prompt title; tail scan overrides with any later AI-generated / user-provided title — the newer `updated_at` wins.
- Sessions without a title show `(untitled)` (first recorded message was not a user text turn, e.g., a tool-result-only continuation).
- Sessions without a summary still list — they were interrupted before clean exit, so `Msgs` displays `-`.
- Sort is by file mtime (session_id tiebreak), so resumed sessions bubble back to the top.
- Under `--all`, a `Project` column shows the tildified stored `cwd` so cross-project rows stay distinguishable. The column is sized to the widest row, clamped to `[8, 40]` chars.
- Titles are truncated to fit the detected terminal width (`crossterm::terminal::size`) with a trailing `...`. Piped output (`ox -l | less`) and undetectable widths skip truncation so downstream tools see the full text.

## Design Decisions for oxide-code

The following decisions shaped the implementation:

1. **JSONL, append-only, one file per session** — proven format, no dependencies, crash-safe with write-then-flush. Matches every reference project.
2. **Per-project subdirectory under `$XDG_DATA_HOME/ox/sessions/`** — listings stay scoped to the project you're working in; `--all` opts into the cross-project view.
3. **`{epoch}-{uuid}.jsonl` naming** — timestamp prefix keeps `ls` chronological; lookup by UUID suffix stays O(readdir); zero collisions across machines.
4. **Forward-compat entry schema** — tagged union with `#[serde(other)]` catch-all, `parent_uuid` DAG, `version` field. New entry types land additively; older readers survive.
5. **Lazy file creation** — `start` is in-memory only; the file materializes on the first `record_message`. Empty `ox`-then-quit sessions leave no artifact.
6. **Resume + symmetric sanitization** — reopen existing file, walk the UUID DAG to pick the newest leaf, run the sanitization pipeline (strip trailing thinking, drop orphan tool_use / tool_result, merge same-role, inject head / tail sentinels). Keeps every resumed transcript API-valid without user intervention.
7. **Fork-on-conflict concurrency, no file lock** — two resumers both get append handles; loader's newest-leaf rule resolves forks deterministically. Small writes are atomic under POSIX `O_APPEND`; large interleaved writes are warn-skipped on read.
8. **Fire-and-forget AI titles** — one-shot Haiku call on fresh sessions, JSON envelope prompt, warn-log on failure. The tail-scan's newer `updated_at` lets the AI title supersede the first-prompt fallback automatically on listings and resumes.
9. **First-failure-only write-error surfacing** — balances visibility (the user learns persistence broke) against spam (no re-report on every write) via a single `AgentEvent::Error` plus warn-logs.
10. **Head + tail listing scan, mtime sort** — head catches the first-prompt title (line 2, never re-appended); tail picks up later titles and the exit summary; mtime sort reflects "last used" for free.
11. **Actor-owned writes, receive-and-drain batching** — one `tokio::spawn`-ed task owns `SessionState` and the `BufWriter<File>`-wrapped writer; every consumer holds a clone of the cheap `SessionHandle`. `agent_turn` queues a tool round's three writes through one `tokio::join!` so the actor's drain coalesces them into a single buffered flush; isolated writes (text-only turns, AI title append, finish) still flush immediately. Removes the `tokio::sync::Mutex<SessionManager>` shape entirely, drops a tool round's syscall count from one-per-cmd to one-per-turn, and gives a clean extension surface (compaction, fork, interactive prompts) — each new operation is a new `SessionCmd` variant. **Compression** still lands as a new entry type via the `Unknown` catch-all, no migration required.
