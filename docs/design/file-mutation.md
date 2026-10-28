# File Mutation

`edit` and `write` replace a file's content while keeping its metadata, hard links, and ownership. Every platform rewrites an existing file in place through the handle that was checked, and creates a new file with an exclusive open.

Companion docs: [Permissions](permissions.md), [File Tracking](file-tracking.md).

## Checks Before Writing

The path gate resolves the target and the tools open it with `open_no_follow`, which refuses a symlink at the final component, with `O_NONBLOCK` so a swapped-in FIFO cannot hang the open. `validate_regular_file` requires a regular file at or below `MAX_TRACKED_FILE_SIZE` (10 MiB), and the tracker's Read-before-Edit gate supplies the bytes the tool expects to replace.

`commit::write` compares the current content with those bytes before changing anything, and `Original::verify` repeats the identity and byte checks immediately before publication. A changed file reports "the file changed while preparing the write. Read it again." and a replaced inode reports "the file was replaced while preparing the write", so the tool never records success for content another writer already replaced. A writer with access can still change the file between that last check and publication. A file with set-ID permissions on Unix, or with a capability on Linux, is refused, because a write can clear either while the tool would still report success.

## Publication

### In-Place Write

`write_in_place` rewrites through the original's held handle with `set_len(0)`, `write_all`, and `sync_all`, on every platform. On Unix the inode keeps its owner, group, mode, ACLs, extended attributes, flags, and every hard link without recreating any of them. On Windows the file keeps its owner, DACL, native attributes, and alternate streams, while content-dependent NTFS compression follows native write behavior. A final `Original::verify` confirms that the path still holds the written bytes.

### New Files

`create` opens the path with `create_new`, then writes and syncs the file. The exclusive open (`O_EXCL` on Unix, `CREATE_NEW` on Windows) never replaces an existing entry, including a dangling link, and the kernel gives the file the mode and default ACL a plain creation in that directory receives. When the write or sync fails, the error says a partial file remains and asks the model to read it before writing again.

## Design Decisions

1. **Write every existing file in place.** The inode already carries its metadata, so a rewrite keeps all of it with no platform code, while a staged copy and a rename would have to reproduce owner, group, mode, ACLs, extended attributes, SELinux labels, macOS flags, and Windows streams, or fall back to an in-place write for any file whose metadata they miss. In exchange, a crash, a full disk, or a kill mid-write can leave partial content, and a file watcher can see the truncated file. The window lasts one write of the new content, and neither the tracker nor the tools depend on atomicity. Codex and pi also write in place, while Claude Code renames a staged copy after `chmod` and falls back to an in-place write ([Claude Code](https://github.com/hakula139/claude-code), [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono)).
2. **Write through the held handle, not a reopened path.** The handle was opened without following a symlink and checked against the tracked bytes, so rewriting through it cannot land on a different file, and every hard link keeps seeing one inode.
3. **New files trade crash atomicity for portability.** An exclusive open needs no rename or hard link, so it works on volumes such as exFAT, which support neither a no-replace rename nor hard links and on which a staged copy could not be published at all. A crash, a full disk, or a kill mid-write can leave a partial new file, and a reader or file watcher can see the file while it is written, but no earlier content exists to lose.
4. **Hard links, set-ID files, and capability files.** A file with several links can sit inside the workspace while another link sits outside every root, so the workspace gate never auto-allows it, but an explicit rule or approval writes it in place and every link sees the new content. Set-ID files and files carrying a capability stay refused because a write can clear either and the tool cannot report what the file system did with them.

## Sources

- `crates/ox-core/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`, expected bytes from the Read-before-Edit gate.
- `crates/ox-core/src/permission.rs`: `is_hard_linked`, the workspace gate's hard-link rule.
- `crates/ox-core/src/tool/file.rs`: `write_file`, which resolves the tracker key before publication and reports the committed bytes.
- `crates/ox-core/src/tool/file/commit.rs`: `write_with`, `write_in_place`, `create`, `Original::verify`.
- `crates/ox-core/src/util/fs.rs`: `open_no_follow`, `validate_regular_file`, `file_id`.
