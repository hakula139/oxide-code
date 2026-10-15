# File Change Tracking

Read-before-Edit gate, xxh64 staleness detection, persistence across session resume.

## Implementation

The `FileTracker` is a per-session `Arc<Mutex<HashMap>>` shared across tool calls. Read populates the tracker, keyed by the canonical path so every spelling of one file, symlinks included, shares an entry. Edit and Write enforce the Read-before-Edit gate and verify the current bytes against the stored hash before mutation. Each write and its tracker record run in one spawned task, so the tracker matches the disk even when a cancel drops the call mid-write. Tracker state persists to JSONL on session finish and verifies by content hash on resume.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write refuse a file the current session has not Read. Soft warnings the model can ignore defeat the purpose.

2. **Content hash is the gate.** Mutating tools always rehash the current bytes before writing. The tracker keeps only the hash, since same-size writes can preserve timestamps and only the content proves a file is unchanged.

3. **Persist the tracker on session finish, verify on resume.** `Entry::FileSnapshot` stores the tracker state in JSONL, and each snapshot names the tool call that recorded it. On resume, a snapshot is discarded when its call has no result left in the repaired transcript, or names no call at all, since the model cannot see that content. Of the rest, the newest snapshot per path is selected, regular files at or below the tracked-file size cap are rehashed, survivors restore into the in-memory tracker, and paths that fail the rehash are surfaced so the user can re-Read before mutating.

4. **Per-session scope.** Tracker created with the session, dropped on finish. No cross-process sharing.

5. **Edit needs any Read, and Write needs every line.** The gate reflects how each tool uses the file:

   - Edit anchors on an exact `old_string`, and the content hash already rules out staleness, so any Read of the current content clears it, as in Claude Code.
   - Write replaces the whole file, so it needs every line of the current content read. Coverage is the union of the line windows shown under one content hash, so paging through a long file in several Reads counts, and changed content starts over.
   - Read stops before the dispatcher's byte cap and reports the lines it actually showed, so a truncated view never counts as full.

6. **`Arc<Mutex<HashMap>>` for tracker state.** The tracker mutates on every Read / Write / Edit. Routing each update through an actor message would force ten-plus round-trips per turn, while lock contention on a small struct with no I/O costs microseconds.

7. **xxh64 for change detection.** Cryptographic integrity isn't required, since the tracker only needs to spot drift, and xxh64 is already used elsewhere in the crate.

8. **No tracker-managed file lock.** One agent runs tool calls one at a time, so the only concurrent writer is an external editor, which the staleness check catches. Subagents would add concurrent writers within one session ([#136](https://github.com/hakula139/oxide-code/issues/136)).

## Sources

- `crates/oxide-code/src/file_tracker.rs`: `FileTracker`, `LastView`, `FileSnapshot`, `attribute_to`, `write_and_record`, content verification, persist / restore.
- `crates/oxide-code/src/session/entry.rs`: `Entry::FileSnapshot`.
- `crates/oxide-code/src/session/sanitize.rs`: `retain_visible_file_snapshots`, the resume-time provenance filter.
- `crates/oxide-code/src/tool.rs`: `tracker_key`, `write_tracked`.
- `crates/oxide-code/src/tool/edit.rs`: Read-before-Edit gate, current-content verification.
- `crates/oxide-code/src/tool/read.rs`: `FileTracker::record_read`, cache-hit stub.
- `crates/oxide-code/src/tool/write.rs`: Read-before-Write gate.
