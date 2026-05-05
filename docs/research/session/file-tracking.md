# File Change Tracking (Reference)

Research on stale-file edit prevention across reference codebases. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Claude Code (TypeScript)

Explicit `FileStateCache` (LRU, 100 entries x 25 MB) per session.

Per-file state: `content`, `timestamp` (mtime ms), `offset`, `limit`, `isPartialView`.

Invariants (`FileEditTool.ts:275-311`):

1. Edit refuses if no entry exists or entry is `isPartialView: true`.
2. Edit refuses if `mtime > readTimestamp.timestamp`. For full reads, disk content is re-compared; matching bytes allow the Edit through (Windows cloud-sync workaround). Partial reads always reject on mtime drift.
3. Read on cached unchanged file returns a stub instead of repeating bytes.

Resume: rehydrate cache by scanning message history (`queryHelpers.ts:346-501`).

## OpenAI Codex (Rust)

No explicit cache. Validation deferred to `apply_patch` verification layer (`apply_patch.rs:340-464`). Patch context lines must match current file content; mismatches return `CorrectnessError` to the model. No "must Read first" rule.

## opencode (TypeScript)

Per-file `Semaphore` lock (`edit.ts:36-46`). No content / timestamp cache. Edit reads disk on every call and applies string replacement against fresh bytes. Stale `oldString` triggers "string not found" error.

## Comparison

| Repo        | Tracker            | Read-before-Edit | Stale check              | Resume                 | Concurrency                 |
| ----------- | ------------------ | ---------------- | ------------------------ | ---------------------- | --------------------------- |
| Claude Code | LRU per session    | strict           | mtime + content fallback | rehydrate from history | LRUCache (single-thread JS) |
| Codex       | none               | none             | apply-time patch context | implicit (rollout)     | `Mutex<SessionState>`       |
| opencode    | per-file semaphore | none             | string-match failure     | n/a                    | per-file `Semaphore`        |
| oxide-code  | per-session hash   | strict           | mtime + xxh64 fallback   | persist + verify       | `Arc<Mutex<HashMap>>`       |
