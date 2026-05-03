# Tool Output Truncation

Research notes on how reference codebases bound tool output bytes before they reach the model. Tool calls can produce arbitrary payloads — `bash cat` on a 50 MB log, `grep` over a vendored bundle, `find` across `/usr` — and shipping the full bytes to the API wastes context window or overruns hard caps. Each codebase strikes a different point on the "truncate where, by how much, with what fallback" trade-off. Based on analysis of [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Reference Implementations

### Claude Code (TypeScript)

Tiered caps applied at the dispatcher (after `Tool::run`, before message build):

| Constant                             | Value   | Scope                                 |
| ------------------------------------ | ------- | ------------------------------------- |
| `DEFAULT_MAX_RESULT_SIZE_CHARS`      | 50,000  | Per-tool default                      |
| `MAX_TOOL_RESULT_TOKENS`             | 100,000 | System-wide soft cap                  |
| `MAX_TOOL_RESULT_BYTES`              | 400,000 | Derived: `MAX_TOOL_RESULT_TOKENS × 4` |
| `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` | 200,000 | Per-turn message budget               |
| `BYTES_PER_TOKEN`                    | 4       | Token-estimation constant             |

Per-tool defaults are tightened by tool config; per-message budget overrides individual tool caps when the turn carries multiple results. When the per-message budget trips, the largest blocks are spilled to disk, replaced with a path preview the model can re-read via the file tools. The override is feature-flagged (`tengu_hawthorn_window` in GrowthBook) — caps can be tuned without a release.

No per-line truncation in the dispatcher; that's the tool schema's responsibility.

**Sources:**

- `claude-code/src/services/streamingToolExecutor.ts` — per-tool then per-message cap application.
- `claude-code/src/utils/toolLimits.ts` — tiered cap constants.

### OpenAI Codex (Rust)

No system-wide cap. Tools either bound their own output (e.g., `MATCH_LIMIT = 50` for ripgrep results in `fuzzy_file_search.rs`) or return whatever the underlying command produces. Pagination caps appear on message-level features (`THREAD_LIST_DEFAULT_LIMIT = 25`, `THREAD_TURNS_MAX_LIMIT = 100`) but those bound list responses, not tool output bytes.

The implication: a `bash cat large.log` returns however many bytes ripgrep / cat printed. Codex relies on the model to ask for tighter ranges if a tool emits too much.

**Sources:**

- `codex-rs/core/src/tools/handlers/fuzzy_file_search.rs` — `MATCH_LIMIT = 50`, per-tool with no central layer.

### opencode (TypeScript)

Centralized via `Truncate.Service` (one truncation pass after the tool runs, before the result is appended to the message):

| Constant    | Value  | Scope                     |
| ----------- | ------ | ------------------------- |
| `MAX_LINES` | 2,000  | Per-tool result           |
| `MAX_BYTES` | 50 KB  | Per-tool result           |
| `RETENTION` | 7 days | Spilled-file auto-cleanup |

When either limit trips, the service:

1. Takes a head or tail slice (configurable `direction: "head" | "tail"`).
2. Writes the full output to a temp file under `TRUNCATION_DIR`.
3. Returns a result with `{ content: preview, truncated: true, outputPath: path }`.

The hint string adapts to agent capabilities — with a Task tool it reads `"Use the Task tool to have explore agent process this file..."`; without it, `"Use Grep / Read with offset/limit on the full content..."`.

**Sources:**

- `opencode/packages/opencode/src/tool/truncate.ts` — `Truncate.Service`: `MAX_LINES = 2000`, `MAX_BYTES = 50 KB`, `RETENTION = 7 days`, head / tail + file spillover with adapted hint.

## Comparison

| Repo               | Cap location | Per-tool cap | System cap | Strategy           | Spillover              | Per-tool override |
| ------------------ | ------------ | ------------ | ---------- | ------------------ | ---------------------- | ----------------- |
| claude-code        | dispatcher   | 50 KB chars  | 100 K toks | tail-cut + spill   | yes (file path)        | yes               |
| codex              | per-tool     | varies       | none       | varies (or none)   | no                     | n/a               |
| opencode           | dispatcher   | 50 KB        | none       | head/tail + spill  | yes (file path + hint) | no                |
| oxide-code         | dispatcher   | per-tool     | 128 KB     | head-tail + spill  | no                     | yes               |

## oxide-code Implementation

Two-layer truncation is shipped. Per-tool **view-shape** caps (row limits, line-length caps) remain tool-specific; a centralized **byte-budget** (`cap_output` in `ToolRegistry::run`) runs after every `Tool::run` at the dispatcher level.

### Centralized dispatcher cap (`tool.rs`)

- `MAX_OUTPUT_BYTES = 128 * 1024` — enforced on every tool output via `cap_output()`.
- `TRUNCATION_OVERHEAD = 50` — bytes reserved for the head-tail separator.
- `cap_output(content) -> (String, Option<len>)` — head-tail strategy preserving setup context and final outcome. Returns the original byte count when the cap fires.
- `ToolMetadata::truncated_bytes` — set by the dispatcher when the cap fires.
- `MAX_LINE_LENGTH = 500` — per-line cap consumed by `read` and `grep` via `truncate_line`.
- `truncate_line(line) -> Cow<str>` — multibyte-safe per-line cap with `... [N chars]` suffix. Fast-path borrow when under cap.

### Per-tool view-shape caps

- **`tool/bash.rs`** — no per-tool byte cap (rides the dispatcher's `cap_output`).
- **`tool/edit.rs`** / **`tool/write.rs`** — no output truncation; success messages are tiny.
- **`tool/glob.rs`** — row cap at `MAX_RESULTS = 100` matches. Footer: `(Showing N of TOTAL matches.)`. Populates `ToolMetadata::truncated_total`.
- **`tool/grep.rs`** — per-line `truncate_line`, per-mode row cap at `DEFAULT_HEAD_LIMIT = 250`, user-overridable via `head_limit`. File-size pre-gate at `MAX_GREP_FILE_SIZE = 1 MB`.
- **`tool/read.rs`** — per-line `truncate_line` (500 chars), row cap at `DEFAULT_LINE_LIMIT = 2000`, mid-loop byte-budget. File-size pre-gate at `MAX_READ_FILE_SIZE = 10 MB`.

## Design Decisions for oxide-code

Decisions that shaped the implementation:

1. **Two truncation layers, separated by responsibility.**
   - **View-shape** (per-tool): row caps, line-length caps, structured renderers. Stays per-tool — these are tool-specific knowledge ("250 grep matches", "2000 read lines").
   - **Byte-budget** (centralized): the absolute byte cap so no tool can flood context. Lifts to `ToolRegistry::run` and runs after the per-tool `Tool::run`. Bash's existing head-tail logic is the starting point — preserves both setup context and final outcome.
2. **Head-tail, not tail-only.** More useful across tools because it keeps the two pieces a model most often needs to reason about (the command, the outcome). Bash's pre-refactor tests carry over to `tool.rs` as `cap_output_keeps_head_and_tail`, `cap_output_multibyte_at_split_boundary`, `cap_output_barely_over_limit_unchanged`.
3. **No spillover to disk.** opencode-style `"use Grep on /tmp/spill.txt"` needs a Task agent to consume the file. oxide-code has none. The 128 KB head-tail rendering already preserves enough on each side for the model to reason about most outputs. Add when Task lands.
4. **Two distinct metadata fields, not one overloaded signal.** `ToolMetadata::truncated_total` carries unbounded match counts (set by per-tool view-shape caps like glob's `MAX_RESULTS`); `ToolMetadata::truncated_bytes` carries the pre-cap byte count (set only by the registry's safety net). Splitting keeps glob's `(X of N matches)` renderer from accidentally rendering bytes when both layers fire — plausible in deep monorepos with long absolute paths.

## Sources

- `crates/oxide-code/src/tool.rs` — `ToolRegistry::run` (dispatcher cap entry point), `cap_output()` (head-tail), `MAX_OUTPUT_BYTES`, `TRUNCATION_OVERHEAD`, `MAX_LINE_LENGTH`, `truncate_line()`.
- `crates/oxide-code/src/tool/glob.rs` — `MAX_RESULTS`, view-shape `truncated_total` setter.
- `crates/oxide-code/src/tool/grep.rs` — `DEFAULT_HEAD_LIMIT`, per-mode row caps, `MAX_GREP_FILE_SIZE`.
- `crates/oxide-code/src/tool/read.rs` — `DEFAULT_LINE_LIMIT`, `MAX_READ_FILE_SIZE`, view-shape footer.
