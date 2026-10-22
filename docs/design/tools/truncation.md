# Tool Output Truncation

Two-layer truncation combines per-tool view-shape caps with a centralized byte-budget safety net. Output cut by the dispatcher cap or a `bash` pipe capture is saved in full to a spill file the model can read back.

## Implementation

### Centralized Dispatcher Cap

- `MAX_OUTPUT_BYTES = 128 KB`: enforced on every tool output via `cap_output()`.
- `cap_output(content, spill) -> String`: for content over the cap, writes the whole content to the call's spill file, then keeps equal parts of the head and tail around a `... [N bytes omitted, full output saved to <path>] ...` marker, preserving setup context and the final outcome. Because the marker counts against the cap, a long spill path or a failure note leaves less room for the head and tail and never pushes the result past `MAX_OUTPUT_BYTES`. `run_capped` calls it on the blocking thread pool because it writes the whole content at once.

### Spill Files

- **Location**: `store::artifact_dir` designates the directory beside a session file, named after the session ID, that holds the session's artifacts, with spill files placed in its `tool-results/` child. For a session in the store, this is `$XDG_DATA_HOME/ox/sessions/<project>/<session-id>/`, and a session resumed from a file outside the store spills beside that file. Because the session handle derives the directory from the file its writer creates or resumes and the engine passes the live handle's directory with each dispatch, new spills land with the session in use after `/clear` or `/resume`.
- **Names**: Each file name begins with the call's tool use ID, replacing every character other than an ASCII letter, digit, `-`, or `_` with `_`. The dispatcher writes `<id>.txt`, a foreground `bash` call writes `<id>.stdout.txt` and `<id>.stderr.txt`, and the `n`th read of a background shell by `bash_output` or `bash_stop` writes `<id>.<n>.stdout.txt` and `<id>.<n>.stderr.txt`, where `<id>` is the call that started the shell.
- **Creation**: The first write creates the directory and the file and refuses to replace an existing file, so a result that fits leaves nothing on disk. On Unix, the directory gets `0700` perms and the file gets `0600`.
- **Size**: A file keeps at most `MAX_SPILL_BYTES`, equal to `MAX_TRACKED_FILE_SIZE` so `read` can open every spill, and beyond that limit the marker says `first 10 MiB of output saved to <path>`. If creating or writing the file fails, the result is still truncated, and the marker names the error in place of the path.
- **Access and cleanup**: `read` and `grep` open spill files without prompting, while editing prompts only when the file lies in oxide-code's data directory, so the spills of a session resumed from a file elsewhere lack that protection ([permissions](permissions.md#shipped-defaults)). `/delete` removes the session's directory along with its session file, and archiving keeps both.

### Per-Tool View-Shape Caps

The per-line caps below go through `truncate_line(line, max_chars)`, which appends a `[N chars]` suffix with the line's full length.

- **bash**: Drains stdout / stderr while keeping the head and tail of each pipe, ensuring large streams cannot block on a full OS pipe and a trailing error summary survives. Once a pipe sends more than its head and tail hold, every byte it sends streams to its spill file, keeping memory usage at the head and tail buffers plus one read chunk however much the command prints. Rendering splits a single budget just under `MAX_OUTPUT_BYTES`, less the spill paths the markers name, across both pipes, so each marker counts everything its pipe dropped and the dispatcher cap never fires on bash output. The budget counts decoded bytes, in which each invalid UTF-8 sequence takes the three bytes of U+FFFD, so binary output such as `cat` on an image stays within it, while each marker counts the raw bytes its pipe dropped, and the spill holds the pipe's output as the command wrote it. If the split cuts a pipe whose head and tail held all of its output, that pipe spills those bytes at render time. A timeout kills the shell's process tree and returns the output captured so far. A background shell's captures hold what arrived since the last `bash_output` read under the same bounds, and each read empties them, spilling to files numbered by that read. Each pipe's capture has its own lock, so a pump writing its spill holds up neither the other pipe nor a read, which waits only for a write already in flight.
- **edit / tasks / write**: No output truncation because success messages are tiny.
- **glob**: `MAX_RESULTS = 100` matches. Footer: `(Showing 100 of N matches. Use a more specific pattern.)`, with the full count in `ToolMetadata::truncated_total` for the view header. A `Could not read:` note lists the paths the walk failed to read, naming at most `MAX_NOTE_PATHS = 10` and counting the rest.
- **grep**: Per-line cap at `MAX_LINE_CHARS = 500`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable. Content mode also stops collecting rows past `MAX_SPILL_BYTES`, the most a spill keeps, so `head_limit: 0` cannot hold every match of a large tree in memory. File-size pre-gate at `MAX_WALK_FILE_SIZE = 1 MB` for files found by a directory walk, and at `MAX_TRACKED_FILE_SIZE = 10 MB`, as for `read`, when the search root is one file such as a spill. The notes listing files skipped for size and paths the search failed to read each name at most `MAX_NOTE_PATHS = 10` and count the rest.
- **read**: Per-line cap at `MAX_LINE_CHARS = 2000`, row cap at `DEFAULT_LINE_LIMIT = 2000`, and a byte budget just under `MAX_OUTPUT_BYTES` so the footer reports the displayed lines. File-size pre-gate at `MAX_TRACKED_FILE_SIZE = 10 MB`.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape stays per-tool because it needs tool-specific knowledge. Byte-budget runs after `Tool::run` as the absolute safety net.
2. **Head-and-tail strategy.** Keeps the command setup and final outcome, the two slices the model needs most. Tail-only would lose the prompt, and head-only would lose the result.
3. **Spill the cut output to a session file.** The model gets the head and tail inline, and the marker names a file holding the rest for `read` and `grep` to page through when the middle matters. Keying the directory by session ID beside the session file ties the files to the session, so `/delete` removes them and nothing else needs to clean up. Because `bash` spills as it drains, a command's full output never sits in memory, and a background shell spills each read's window to its own files. Because the cap applies to each file, and so to each call or read, the total is unbounded: a long-running background shell read many times can fill the disk across its files.
4. **Per-tool line caps.** Read allows 2000 characters per line because Edit needs the exact text of the lines it targets. Grep lists many hits per call and stays at 500.

## Sources

- `crates/ox-core/src/engine.rs`: the live session's artifact directory, which fills `DispatchContext::artifact_dir` for each dispatch.
- `crates/ox-core/src/engine/dispatch.rs`: `DispatchContext::artifact_dir`, which each dispatch hands to `ToolCtx`.
- `crates/ox-core/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/ox-core/src/session/handle.rs`: `SessionHandle::artifact_dir`.
- `crates/ox-core/src/session/store.rs`: `artifact_dir`, `SessionStore::session_file`, `delete_at`.
- `crates/ox-core/src/tool.rs`: `truncate_line()`.
- `crates/ox-core/src/tool/bash.rs`: `OUTPUT_BUDGET`, `PipeCapture`, `render_pipe()`.
- `crates/ox-core/src/tool/bash/background.rs`: `ShellOutput::take`, the per-read capture of a background shell.
- `crates/ox-core/src/tool/glob.rs`: `MAX_RESULTS`, `truncated_total` setter.
- `crates/ox-core/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_LINE_CHARS`, `MAX_WALK_FILE_SIZE`, and `Rows`, the content-mode row and byte cap.
- `crates/ox-core/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
- `crates/ox-core/src/tool/search.rs`: `MAX_NOTE_PATHS`, `push_path_note()`.
- `crates/ox-core/src/tool/spill.rs`: `run_capped`, `cap_output()`, `MAX_OUTPUT_BYTES`, `Spill`, `MAX_SPILL_BYTES`, `SPILL_DIR`.
