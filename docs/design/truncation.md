# Tool Output Truncation

Two-layer truncation combines per-tool view-shape caps with a centralized byte-budget safety net. Output cut by the dispatcher cap or a `bash` output capture is saved in full to a spill file the model can read back.

## Centralized Dispatcher Cap

- `MAX_OUTPUT_BYTES = 128 KiB`: enforced on every tool output via `cap_output()`.
- `cap_output(content, spill) -> String`: for content over the cap, writes the whole content to the call's spill file, then keeps up to half the cap left after the marker from each end around a `... [N bytes omitted, full output saved to <path>] ...` marker, preserving setup context and the final outcome. The head ends and the tail starts at a line boundary when a line break lies within `LINE_SNAP_BYTES = 1 KiB` of the cut, so the lines beside the marker are whole, while a line longer than that window keeps the UTF-8 boundary cut. A cut that already falls at a line boundary stays put. The dropped partial lines, and the line break beside the marker, count in its omitted total. Because the marker counts against the cap, a long spill path or a failure note leaves less room for the head and tail and never pushes the result past `MAX_OUTPUT_BYTES`. `run_capped` calls it on the blocking thread pool because it writes the whole content at once.

## Spill Files

- **Location**: `store::artifact_dir` designates the directory that holds a session's artifacts, `$XDG_DATA_HOME/ox/artifacts/<session-id>/`, with spill files placed in its `tool-results/` child. The engine passes the live handle's directory with each dispatch, so new spills land with the session in use after `/clear`, `/resume`, or a [lazy fork](persistence.md#lazy-fork). Tool results a fork copied still name the source session's spills.
- **Names**: Each file name begins with the call's tool use ID, replacing every character other than an ASCII letter, digit, `-`, or `_` with `_`. The dispatcher writes `<id>.txt`, a foreground `bash` call writes `<id>.output.txt`, and the `n`th read of a background shell by `bash_output` or `bash_stop` writes `<id>.<n>.output.txt`, where `<id>` is the call that started the shell.
- **Creation**: The first write creates the directory and the file and refuses to replace an existing file, so a result that fits leaves nothing on disk. On Unix, the directory gets `0700` perms and the file gets `0600`.
- **Size**: A file keeps at most `MAX_SPILL_BYTES`, equal to `MAX_TRACKED_FILE_SIZE` so `read` can open every spill, and beyond that limit the marker says `first 10 MiB of output saved to <path>`. If creating or writing the file fails, the result is still truncated, and the marker names the error in place of the path.
- **Access and cleanup**: `read` and `grep` open spill files without prompting, while editing them prompts, since they lie in oxide-code's data directory ([permissions](permissions.md#shipped-defaults)). `/delete` removes the session's directory along with its rows, and archiving keeps both.

## Per-Tool View-Shape Caps

The per-line caps below go through `truncate_line(line, max_chars)`, which appends a `[N chars]` suffix with the line's full length.

- **bash**: stdout and stderr share one pipe, so the output holds the command's writes in the order they reach it, with the costs that [Shell Execution](bash.md#design-decisions) records. ox drains the pipe while keeping its head and tail, ensuring large output cannot block on a full OS pipe and a trailing error summary survives. Once the pipe sends more than the head and tail hold, every byte it sends streams to the spill file, keeping memory usage at the head and tail buffers plus one read chunk however much the command prints. Rendering keeps a budget just under `MAX_OUTPUT_BYTES`, less the spill path the marker names, so the marker counts everything the capture dropped and the dispatcher cap never fires on bash output. Both ends beside the marker move to line boundaries under the same rule as `cap_output`, for which the capture records whether the last byte it dropped was a line break. A capture that dropped nothing renders its head and tail as one stretch, splitting them at the start of the line the head ends in, so a short tail still holds the command's last line. The budget counts decoded bytes, in which each invalid UTF-8 sequence takes the three bytes of U+FFFD, so binary output such as `cat` on an image stays within it, while the marker counts the raw bytes dropped, and the spill holds the output as the command wrote it. If the budget cuts a capture whose head and tail held all of the output, the capture spills those bytes at render time. A timeout kills the shell's process tree and returns the output captured so far. A background shell's capture holds what arrived since the last `bash_output` read under the same bounds, and each read empties it, spilling to a file numbered by that read. A read waits only for a spill write already in flight.
- **edit / tasks / write**: No output truncation because success messages are tiny.
- **glob**: Keeps the newest `MAX_RESULTS = 100` matches while walking, with walk order breaking modification-time ties, and counts every match. Footer: `(Showing 100 of N matches. Use a more specific pattern.)`, with the full count in the stored view. A `Could not read:` note lists the paths the walk failed to read, naming at most `MAX_NOTE_PATHS = 10` and counting the rest.
- **grep**: Per-line cap at `MAX_LINE_CHARS = 500`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable. All modes share a row collector bounded by the requested prefix and `MAX_SPILL_BYTES`, so `head_limit: 0` cannot retain every match of a large tree. Content and file-list modes stop reading after their output fills. Count mode continues reading for exact totals while retaining the bounded prefix. File-size pre-gate at `MAX_WALK_FILE_SIZE = 1 MiB` for files found by a directory walk, and at `MAX_TRACKED_FILE_SIZE = 10 MiB`, as for `read`, when the search root is one file such as a spill. The notes listing files skipped for size and paths the search failed to read each name at most `MAX_NOTE_PATHS = 10` and count the rest.
- **read**: Complete rows up to `DEFAULT_LINE_LIMIT = 2000` and a byte budget just under `MAX_OUTPUT_BYTES` so the footer reports the displayed lines. A row larger than the entire budget returns an explicit error when requested. File-size limit at `MAX_TRACKED_FILE_SIZE = 10 MiB`, enforced on the opened regular file and during reading.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape stays per-tool because it needs tool-specific knowledge. Byte-budget runs after `Tool::run` as the absolute safety net.
2. **Head-and-tail strategy.** Keeps the command setup and final outcome, the two slices the model needs most. Tail-only would lose the prompt, and head-only would lose the result.
3. **Spill the cut output to a session artifact.** The model gets the head and tail inline, and the marker names a file holding the rest for `read` and `grep` to page through when the middle matters. Keying the directory by session ID ties the files to the session, so `/delete` removes them and nothing else needs to clean up. Because `bash` spills as it drains, a command's full output never sits in memory, and a background shell spills each read's window to its own file. Because the cap applies to each file, and so to each call or read, the total is unbounded: a long-running background shell read many times can fill the disk across its files, which [#679](https://github.com/hakula139/oxide-code/issues/679) tracks.
4. **Complete Read rows.** Read emits each line in full because Edit needs its exact text and Write needs coverage of every byte. Grep lists many hits per call and caps each line at 500 characters.
5. **A fixed 1 KiB window for line-boundary cuts.** A cut always moves to the nearest line break, so a wider window drops no more than the partial line, but it lets a partial line of several KiB vanish whole at the cost of up to that much of each end's budget. 1 KiB is under 2% of a 64 KiB end, while longer lines, such as minified code, keep the char cut and stay visible.

## Sources

- `crates/ox-core/src/engine.rs`: the live session's artifact directory, which fills `DispatchContext::artifact_dir` for each dispatch.
- `crates/ox-core/src/engine/dispatch.rs`: `DispatchContext::artifact_dir`, which each dispatch hands to `ToolCtx`.
- `crates/ox-core/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/ox-core/src/session/handle.rs`: `SessionHandle::artifact_dir`.
- `crates/ox-core/src/session/store.rs`: `SessionStore::artifact_dir` and `SessionStore::delete`.
- `crates/ox-core/src/tool.rs`: `truncate_line()`.
- `crates/ox-core/src/tool/bash.rs`: `OUTPUT_BUDGET`, `ShellTree::spawn()`, `PipeCapture`, `render_pipe()`, `ends_within()`.
- `crates/ox-core/src/tool/bash/background.rs`: `ShellOutput::take`, the per-read capture of a background shell.
- `crates/ox-core/src/tool/glob.rs`: `MAX_RESULTS`, the `GlobFiles` view's total.
- `crates/ox-core/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_LINE_CHARS`, and `MAX_WALK_FILE_SIZE`.
- `crates/ox-core/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `OUTPUT_BUDGET`.
- `crates/ox-core/src/tool/search.rs`: `SearchRows`, the shared row and byte cap, `newest_first()`, `MAX_NOTE_PATHS`, and `push_path_note()`.
- `crates/ox-core/src/tool/spill.rs`: `run_capped`, `cap_output()`, `MAX_OUTPUT_BYTES`, `LINE_SNAP_BYTES`, `whole_line_head()`, `whole_line_tail()`, `Spill`, `MAX_SPILL_BYTES`, `SPILL_DIR`.
