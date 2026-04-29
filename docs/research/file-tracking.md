# File Change Tracking

Research notes on how reference codebases prevent stale-file edits and skip redundant Reads. The shared problem: a model that Read a file at turn T and Edits it at turn T+5 may be reasoning against bytes the user overwrote in their own editor at turn T+3. Without tracking, the Edit silently clobbers the user's change. Each codebase encodes a different contract — strict gate, deferred validation, or per-file lock — and each handles the resume-state question differently. Based on analysis of [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Reference Implementations

### Claude Code (TypeScript)

Explicit `FileStateCache` (LRU, 100 entries × 25 MB) per session.

**Per-file state:**

```typescript
type FileState = {
  content: string                      // Raw disk bytes
  timestamp: number                    // mtime in milliseconds
  offset: number | undefined           // Set on partial reads
  limit: number | undefined            // Set on partial reads
  isPartialView?: boolean              // True if model saw a filtered slice
}
```

**Update points:**

| Event          | Cache mutation                                                   |
| -------------- | ---------------------------------------------------------------- |
| Read (full)    | Sets entry with `offset = limit = undefined`                     |
| Read (partial) | Sets entry with the actual `offset` / `limit`                    |
| Write          | Sets entry from input `content`, `offset = undefined`            |
| Edit           | Reads disk for fresh mtime, sets entry with `offset = undefined` |
| Resume         | Cache rehydrated by walking message history (see below)          |

**Invariants enforced (`FileEditTool.ts:275-311`):**

1. Edit refuses if no entry exists or the entry is `isPartialView: true`: `"File has not been read yet. Read it first before writing to it."`
2. Edit refuses if `mtime > readTimestamp.timestamp`. For full reads, the disk content is re-compared against the cached content; matching bytes allow the Edit through (Windows cloud-sync mtime-touch workaround). Partial reads always reject on mtime drift.
3. Read on a cached unchanged file returns a stub (`"File hasn't been modified since the last read. Returning already-read file."`) instead of repeating the bytes.

**Resume:** rehydrate the cache by scanning the message history (`queryHelpers.ts:346-501`). For each past `read` tool use, extract the corresponding `tool_result` content. For past `edit` uses, re-read the current disk state to capture the post-edit bytes. For past `write` uses, use the input `content`. The result is a cold-disk-state cache populated from the model's own past observations.

**Concurrency:** single-threaded JavaScript with the LRU cache as implicit serialization point. No `Mutex` / `Semaphore`.

### OpenAI Codex (Rust)

No explicit cache. File validation is deferred to the `apply_patch` verification layer (`codex-rs/core/src/tools/handlers/apply_patch.rs:340-464`).

**Validation flow:**

1. Patch is parsed via `maybe_parse_apply_patch_verified()`.
2. Patch is applied to the filesystem; the apply step verifies that the patch's context lines match the current file content.
3. Mismatches return `CorrectnessError` to the model with a clear prompt to retry with corrected context.

There is no "must Read first" rule. Code can be edited that the model has never read — the apply-time context check is the only guard. The trade-off: late failures, but the simplest possible state machine.

**Concurrency:** per-session `Mutex<SessionState>`; no per-file lock.

### opencode (TypeScript / Effect)

Per-file `Semaphore` lock (`packages/opencode/src/tool/edit.ts:36-46`) prevents concurrent edits to the same path; no content / timestamp cache.

```typescript
const locks = new Map<string, Semaphore.Semaphore>()  // module-global
```

Edit reads disk on every call (`edit.ts:115-118`) and applies the string replacement against the freshly-read bytes:

```typescript
const source = yield* Bom.readFile(afs, filePath)
const next = Bom.split(replace(contentOld, old, replacement, params.replaceAll))
```

A stale `oldString` triggers a "string not found" error — the model must adjust and retry. There is no "file has been modified" message; drift surfaces as the same error a typo would produce.

**Resume:** not applicable. opencode is single-session.

**Concurrency:** per-file `Semaphore` from `effect` library (`withPermits(1)`). Different files edit in parallel; same file serializes.

## Comparison

| Repo               | Tracker            | Read-before-Edit | Stale check              | Resume                 | Concurrency                 |
| ------------------ | ------------------ | ---------------- | ------------------------ | ---------------------- | --------------------------- |
| claude-code        | LRU per session    | strict           | mtime + content fallback | rehydrate from history | LRUCache (single-thread JS) |
| codex              | none               | none             | apply-time patch context | implicit (rollout)     | `Mutex<SessionState>`       |
| opencode           | per-file semaphore | none             | string-match failure     | n/a                    | per-file `Semaphore`        |
| oxide-code (today) | none               | none             | none                     | n/a                    | none                        |

## oxide-code Today

Read / Write / Edit tools are unit structs (`ReadTool`, `WriteTool`, `EditTool`) with no shared state. Each tool call is independent.

The only existing signal for external modification is Edit's `"old_string not found in {path}"` error — a false negative if the user happens to leave the matched substring intact while changing surrounding lines. The model would then edit the file based on stale context.

The session machinery already parses past tool_use / tool_result pairs via `crates/oxide-code/src/session/history.rs`. That gives us the building blocks for claude-code-style message-history rehydration if we want it. But the JSONL schema's `Entry::Unknown` `#[serde(other)]` catch-all (see `crates/oxide-code/src/session/entry.rs` and `docs/research/session-persistence.md` § Forward Compatibility) also makes it cheap to add a new entry type — explicit persistence is more direct than parsing message bodies and avoids coupling to sanitization shape.

## Design Decisions for oxide-code

The roadmap item is: skip re-reads when content hasn't changed, and guard against blind overwrites. Decisions that shape the planned implementation:

1. **Strict Read-before-Edit gate.** Edit and Write refuse if the file has not been **fully** Read in this session. Soft warnings the model can ignore defeat the purpose. The friction (one extra Read in flows where the model thinks it already knows the content) is cheap insurance against silent overwrites.
2. **mtime + size fast path, content-hash slow path.** The common case (file untouched) is a single `stat()`. When mtime / size differ, re-hash the file via xxh64 (already in the dep tree); if the hash matches, treat as unchanged (Windows cloud-sync false-positive workaround à la claude-code).
3. **Persist the tracker on session finish, verify on resume.** A new `Entry::FileSnapshot` variant rides the existing JSONL forward-compat — old readers absorb it as `Entry::Unknown` and skip past. On resume each snapshot is re-`stat()`-checked; survivors restore into the in-memory tracker. Mismatches (mtime / size drift, missing files) drop silently and the model re-Reads on first access. This trades one extra Read after resume (the cold-tracker alternative) for cleanly bridging session boundaries — the extra disk write is one line per tracked file, batched into the existing finish flush.
4. **Per-session scope.** Tracker created with the session, dropped on finish. No cross-process or cross-session sharing. Simpler concurrency and matches today's session-state lifecycle.
5. **Partial-view Reads do not satisfy the gate.** A ranged Read populates `LastView::Partial { offset, limit }`. Edit / Write against a partial-view path fires the "must read fully first" error — the model only saw a slice and may be reasoning about content it hasn't seen. Claude Code's same rule, justified the same way.
6. **`Arc<Mutex<HashMap>>` instead of an actor channel.** The tracker mutates on every Read / Write / Edit; an actor-message-per-update path would force ten-plus round-trips per turn. Lock contention on a small struct with no I/O is microseconds — same exception shape as `.claude/plans/session-write-batching.md`'s `SharedState` slot.
7. **xxh64, not SHA-256.** Change detection, not cryptographic integrity. Already used elsewhere in the crate (billing `cch`, project-name sanitization in `session/path.rs`).
8. **No tracker-managed file lock.** opencode's per-file `Semaphore` prevents two concurrent edits to the same file. oxide-code is single-agent today; the tracker handles the "external editor changed the file" case directly. Add the `Semaphore` when a multi-agent or parallel-tool-execution feature lands.

## Sources

### oxide-code

- `crates/oxide-code/src/session/entry.rs` — JSONL forward-compat (`Entry::Unknown` `#[serde(other)]` catch-all).
- `crates/oxide-code/src/session/history.rs` — past tool_use / tool_result pairing (alternative resume strategy: rehydrate from message history rather than persisted snapshots).
- `crates/oxide-code/src/tool/edit.rs:185-266` — read-before-replace flow (line 212 reads pre-edit content; line 256 writes post-edit).
- `crates/oxide-code/src/tool/read.rs:103-195` — file open + bytes-in-memory point where post-Read hashing would slot in.
- `crates/oxide-code/src/tool/write.rs:78-101` — `is_new` detection pattern (today's closest analog to "we touched this file").

### Reference projects

- `claude-code/src/tools/FileEditTool/FileEditTool.ts:275-311` — Edit-time gate + mtime/content fallback.
- `claude-code/src/utils/fileStateCache.ts` — LRU cache shape, eviction.
- `claude-code/src/utils/queryHelpers.ts:346-501` — resume rehydration via message history (the approach we explicitly do **not** take).
- `codex-rs/core/src/tools/handlers/apply_patch.rs:340-464` — codex's deferred apply-time validation.
- `opencode/packages/opencode/src/tool/edit.ts:36-46` — per-file `Semaphore`.
- `opencode/packages/opencode/src/tool/edit.ts:115-118` — disk re-read on every Edit.
