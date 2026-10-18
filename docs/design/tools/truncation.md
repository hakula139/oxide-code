# Tool Output Truncation

Two-layer truncation: per-tool view-shape caps and a centralized byte-budget safety net.

## Implementation

### Centralized Dispatcher Cap (`tool.rs`)

- `MAX_OUTPUT_BYTES = 128 KB`: enforced on every tool output via `cap_output()`.
- `TRUNCATION_OVERHEAD = 80`: bytes reserved for the head-tail separator.
- `cap_output(content) -> (String, Option<len>)`: keeps the head and tail, preserving setup context and the final outcome.
- `truncate_line(line, max_chars)`: per-line cap with a `[N chars]` suffix. Each caller picks its own limit.

### Per-Tool View-Shape Caps

- **bash**: Drains stdout / stderr while keeping the head and tail of each pipe, so large streams cannot block on a full OS pipe and a trailing error summary survives. Rendering splits one budget just under `MAX_OUTPUT_BYTES` across both pipes, so each `[N bytes omitted]` marker counts everything its pipe dropped and the dispatcher cap never fires on bash output. A timeout kills the shell's process tree and returns the output captured so far. A background shell's captures hold what arrived since the last `bash_output` read under the same bounds, and each read empties them.
- **edit / tasks / write**: No output truncation because success messages are tiny.
- **glob**: `MAX_RESULTS = 100` matches. Footer: `(Showing 100 of N matches. Use a more specific pattern.)`. A `Could not read:` note lists the paths the walk failed to read, naming at most `MAX_NOTE_PATHS = 10` and counting the rest.
- **grep**: Per-line cap at `MAX_LINE_CHARS = 500`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable. File-size pre-gate at `MAX_GREP_FILE_SIZE = 1 MB`. The notes listing files skipped for size and paths the search failed to read each name at most `MAX_NOTE_PATHS = 10` and count the rest.
- **read**: Per-line cap at `MAX_LINE_CHARS = 2000`, row cap at `DEFAULT_LINE_LIMIT = 2000`, and a byte budget just under `MAX_OUTPUT_BYTES` so the footer reports the lines actually shown. File-size pre-gate at `MAX_TRACKED_FILE_SIZE = 10 MB`.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape stays per-tool because it needs tool-specific knowledge. Byte-budget runs after `Tool::run` as the absolute safety net.
2. **Head-and-tail strategy.** Keeps the command setup and final outcome, the two slices the model needs most. Tail-only would lose the prompt, and head-only would lose the result.
3. **No spillover to disk.** Output past the 128 KB head and tail is dropped, which keeps enough for one-shot commands. A long-running background shell needs no spill either, because each `bash_output` read returns only what arrived since the previous one, so a model that reads before 128 KB piles up sees the whole stream.
4. **Two distinct metadata fields.** `ToolMetadata::truncated_total` holds the unbounded match count from a per-tool cap, and `ToolMetadata::truncated_bytes` holds the pre-cap byte count from the registry's safety net. Keeping them apart stops the glob view's `<pattern> (X of N)` header from counting bytes when both layers fire.
5. **Per-tool line caps.** Read allows 2000 characters per line because Edit needs the exact text of the lines it targets. Grep lists many hits per call and stays at 500.

## Sources

- `crates/oxide-code/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/oxide-code/src/tool.rs`: `run_capped`, `cap_output()`, `MAX_OUTPUT_BYTES`, `truncate_line()`, `MAX_NOTE_PATHS`, `push_path_note()`.
- `crates/oxide-code/src/tool/bash.rs`: `OUTPUT_BUDGET`, `PipeCapture`, `render_pipe()`.
- `crates/oxide-code/src/tool/bash/background.rs`: `ShellOutput::take`, the per-read capture of a background shell.
- `crates/oxide-code/src/tool/glob.rs`: `MAX_RESULTS`, `truncated_total` setter.
- `crates/oxide-code/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_GREP_FILE_SIZE`, `MAX_LINE_CHARS`.
- `crates/oxide-code/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
