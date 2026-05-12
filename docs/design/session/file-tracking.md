# File Change Tracking

Read-before-Edit gate, xxh64 staleness detection, persistence across session resume.

## Implementation

The `FileTracker` is a per-session `Arc<Mutex<HashMap>>` shared across tool calls. Read populates the tracker. Edit and Write enforce the Read-before-Edit gate and rehash the current bytes before mutation. Tracker state persists to JSONL on session finish and verifies by content hash on resume.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write refuse if the file has not been fully Read in this session. Soft warnings the model can ignore defeat the purpose.

2. **Content hash is the gate.** Mutating tools always rehash the current bytes before writing. `(mtime, size)` is useful for metadata and diagnostics, but it is not a correctness proof because same-size writes can preserve timestamps.

3. **Persist the tracker on session finish, verify on resume.** `Entry::FileSnapshot` stores the tracker state in JSONL. On resume each snapshot is rehashed. Survivors restore into the in-memory tracker, while mismatches drop silently.

4. **Per-session scope.** Tracker created with the session, dropped on finish. No cross-process sharing.

5. **Partial-view Reads do not satisfy the gate.** A ranged Read populates `LastView::Partial { offset, limit }`. Edit / Write against a partial-view path fires the "must read fully first" error.

6. **`Arc<Mutex<HashMap>>` instead of an actor channel.** The tracker mutates on every Read / Write / Edit, and an actor-message-per-update path would force ten-plus round-trips per turn, while lock contention on a small struct with no I/O is microseconds.

7. **xxh64 for change detection.** Cryptographic integrity isn't required, since the tracker only needs to spot drift, and xxh64 is already used elsewhere in the crate.

8. **No tracker-managed file lock.** Single-agent today, so the tracker handles the "external editor changed the file" case directly. Add the `Semaphore` when multi-agent or parallel-tool-execution lands.

## Sources

- `crates/oxide-code/src/file_tracker.rs`: `FileTracker`, `LastView`, `FileSnapshot`, staleness checks, persist / restore.
- `crates/oxide-code/src/session/entry.rs`: `Entry::FileSnapshot`.
- `crates/oxide-code/src/tool/edit.rs`: Read-before-Edit gate, staleness check.
- `crates/oxide-code/src/tool/read.rs`: `FileTracker::record_read`, cache-hit stub.
- `crates/oxide-code/src/tool/write.rs`: Read-before-Write gate.
