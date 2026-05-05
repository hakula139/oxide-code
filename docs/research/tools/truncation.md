# Tool Output Truncation (Reference)

Research on tool output bounding across reference codebases. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Claude Code (TypeScript)

Tiered caps applied at the dispatcher (after `Tool::run`, before message build):

| Constant                             | Value   | Scope                    |
| ------------------------------------ | ------- | ------------------------ |
| `DEFAULT_MAX_RESULT_SIZE_CHARS`      | 50,000  | Per-tool default         |
| `MAX_TOOL_RESULT_TOKENS`             | 100,000 | System-wide soft cap     |
| `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` | 200,000 | Per-turn message budget  |

When the per-message budget trips, the largest blocks are spilled to disk, replaced with a path preview the model can re-read. Feature-flagged (`tengu_hawthorn_window` in GrowthBook).

## OpenAI Codex (Rust)

No system-wide cap. Tools bound their own output (e.g., `MATCH_LIMIT = 50` for ripgrep). A `bash cat large.log` returns whatever the command printed.

## opencode (TypeScript)

Centralized via `Truncate.Service`:

| Constant    | Value  | Scope           |
| ----------- | ------ | --------------- |
| `MAX_LINES` | 2,000  | Per-tool result |
| `MAX_BYTES` | 50 KB  | Per-tool result |
| `RETENTION` | 7 days | Spill cleanup   |

When either limit trips: take head or tail slice, write full output to temp file, return preview with `{ truncated: true, outputPath }`. Hint adapts to agent capabilities.

## Comparison

| Repo        | Cap location | Per-tool cap | System cap | Strategy          | Spillover          | Per-tool override |
| ----------- | ------------ | ------------ | ---------- | ----------------- | ------------------ | ----------------- |
| Claude Code | dispatcher   | 50 KB chars  | 100 K toks | tail-cut + spill  | yes (file path)    | yes               |
| Codex       | per-tool     | varies       | none       | varies (or none)  | no                 | n/a               |
| opencode    | dispatcher   | 50 KB        | none       | head/tail + spill | yes (file + hint)  | no                |
| oxide-code  | dispatcher   | per-tool     | 128 KB     | head-tail + spill | no                 | yes               |
