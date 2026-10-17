# Tool Output Truncation

Two-layer truncation: per-tool view-shape caps + centralized byte-budget safety net.

## Implementation

### Centralized dispatcher cap (`tool.rs`)

- `MAX_OUTPUT_BYTES = 128 KB`: enforced on every tool output via `cap_output()`.
- `TRUNCATION_OVERHEAD = 80`: bytes reserved for the head-tail separator.
- `cap_output(content) -> (String, Option<len>)`: head + tail strategy preserving setup context and final outcome.
- `truncate_line(line, max_chars)`: per-line cap with a `[N chars]` suffix. Each caller picks its own limit.

### Per-tool view-shape caps

- **bash**: Drains stdout / stderr while keeping the head and tail of each pipe, so large streams cannot block on a full OS pipe and a trailing error summary survives. Rendering splits one budget just under `MAX_OUTPUT_BYTES` across both pipes, so each `[N bytes omitted]` marker counts everything its pipe dropped and the dispatcher cap never fires on bash output. A timeout kills the shell's process tree and returns the output captured so far.
- **edit / tasks / write**: No output truncation because success messages are tiny.
- **glob**: `MAX_RESULTS = 100` matches. Footer: `(Showing N of TOTAL matches.)`.
- **grep**: Per-line cap at `MAX_LINE_CHARS = 500`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable. File-size pre-gate at `MAX_GREP_FILE_SIZE = 1 MB`.
- **read**: Per-line cap at `MAX_LINE_CHARS = 2000`, row cap at `DEFAULT_LINE_LIMIT = 2000`, and a byte budget just under `MAX_OUTPUT_BYTES` so the footer reports the lines actually shown. File-size pre-gate at `MAX_TRACKED_FILE_SIZE = 10 MB`.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape stays per-tool because it needs tool-specific knowledge. Byte-budget runs after `Tool::run` as the absolute safety net.
2. **Head + tail strategy.** Keeps the command setup and final outcome, the two slices the model needs most. Tail-only would lose the prompt, and head-only would lose the result.
3. **No spillover to disk.** Output past the 128 KB head and tail is dropped, which keeps enough for one-shot commands. opencode-style spill pays off only once a later call can read the full output, as with background shell commands in [#137](https://github.com/hakula139/oxide-code/issues/137).
4. **Two distinct metadata fields.** `ToolMetadata::truncated_total` (unbounded match counts from per-tool caps) vs `ToolMetadata::truncated_bytes` (pre-cap byte count from the registry's safety net). Prevents glob's `(X of N matches)` from rendering bytes when both layers fire.
5. **Per-tool line caps.** Read allows 2000 characters per line because Edit needs the exact text of the lines it targets. Grep lists many hits per call and stays at 500.

## Sources

- `crates/oxide-code/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/oxide-code/src/tool.rs`: `ToolRegistry::run`, `cap_output()`, `MAX_OUTPUT_BYTES`, `truncate_line()`.
- `crates/oxide-code/src/tool/bash.rs`: `OUTPUT_BUDGET`, `PipeCapture`, `render_pipe()`.
- `crates/oxide-code/src/tool/glob.rs`: `MAX_RESULTS`, `truncated_total` setter.
- `crates/oxide-code/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_GREP_FILE_SIZE`, `MAX_LINE_CHARS`.
- `crates/oxide-code/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`, `MAX_LINE_CHARS`, `OUTPUT_BUDGET`.
