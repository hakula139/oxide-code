# Tool Output Truncation

Two-layer truncation: per-tool view-shape caps + centralized byte-budget safety net.

## Implementation

### Centralized dispatcher cap (`tool.rs`)

- `MAX_OUTPUT_BYTES = 128 KB` -- enforced on every tool output via `cap_output()`.
- `TRUNCATION_OVERHEAD = 50` -- bytes reserved for the head-tail separator.
- `cap_output(content) -> (String, Option<len>)` -- head-tail strategy preserving setup context and final outcome.
- `MAX_LINE_LENGTH = 500` -- per-line cap consumed by `read` and `grep` via `truncate_line`.

### Per-tool view-shape caps

- **bash** -- no per-tool byte cap (rides the dispatcher).
- **edit / write** -- no output truncation; success messages are tiny.
- **glob** -- `MAX_RESULTS = 100` matches. Footer: `(Showing N of TOTAL matches.)`.
- **grep** -- per-line `truncate_line`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable. File-size pre-gate at `MAX_GREP_FILE_SIZE = 1 MB`.
- **read** -- per-line `truncate_line` (500 chars), row cap at `DEFAULT_LINE_LIMIT = 2000`, mid-loop byte-budget. File-size pre-gate at `MAX_READ_FILE_SIZE = 10 MB`.

## Design Decisions

1. **Two truncation layers, separated by responsibility.** View-shape (per-tool) stays per-tool -- tool-specific knowledge. Byte-budget (centralized) runs after `Tool::run` as the absolute safety net.
2. **Head-tail, not tail-only.** Preserves both the command and the outcome -- the two pieces the model most needs.
3. **No spillover to disk.** opencode-style spill needs a Task agent. 128 KB head-tail preserves enough. Add when Task lands.
4. **Two distinct metadata fields.** `ToolMetadata::truncated_total` (unbounded match counts from per-tool caps) vs `ToolMetadata::truncated_bytes` (pre-cap byte count from the registry's safety net). Prevents glob's `(X of N matches)` from rendering bytes when both layers fire.

## Sources

- `crates/oxide-code/src/tool.rs` -- `ToolRegistry::run`, `cap_output()`, `MAX_OUTPUT_BYTES`, `truncate_line()`.
- `crates/oxide-code/src/tool/glob.rs` -- `MAX_RESULTS`, `truncated_total` setter.
- `crates/oxide-code/src/tool/grep.rs` -- `DEFAULT_HEAD_LIMIT`, `MAX_GREP_FILE_SIZE`.
- `crates/oxide-code/src/tool/read.rs` -- `DEFAULT_LINE_LIMIT`, `MAX_READ_FILE_SIZE`.
