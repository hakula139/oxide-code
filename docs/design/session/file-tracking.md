# File Change Tracking

Read-before-Edit gate, xxh64 staleness detection, persistence across session resume.

## Implementation

The `FileTracker` is a per-session `Arc<Mutex<HashMap>>` shared across tool calls. Read populates the tracker. Edit and Write enforce the Read-before-Edit gate and verify the current bytes against the stored hash before mutation. Tracker state persists to JSONL on session finish and verifies by content hash on resume.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write require a full-file Read in the current session. Soft warnings the model can ignore defeat the purpose.

2. **Content hash is the gate.** Mutating tools always rehash the current bytes before writing. `(mtime, size)` helps metadata and diagnostics, while the content hash is the correctness proof because same-size writes can preserve timestamps.

3. **Persist the tracker on session finish, verify on resume.** `Entry::FileSnapshot` stores the tracker state in JSONL. On resume the newest snapshot per path is selected, regular files at or below the tracked-file size cap are rehashed, survivors restore into the in-memory tracker, and dropped paths are surfaced so the user can re-Read before mutating.

4. **Per-session scope.** Tracker created with the session, dropped on finish. No cross-process sharing.

5. **Partial-view Reads keep the gate closed.** A ranged Read populates `LastView::Partial { offset, limit }`. Edit / Write against that path fires the "must read fully first" error.

6. **`Arc<Mutex<HashMap>>` instead of an actor channel.** The tracker mutates on every Read / Write / Edit, and an actor-message-per-update path would force ten-plus round-trips per turn, while lock contention on a small struct with no I/O is microseconds.

7. **xxh64 for change detection.** Cryptographic integrity isn't required, since the tracker only needs to spot drift, and xxh64 is already used elsewhere in the crate.

8. **No tracker-managed file lock.** Single-agent today, so the tracker handles the "external editor changed the file" case directly. Add the `Semaphore` when multi-agent or parallel-tool-execution lands.

## Sources

- `crates/oxide-code/src/file_tracker.rs`: `FileTracker`, `LastView`, `FileSnapshot`, content verification, persist / restore.
- `crates/oxide-code/src/session/entry.rs`: `Entry::FileSnapshot`.
- `crates/oxide-code/src/tool/edit.rs`: Read-before-Edit gate, current-content verification.
- `crates/oxide-code/src/tool/read.rs`: `FileTracker::record_read`, cache-hit stub.
- `crates/oxide-code/src/tool/write.rs`: Read-before-Write gate.
