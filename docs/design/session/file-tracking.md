# File Change Tracking

Read-before-Edit gate, mtime + xxh64 staleness detection, persistence across session resume.

## Implementation

The `FileTracker` (`crates/oxide-code/src/file_tracker.rs`) is a per-session `Arc<Mutex<HashMap>>` shared across tool calls. Read populates the tracker; Edit and Write enforce the Read-before-Edit gate and mtime + xxh64 staleness check. Tracker state persists to JSONL on session finish and verifies on resume.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write refuse if the file has not been fully Read in this session. Soft warnings the model can ignore defeat the purpose.
2. **mtime + size fast path, content-hash slow path.** Common case (file untouched) is a single `stat()`. When mtime / size differ, re-hash via xxh64; if the hash matches, treat as unchanged (Windows cloud-sync false-positive workaround).
3. **Persist the tracker on session finish, verify on resume.** A new `Entry::FileSnapshot` variant rides the existing JSONL forward-compat. On resume each snapshot is re-`stat()`-checked; survivors restore into the in-memory tracker. Mismatches drop silently.
4. **Per-session scope.** Tracker created with the session, dropped on finish. No cross-process sharing.
5. **Partial-view Reads do not satisfy the gate.** A ranged Read populates `LastView::Partial { offset, limit }`. Edit / Write against a partial-view path fires the "must read fully first" error.
6. **`Arc<Mutex<HashMap>>` instead of an actor channel.** The tracker mutates on every Read / Write / Edit; an actor-message-per-update path would force ten-plus round-trips per turn. Lock contention on a small struct with no I/O is microseconds.
7. **xxh64, not SHA-256.** Change detection, not cryptographic integrity. Already used elsewhere in the crate.
8. **No tracker-managed file lock.** Single-agent today; the tracker handles the "external editor changed the file" case directly. Add the `Semaphore` when multi-agent or parallel-tool-execution lands.

## Sources

- `crates/oxide-code/src/file_tracker.rs` -- `FileTracker`, `LastView`, `FileSnapshot`, staleness checks, persist / restore.
- `crates/oxide-code/src/session/entry.rs` -- JSONL forward-compat (`Entry::Unknown`, `Entry::FileSnapshot`).
- `crates/oxide-code/src/tool/edit.rs` -- Read-before-Edit gate, staleness check.
- `crates/oxide-code/src/tool/read.rs` -- `FileTracker::record_read`, cache-hit stub.
- `crates/oxide-code/src/tool/write.rs` -- Read-before-Write gate.
