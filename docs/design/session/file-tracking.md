# File Change Tracking

Read-before-Edit gate, xxh64 staleness detection, persistence across session resume.

## Implementation

The `FileTracker` is shared across a session's tool calls. Read populates its mutex-protected map, keyed by the canonical path so every spelling of one file, symlinks included, shares an entry. Edit and Write enforce the Read-before-Edit gate and verify the current bytes against the stored hash before mutation. Each write, including parent-directory creation, and its tracker record run in one tracked task. The engine stops tool admission and waits for those tasks before reporting cancellation, clearing or restoring tracker state, changing sessions, or capturing final snapshots. Tracker state persists to JSONL on session finish and verifies by content hash on resume.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write refuse a file the current session has not Read. Soft warnings the model can ignore defeat the purpose.

2. **Content hash is the gate.** Mutating tools always rehash the current bytes before writing. The tracker keeps only the hash, since same-size writes can preserve timestamps and only the content proves a file is unchanged.

3. **Persist the tracker on session finish, verify on resume.** `Entry::FileSnapshot` stores the tracker state in JSONL, and each snapshot names the tool call that recorded it. On resume, a snapshot is discarded when its call has no result left in the repaired transcript, since the model cannot see that content. Of the rest, the newest snapshot per path is selected, regular files at or below the tracked-file size cap are rehashed, and the survivors seed the session's new tracker. The engine restores them for every resume, at startup and on `/resume`, and reports the paths that fail the rehash in a notice when it starts on the session, so every frontend tells the user to re-Read them before mutating.

4. **Per-session scope.** The tracker starts and ends with its session, and no other process shares it. The engine builds a new tracker whenever it starts on a session, at startup, on `/clear`, and on `/resume`. Pending writes finish before that session's snapshots are captured and before the next session starts.

5. **Edit needs any Read, and Write needs every line.** The gate reflects how each tool uses the file:

   - Edit anchors on an exact `old_string`, and the content hash already rules out staleness, so any Read of the current content clears it, as in Claude Code. The state it records credits the new content lines the replacement wrote, or the whole content when the tracked view already covered every line, so a later Write still needs the rest read.
   - Write replaces the whole file, so it needs every line of the current content read. Coverage is the union of the line windows shown under one content hash, so paging through a long file in several Reads counts, and changed content starts over.
   - Read emits complete rows within its output budget and reports the lines it showed, so omitted content never counts as full. A line larger than the entire budget returns an explicit error when requested and cannot earn full coverage through line pagination.
   - A full re-Read of unchanged content returns a cache stub that names the earlier Read only when a Read recorded the state. A Write or Edit supplies content through its tool input, so deduplicating a Read against it would point the model at pre-edit text.

6. **`Arc<Mutex<HashMap>>` for tracker state.** The tracker mutates on every Read / Write / Edit. Routing each update through an actor message would force ten-plus round-trips per turn, while lock contention on a small struct with no I/O costs microseconds. The resume rehash reads files, so it runs while the new tracker is built, before any tool can take its lock.

7. **xxh64 for change detection.** Cryptographic integrity isn't required, since the tracker only needs to spot drift, and xxh64 is already used elsewhere in the crate.

8. **No tracker-managed file lock.** One agent runs its edit-class calls one at a time, and only read-only calls share a batch. Edit and Write retain the gated bytes and recheck the original identity and metadata, followed by the prepared bytes and metadata, before atomic replacement. An external writer can still change either file during those checks or before publication. Subagents would add concurrent writers within one session ([#136](https://github.com/hakula139/oxide-code/issues/136)).

## Sources

- `crates/ox-core/src/engine/sessions.rs`: `adopt_session`, which resumes a saved session into the live transcript, and `report_drift`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::load` and `LiveTranscript::adopt`, which restore the tracker.
- `crates/ox-core/src/file_tracker.rs`: `FileTracker`, `FileState`, `LastView`, `FileSnapshot`, `record_read` and `write_and_record`, which attribute each record to the call ID they take, content verification, persist / restore.
- `crates/ox-core/src/session/entry.rs`: `Entry::FileSnapshot`.
- `crates/ox-core/src/session/sanitize.rs`: `retain_visible_file_snapshots`, the resume-time provenance filter.
- `crates/ox-core/src/tool/edit.rs`: Read-before-Edit gate, current-content verification.
- `crates/ox-core/src/tool/file.rs`: `tracker_key`, `write_file`, which resolves the tracker key before publication and returns the committed bytes directly to recording. Its `commit` module owns private preparation, metadata preservation, final validation, and publication.
- `crates/ox-core/src/tool/read.rs`: `FileTracker::record_read`, cache-hit stub.
- `crates/ox-core/src/tool/write.rs`: Read-before-Write gate.
- `crates/ox-core/src/util/fs.rs`: bounded regular-file reads through no-follow handles, shared by tools and resume verification.
