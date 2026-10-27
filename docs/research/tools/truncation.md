# Tool Output Truncation (Reference)

Research on tool output bounding across reference codebases. Based on [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

Verified 2026-10-26 against Claude Code 2.1.87 (the frozen source snapshot), Codex `2351d9e1b608`, and opencode `v1.4.11-1713-g7d5e91b80`.

## Claude Code (TypeScript)

Tiered caps applied at the dispatcher (after `Tool::run`, before message build):

| Constant                             | Value   | Scope                   |
| ------------------------------------ | ------- | ----------------------- |
| `DEFAULT_MAX_RESULT_SIZE_CHARS`      | 50,000  | Per-tool default        |
| `MAX_TOOL_RESULT_TOKENS`             | 100,000 | System-wide soft cap    |
| `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` | 200,000 | Per-turn message budget |

When the per-message budget trips, the largest blocks are spilled to disk, replaced with a path preview the model can re-read. Feature-flagged (`tengu_hawthorn_window` in GrowthBook).

## OpenAI Codex (Rust)

Tool output passes through a per-model `TruncationPolicy` (`core/src/tools/context.rs`, defined in `protocol/src/protocol.rs`): a `Bytes(n)` or `Tokens(n)` budget from the model's metadata, falling back to `Bytes(10_000)`. The `tool_output_token_limit` config key overrides the budget. Exec, unified-exec, and MCP outputs are middle-truncated to it, keeping the head and tail, and a unified-exec call can request a tighter `max_output_tokens`. Nothing spills to disk.

## opencode (TypeScript)

Centralized via `Truncate.Service`:

| Constant    | Value  | Scope           |
| ----------- | ------ | --------------- |
| `MAX_LINES` | 2,000  | Per-tool result |
| `MAX_BYTES` | 50 KB  | Per-tool result |
| `RETENTION` | 7 days | Spill cleanup   |

When either limit trips: take head or tail slice, write full output to temp file, return preview with `{ truncated: true, outputPath }`. Hint adapts to agent capabilities.

## Comparison

| Repo        | Cap location | Per-tool cap | System cap | Strategy          | Spillover         | Per-tool override |
| ----------- | ------------ | ------------ | ---------- | ----------------- | ----------------- | ----------------- |
| Claude Code | dispatcher   | 50,000 chars | 100 K toks | tail-cut + spill  | yes (file path)   | yes               |
| Codex       | tool output  | none         | per model  | head-tail         | no                | exec, lower only  |
| opencode    | dispatcher   | 50 KB        | none       | head/tail + spill | yes (file + hint) | no                |
