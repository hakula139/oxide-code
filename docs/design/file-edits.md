# File Edits

`edit` and `write` change a file only while the session knows its current content, from a Read or its own earlier write, and they replace that content while keeping the file's metadata, hard links, and ownership. A session-scoped tracker records what each Read showed and what each write left, every existing file is rewritten in place through the handle that was checked, and a new file is created with an exclusive open.

Companion docs: [Permissions](permissions.md).

## Tracking

The `FileTracker` is shared across a session's tool calls. Read records the content hash and the lines it showed, and each write records the hash of the content it left, in a mutex-protected map, keyed by the canonical path so every spelling of one file, symlinks included, shares an entry. Edit and Write refuse a file the tracker holds no state for, and verify the current bytes against the stored hash before they change anything (**Strict Read-before-Edit gate**, **Content hash is the gate**).

Each write, including parent-directory creation, and its tracker record run in one tracked task. The engine stops tool admission and waits for those tasks before it reports a cancellation, clears or restores tracker state, changes sessions, or captures the final snapshots. The tracker state persists to the session's row when the session finishes and verifies by content hash on resume (**Persist the tracker on session finish, verify on resume**).

## Checks Before Writing

The tools open the target that the path gate resolved, refusing a symlink at its final component and never blocking on a swapped-in FIFO, and accept only a regular file within the 10 MiB tracked-file cap. An edit or an overwrite opens an existing file read-only, and the Read-before-Edit gate matches the content read through that handle against the tracked hash, so a call the gate refuses never opens the file for writing.

Just before truncating, the `commit` module reopens the path for writing, confirms that it still names the file the gate read, and rereads it. A file replaced since the gate, such as by an editor that renames the original to a backup, reports "the file was replaced while preparing the write", and a change in content reports "the file changed while preparing the write. Read it again." After the write, it reopens the path once more to confirm that the path still names that file and holds the new bytes. The tool therefore never records success for content another writer already replaced. A writer with access can still change the file between the reread and the write. A file with set-ID permissions on Unix, or with a capability on Linux, is refused, because a write can clear either while the tool would still report success.

## Publication

An existing file is truncated and rewritten through the handle opened for writing, then synced, on every platform (**Write every existing file in place**). On Unix, the inode keeps its owner, group, mode, ACLs, extended attributes, flags, and every hard link. On Windows, the file keeps its owner, DACL, native attributes, and alternate streams, while content-dependent NTFS compression follows native write behavior. A final check confirms that the path still holds the written bytes.

A new file is created with an exclusive open, then written and synced. The exclusive open (`O_EXCL` on Unix, `CREATE_NEW` on Windows) never replaces an existing entry, including a dangling link, and the kernel gives the file the mode and default ACL a plain creation in that directory receives.

When a write or sync fails after the truncation or the creation, the error says a partial file remains and asks the model to read it before writing again.

## Design Decisions

1. **Strict Read-before-Edit gate.** Edit and Write refuse a file whose current content the session has neither read nor written. Soft warnings the model can ignore defeat the purpose.

2. **Content hash is the gate.** Mutating tools always rehash the current bytes before writing. The tracker keeps only the hash, since same-size writes can preserve timestamps and only the content proves a file is unchanged.

3. **Edit needs any Read, and Write needs every line.** The gate reflects how each tool uses the file:

   - Edit anchors on an exact `old_string`, and the content hash already rules out staleness, so any Read of the current content clears it, as in Claude Code. The state it records credits the new content lines the replacement wrote, or the whole content when the tracked view already covered every line, so a later Write still needs the rest read.
   - Write replaces the whole file, so it needs every line of the current content read. Coverage is the union of the line windows shown under one content hash, so paging through a long file in several Reads counts, and changed content starts over.
   - Read emits complete rows within its output budget and reports the lines it showed, so omitted content never counts as full. A line larger than the entire budget returns an explicit error when requested and cannot earn full coverage through line pagination.
   - A full re-Read of unchanged content returns a cache stub that names the earlier Read only when a Read recorded the state. A Write or Edit supplies content through its tool input, so deduplicating a Read against it would point the model at pre-edit text.

4. **Persist the tracker on session finish, verify on resume.** The session row's `file_snapshots` column stores the tracker state, written only while the session's head is still the one the process last committed or loaded, and each snapshot names the tool call that recorded it. On resume, a snapshot is discarded when its call has no result left in the repaired transcript, since the model cannot see that content. Of the rest, the newest snapshot per path is selected, regular files at or below the size cap are rehashed, and the survivors seed the session's new tracker. The engine restores them on every resume, at startup and on `/resume`, and reports the paths that fail the rehash in a notice when it starts on the session, so every frontend tells the user to read them again before changing them.

5. **Per-session scope.** The tracker starts and ends with its session, and no other process shares it. The engine builds a new tracker whenever it starts on a session, at startup, on `/clear`, and on `/resume`, and pending writes finish before that session's snapshots are captured and before the next session starts.

6. **`Arc<Mutex<HashMap>>` for tracker state.** The tracker mutates on every Read, Write, and Edit. Routing each update through an actor message would force ten-plus round-trips per turn, while lock contention on a small struct with no I/O costs microseconds. The resume rehash reads files, so it runs while the new tracker is built, before any tool can take its lock.

7. **xxh64 for change detection.** Cryptographic integrity isn't required, since the tracker only needs to spot drift, and xxh64 is already used elsewhere in the crate.

8. **No tracker-managed file lock.** One agent runs its edit-class calls one at a time, and only read-only calls share a batch. Edit and Write reopen and reread the file just before truncating it, leaving an external writer only the window between that reread and the write. Subagents would add concurrent writers within one session ([#136](https://github.com/hakula139/oxide-code/issues/136)).

9. **Write every existing file in place.** The inode already carries its metadata, so a rewrite keeps all of it with no platform code, while a staged copy and a rename would have to reproduce owner, group, mode, ACLs, extended attributes, SELinux labels, macOS flags, and Windows streams, or fall back to an in-place write for any file whose metadata they miss. In exchange, a crash, a full disk, or a kill mid-write can leave partial content, and a file watcher can see the truncated file. A process reading the file meanwhile sees the new or truncated content on the same inode: a script running in a background `bash` shell continues from its byte offset in the rewritten file, and a reader that maps the file past its new end faults with `SIGBUS`. The window lasts one write of the new content, and neither the tracker nor the tools depend on atomicity. Codex, pi, and oh-my-pi also write in place, while Claude Code renames a staged copy after `chmod` and falls back to an in-place write ([Claude Code](https://github.com/hakula139/claude-code), [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono), [oh-my-pi](https://github.com/can1357/oh-my-pi)).

10. **Gate on a read-only handle, write on the same file.** The gate reads through a read-only handle opened without following a symlink, so a call the gate refuses never closes a write-opened descriptor, which Linux file watchers such as `entr` report as a save. The write reopens the path and requires the identity of the file the gate read before truncating, so it cannot fill an inode that was renamed away before the write reopened it, such as an editor's backup, and every hard link keeps seeing one inode.

11. **New files trade crash atomicity for portability.** An exclusive open needs no rename or hard link, so it works on volumes such as exFAT, which support neither a no-replace rename nor hard links and on which a staged copy could not be published at all. A crash, a full disk, or a kill mid-write can leave a partial new file, and a reader or file watcher can see the file while it is written, but no earlier content exists to lose.

12. **Hard links, set-ID files, and capability files.** A file with several links can sit inside the workspace while another link sits outside every root, so the workspace gate never auto-allows it, but an explicit rule or approval writes it in place and every link sees the new content. Set-ID files and files carrying a capability stay refused because a write can clear either and the tool cannot report what the file system did with them.

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
