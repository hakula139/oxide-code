# File Edits

`edit` and `write` modify a file only while the session knows its current content, from a Read or its own earlier write, replacing content in place while preserving metadata, hard links, and ownership. A session-scoped tracker records what each Read showed and what each write left, existing files are rewritten through the checked handle, and new files are created with an exclusive open. [Permissions](permissions.md) covers path resolution and approval gating before these checks run.

## Tracking

A session's tool calls share one `FileTracker` (`Arc<Mutex<HashMap>>`) keyed by canonical path so every spelling of a file, symlinks included, shares an entry. Read records the xxh64 content hash and displayed line windows, and each write records the hash of the bytes it wrote. Edit and Write refuse untracked files and verify the current bytes against the stored hash before writing (**Strict Read-before-Edit gate**, **Content hash is the gate**).

Each write, including parent-directory creation, and its tracker update run in one tracked task. The engine stops tool admission and waits for those tasks before reporting a cancellation, clearing or restoring tracker state, switching sessions, or capturing final snapshots. On session finish, tracker state persists to `sessions.file_snapshots` only while `head_id` still equals the head this process last committed or loaded, and resume verifies survivors by content hash (**Persist the tracker on session finish, verify on resume**).

## Checks Before Writing

Tools open the path the permission gate resolved with no-follow and non-blocking flags (refusing a final-component symlink and never blocking on a swapped-in FIFO), accepting only regular files within the 10 MiB cap. Edit and overwrite open the file read-only for the Read-before-Edit hash check, so refused calls never open a write descriptor.

Before truncating, `commit` reopens the path for writing, confirms it still refers to the file the gate read, and rereads it. A replaced file reports `the file was replaced while preparing the write`, and modified content reports `the file changed while preparing the write. Read it again`. After writing, it reopens the path to confirm identity and new bytes so the tool never reports success for overwritten content, leaving only the window between reread and write exposed to concurrent writers. Files with Unix set-ID bits or Linux capabilities are refused because a write can clear either silently.

## Publication

An existing file is truncated, rewritten through the write handle, and synced on every platform (**Write every existing file in place**), preserving Unix inode metadata (owner, group, mode, ACLs, extended attributes, flags, hard links) and Windows metadata (owner, DACL, attributes, and alternate streams, while NTFS compression follows native write behavior).

A new file uses an exclusive open (`O_EXCL` on Unix, `CREATE_NEW` on Windows), which never replaces an existing entry or dangling link and inherits default directory permissions and ACLs, then writes and syncs. If a write or sync fails after truncation or creation, the error warns that a partial file remains and tells the model to read it before writing again.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write refuse files whose current content the session has neither read nor written, since soft warnings can be ignored.
2. **Content hash is the gate.** Mutating tools always rehash current bytes because same-size writes can preserve timestamps.
3. **Edit needs any Read, and Write needs every line.** Edit anchors on an exact `old_string` with hash verification, so any Read of the current content clears it, as in Claude Code. The updated state credits the replacement lines, or the whole file if previously fully covered, so a later Write still requires reading unread regions. Write replaces the whole file and requires every line covered by the union of Read windows under the current hash (changed content resets coverage). Read emits complete rows within its budget so omitted content never counts as full, returning an explicit error for a line larger than the budget. A full re-Read of unchanged content returns a cache stub naming the earlier Read only when a Read recorded the state, because Write and Edit supply text in tool input and deduplicating against them would point at pre-edit text.
4. **Persist the tracker on session finish, verify on resume.** Each snapshot in `file_snapshots` names the recording tool call. On resume, snapshots whose call has no result in the repaired transcript are dropped, since the model cannot see that content. Among survivors, the newest per path is kept, regular files within the size cap are rehashed, valid entries seed the new tracker, and paths failing rehash trigger a drift notice on both startup and `/resume`.
5. **Per-session scope.** Each session builds a fresh tracker at startup, `/clear`, and `/resume`, shared with no other process, and pending writes drain before snapshot capture or session switch.
6. **`Arc<Mutex<HashMap>>` for tracker state.** Tracker updates happen on every Read, Write, and Edit without I/O, so a mutex avoids ten-plus actor round-trips per turn. Resume rehashing performs file I/O while building the new tracker, before any tool can lock it.
7. **xxh64 for change detection.** Change detection needs no cryptographic hash, and xxh64 is already in the crate.
8. **No tracker-managed file lock.** One agent serializes edit-class calls and batches only reads, and the pre-truncate reopen narrows external races to the window between reread and write. Subagents would add concurrent in-session writers ([#136](https://github.com/hakulab/oxide-code/issues/136)).
9. **Write every existing file in place.** Rewriting the existing inode preserves all platform metadata with no platform code, whereas a staged copy and rename would have to replicate owner, group, mode, ACLs, extended attributes, SELinux labels, macOS flags, and Windows streams or fall back anyway. The tradeoff is a single-write window where a crash, full disk, or kill can leave partial content, watchers see truncation, background `bash` readers continue from their byte offset in the rewritten file, and readers mapped past the new end fault with `SIGBUS`. Neither the tracker nor tools require atomicity. [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono), and [oh-my-pi](https://github.com/can1357/oh-my-pi) also write in place, while Claude Code renames a staged copy after `chmod` and falls back to writing in place.
10. **Gate on a read-only handle, write on the same file.** Keeping refused calls read-only prevents closing a write descriptor (which Linux watchers like `entr` treat as a save). Verifying file identity on reopen avoids writing into an inode renamed away to a backup and keeps all hard links pointing to one inode.
11. **New files trade crash atomicity for portability.** Exclusive open needs no rename or hard link, so it works on volumes such as exFAT where staged creation cannot publish safely. A mid-write failure can leave a partial new file visible to readers and watchers, with no prior content at risk.
12. **Hard links, set-ID files, and capability files.** A hard-linked file inside the workspace can also link outside every root, so the workspace gate never auto-allows it, though explicit rules or approval rewrite it in place for all links. Set-ID and capability files stay refused because writing can clear them silently.

## Sources

- `crates/ox-core/src/engine/sessions.rs`: `adopt_session`, which resumes a saved session into the live transcript, and `report_drift`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::load` and `LiveTranscript::adopt`, which restore the tracker.
- `crates/ox-core/src/file_tracker.rs`: `FileTracker`, `FileState`, `LastView`, `FileSnapshot`, `MAX_TRACKED_FILE_SIZE`, and `record_read` and `write_and_record`, which attribute each record to the call ID they take.
- `crates/ox-core/src/permission.rs`: `is_hard_linked`, the workspace gate's hard-link rule.
- `crates/ox-core/src/session/sanitize.rs`: `retain_visible_file_snapshots`, the resume-time provenance filter.
- `crates/ox-core/src/session/store/commit.rs`: `commit`, which writes the snapshots only while the head is the process's own.
- `crates/ox-core/src/tool/edit.rs`: the Read-before-Edit gate and current-content verification.
- `crates/ox-core/src/tool/file.rs`: `tracker_key`, `open_gated`, which runs the Read-before-Edit gate on the opened file and phrases each refusal, and `rewrite_file` and `create_file`, which return the committed bytes to recording under their tracker key.
- `crates/ox-core/src/tool/file/commit.rs`: `Checked::open`, `Checked::rewrite`, and `create`.
- `crates/ox-core/src/tool/read.rs`: `FileTracker::record_read` and the cache-hit stub.
- `crates/ox-core/src/tool/write.rs`: the Read-before-Write gate.
- `crates/ox-core/src/util/fs.rs`: `read_regular_file`, the bounded no-follow read that tools and resume verification share, `open_no_follow`, `read_regular_contents`, and `file_id`, which the checked open and the publication checks use, and `validate_regular_file`, the type and size check the tools run before an open.
