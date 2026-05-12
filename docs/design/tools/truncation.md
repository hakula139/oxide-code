# Tool Output Truncation

Two-layer truncation: per-tool view-shape caps + centralized byte-budget safety net.

## Implementation

### Centralized dispatcher cap (`tool.rs`)

- `MAX_OUTPUT_BYTES = 128 KB`: enforced on every tool output via `cap_output()`.
- `TRUNCATION_OVERHEAD = 80`: bytes reserved for the head-tail separator.
- `cap_output(content) -> (String, Option<len>)`: head + tail strategy preserving setup context and final outcome.
- `MAX_LINE_LENGTH = 500`: per-line cap consumed by `read` and `grep` via `truncate_line`.

### Per-tool view-shape caps

- **bash**: Drains stdout / stderr while retaining `MAX_OUTPUT_BYTES` per pipe so large streams cannot block on a full OS pipe. The dispatcher cap still applies to the combined output.
- **edit / write**: No output truncation because success messages are tiny.
- **glob**: `MAX_RESULTS = 100` matches. Footer: `(Showing N of TOTAL matches.)`.
- **grep**: Per-line `truncate_line`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable. File-size pre-gate at `MAX_GREP_FILE_SIZE = 1 MB`.
- **read**: Per-line `truncate_line` (500 chars), row cap at `DEFAULT_LINE_LIMIT = 2000`, mid-loop byte budget. File-size pre-gate at `MAX_TRACKED_FILE_SIZE = 10 MB`.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape stays per-tool because it needs tool-specific knowledge. Byte-budget runs after `Tool::run` as the absolute safety net.
2. **Head + tail strategy.** Keeps the command setup and final outcome, the two slices the model needs most. Tail-only would lose the prompt, and head-only would lose the result.
3. **No spillover to disk.** opencode-style spill needs a Task agent, and 128 KB head-tail preserves enough today. Add spill when Task lands.
4. **Two distinct metadata fields.** `ToolMetadata::truncated_total` (unbounded match counts from per-tool caps) vs `ToolMetadata::truncated_bytes` (pre-cap byte count from the registry's safety net). Prevents glob's `(X of N matches)` from rendering bytes when both layers fire.

## Sources

- `crates/oxide-code/src/tool.rs`: `ToolRegistry::run`, `cap_output()`, `MAX_OUTPUT_BYTES`, `truncate_line()`.
- `crates/oxide-code/src/tool/glob.rs`: `MAX_RESULTS`, `truncated_total` setter.
- `crates/oxide-code/src/tool/grep.rs`: `DEFAULT_HEAD_LIMIT`, `MAX_GREP_FILE_SIZE`.
- `crates/oxide-code/src/file_tracker.rs`: `MAX_TRACKED_FILE_SIZE`.
- `crates/oxide-code/src/tool/read.rs`: `DEFAULT_LINE_LIMIT`.
