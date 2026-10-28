# File Mutation

`edit` and `write` replace a file's content while keeping its metadata, hard links, and ownership. A staged copy publishes atomically only when the copy reproduces the original's metadata by itself, and every other file is rewritten in place through the handle that was checked.

Companion docs: [Permissions](permissions.md), [File Tracking](file-tracking.md).

## Checks Before Writing

The path gate resolves the target and the tools open it with `open_no_follow`, which refuses a symlink at the final component, with `O_NONBLOCK` so a swapped-in FIFO cannot hang the open. `validate_regular_file` requires a regular file at or below `MAX_TRACKED_FILE_SIZE` (10 MiB), and the tracker's Read-before-Edit gate supplies the bytes the tool expects to replace.

`commit::write` compares the current content with those bytes before changing anything, and `Original::verify` repeats the identity and byte checks immediately before publication. A changed file reports "the file changed while preparing the write. Read it again." and a replaced inode reports "the file was replaced while preparing the write", so the tool never records success for content another writer already replaced. On Unix, a file with set-ID permissions or a capability is refused, because a write can clear either while the tool would still report success.

## Publication

### Atomic Replacement

On Linux, `replace_atomically` stages a `0600` file in the destination's parent, writes and syncs it, gives it the original's mode, and captures a `Snapshot` of the staged file's owner, group, mode, and extended attributes, including the POSIX access ACL and the SELinux label. When the snapshot equals the original's, the staged file already carries that metadata, so the rename publishes it without recreating any of it. Before the rename, the tool rechecks the staged bytes and name, so a substituted or removed stage cannot be published, and rechecks the original's identity and bytes, so a concurrent change is reported.

A file with more than one hard link, a parent that refuses a new entry, a different owner or group, or any attribute the staged copy does not inherit takes the in-place path instead. A `Snapshot` comparison leaves the decision to the filesystem: a parent's default ACL, an inherited label, and a filesystem that cannot store an ACL all need no case of their own.

### In-Place Write

`write_in_place` rewrites through the original's held handle with `set_len(0)`, `write_all`, and `sync_all`. On Unix the inode keeps its owner, group, mode, ACLs, extended attributes, flags, and every hard link without recreating any of them, while the kernel clears set-ID bits and capabilities on a content write, so files carrying either are refused. On Windows the file keeps its owner, DACL, native attributes, and alternate streams, while content-dependent NTFS compression follows native write behavior. A final `Original::verify` confirms that the path still holds the written bytes. macOS and Windows always take this path, since reproducing macOS ACLs and BSD flags or Windows access-control state needs platform calls outside Rust's safe standard library.

### New Files

`create` stages a file in the parent, writes and syncs it, sets it to the mode a plain creation in that directory receives, which a `0666` probe reports through the umask, verifies the staged name, and publishes it with a no-clobber persist. A no-clobber publish never replaces an existing entry, including a dangling link.

## Design Decisions

1. **Choose the strategy per file.** One strategy either refuses files whose metadata a staged copy cannot reproduce, or gives up crash atomicity for every write. Comparing what the copy can reproduce keeps the common case atomic and writes the files that a rename would strip instead of rejecting them. Claude Code follows the same shape, renaming a staged copy after `chmod` and falling back to an in-place write, while Codex and pi write in place ([Claude Code](https://github.com/hakula139/claude-code), [Codex](https://github.com/openai/codex), [pi](https://github.com/badlogic/pi-mono)).

2. **Reproduce metadata by comparing snapshots.** Capturing the staged file and comparing it with the original asks the filesystem what it actually attached to the new file, so inherited ACLs, SELinux labels, and filesystems without extended-attribute support need no capability probe or special case.

3. **Only Linux renames.** A staged file already carries the owner of the process that creates it and the group and extended attributes its parent gives it, so `fchmod` completes the set with the original's mode. macOS and Windows need native ACL, flag, stream, and compression work to reach the same guarantee, and their in-place writes keep all of it already.

4. **Write through the held handle, not a reopened path.** The handle was opened without following a symlink and checked against the tracked bytes, so rewriting through it cannot land on a different file, and every hard link keeps seeing one inode.

5. **In-place publication trades crash atomicity for preservation.** A crash, a full disk, or a kill mid-write can leave partial content, while the atomic path publishes either the complete bytes or none of them. Ordinary workspace files keep the atomic path, while a file whose metadata the copy cannot inherit is written in place and gives up crash atomicity for that write.

6. **Hard links, set-ID files, and capability files.** A file with several links can sit inside the workspace while another link sits outside every root, so the workspace gate never auto-allows it, but an explicit rule or approval writes it in place and every link sees the new content. Set-ID files and files carrying a capability stay refused because a write can clear either and the tool cannot report what the file system did with them.

## Sources

- `crates/ox-core/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`, expected bytes from the Read-before-Edit gate.
- `crates/ox-core/src/permission.rs`: `is_hard_linked`, the workspace gate's hard-link rule.
- `crates/ox-core/src/tool/file.rs`: `write_file`, which resolves the tracker key before publication and reports the committed bytes.
- `crates/ox-core/src/tool/file/commit.rs`: `write_with`, `replace_atomically`, `write_in_place`, `create`, `Original::verify`.
- `crates/ox-core/src/tool/file/commit/metadata.rs`: `Snapshot`, `attributes`, `supported_attribute`.
- `crates/ox-core/src/util/fs.rs`: `open_no_follow`, `validate_regular_file`, `file_id`.
