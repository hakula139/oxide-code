# Extended Thinking

Research notes on how Claude Code handles extended thinking, content block types, and signature verification. Based on [Claude Code](https://github.com/hakula139/claude-code).

## Content Block Types

The Anthropic Messages API streams multiple content block types beyond `text` and `tool_use`. Claude Code handles all of them:

| Block type          | Delta type                           | Description                                                |
| ------------------- | ------------------------------------ | ---------------------------------------------------------- |
| `text`              | `text_delta`                         | Regular text output                                        |
| `tool_use`          | `input_json_delta`                   | Client-side tool call (accumulated JSON)                   |
| `server_tool_use`   | `input_json_delta`                   | Server-side tool call (same delta mechanism as `tool_use`) |
| `thinking`          | `thinking_delta` + `signature_delta` | Model reasoning (extended thinking)                        |
| `redacted_thinking` | (none)                               | Safety-redacted thinking (opaque, no content)              |

### Server Tool Use

Server tool use blocks stream identically to client `tool_use`: `input_json_delta` events accumulate the JSON input. The difference is execution, since the API runs server tools itself. Claude Code currently handles the `advisor` tool (internal).

## Thinking Configuration

Extended thinking is controlled by a `thinking` field in the request body:

```json
{
  "thinking": { "type": "enabled", "budget_tokens": 10000 }
}
```

Claude 4.6+ models support an `adaptive` mode where the API decides the budget:

```json
{
  "thinking": { "type": "adaptive" }
}
```

When thinking is enabled, `temperature` must be omitted from the request (API rejects it).

### Beta Headers

- `interleaved-thinking-2025-05-14`: Enables thinking blocks interleaved with text / tool_use.
- Without this header, thinking blocks appear only at the start of the response.

### Display modes (Opus 4.7+)

`thinking.display`, added in Opus 4.7, takes three wire values:

| Value          | Meaning                                                      |
| -------------- | ------------------------------------------------------------ |
| `"summarized"` | Thinking blocks stream summarized reasoning text.            |
| `"omitted"`    | Thinking blocks still ship but `thinking: ""` is empty.      |
| `"updates"`    | Only progress notes stream as text (Opus / Sonnet 5.5, beta) |

**Silent default change.** On Opus 4.6, the server defaulted to `"summarized"`. On Opus 4.7 the default is `"omitted"`, so any UI that renders streaming reasoning (including oxide-code's `show_thinking` TUI mode) sees a long pause followed by the final answer unless it opts back in:

```json
{
  "thinking": { "type": "adaptive", "display": "summarized" }
}
```

Older models (4.6, 4.5) accept the field and ignore it, so sending it unconditionally is safe when the caller wants summarized output.

Claude Opus 5.5 and Sonnet 5.5 return the notes the model writes between tool calls as progress-update thinking blocks, which are empty under `"omitted"`. Each note is its own thinking block directly before the tool call it introduces. A response that stops early (`max_tokens`, context window, stop sequence) can end with a note whose text is exactly `This part of the response was interrupted before it finished.`, and continuing means passing that turn back unchanged with a new user message.

oxide-code couples `display` to `config.show_thinking`: `"summarized"` when the TUI renders reasoning, otherwise `"updates"` on models that support it and no field on the rest.

`"summarized"` and `"omitted"` are GA since Opus 4.7, and `"updates"` needs the `thinking-display-updates-2026-08-18` beta.

## Thinking Block Lifecycle

### Streaming

1. `content_block_start` with `type: "thinking"` initializes with empty `thinking: ""` and `signature: ""`.
2. `content_block_delta` with `thinking_delta` appends to `thinking` text.
3. `content_block_delta` with `signature_delta` sets `signature` to the full complete value.
4. `content_block_stop` marks the block complete.

### Redacted Thinking

`redacted_thinking` blocks arrive as a single `content_block_start` with no deltas. They have no visible content and must be preserved verbatim for round-tripping.

### Round-Tripping

**Critical**: Thinking and redacted_thinking blocks must be included in the conversation history sent back to the API. Stripping them causes the API to reject subsequent requests or produce degraded responses.

Claude Code preserves these blocks through normalization in `normalizeMessagesForAPI()`, which runs a multi-pass pipeline before each API request:

1. `filterOrphanedThinkingOnlyMessages()`: Drops thinking-only assistant messages with no same-`message.id` partner carrying non-thinking content (handles resume / compaction artifacts).
2. `filterTrailingThinkingFromLastAssistant()`: Strips trailing thinking / redacted_thinking from the last assistant message. If stripping removes all content, inserts a `[No message content]` placeholder to preserve user / assistant alternation.
3. `filterWhitespaceOnlyAssistantMessages()`: Removes assistant messages with only whitespace text.
4. `ensureNonEmptyAssistantContent()`: Safety net for empty assistant messages.

Order matters: trailing thinking must be stripped before whitespace filtering, otherwise a message like `[text("\n\n"), thinking("...")]` survives the whitespace filter, then thinking stripping removes the thinking block, leaving `[text("\n\n")]` which the API rejects.

### Constraints

- **Trailing thinking**: A final assistant message must not end with a thinking block. Stripping can leave a thinking-only message empty, so a placeholder text block must be inserted to preserve user / assistant alternation. Deleting the message would create consecutive user messages, which the API rejects.
- **Credential rotation**: Signatures are cryptographically bound to the API key that generated them, so when credentials change (e.g., user logs in with a different account) all thinking and redacted_thinking blocks must be stripped from history. Old signatures become invalid and the API rejects them with 400.

## Signatures

Every `thinking` block includes a `signature` field received via `signature_delta`. Signatures are authentication markers that prove the thinking was genuinely generated under a specific API key. They are:

- Received as a full value (not incremental like text deltas).
- Stored alongside the thinking content.
- Validated by the API on subsequent requests.
- Invalidated when API credentials change.

Claude Code handles credential rotation in `stripSignatureBlocks()`, which removes all thinking / redacted_thinking blocks when the active credential changes.

oxide-code implements the full thinking data pipeline: typed `Thinking`, `RedactedThinking`, and `ServerToolUse` content blocks with streaming accumulation, signature handling, and round-trip preservation. Requests always end on a user message, so no trailing-thinking strip is needed. Adaptive thinking is enabled by default with `block_binding.prefix_mismatch_behavior = "drop_block"` (beta `thinking-binding-controls-2026-08-01`), so a change to the system prompt, the tools, or earlier messages drops the affected replayed blocks for that request. With `show_thinking` off, the chat renders each progress note as its tool use starts streaming, and a live or resumed turn that ends on the interrupted-work note asks the user to send a message. Credential-rotation stripping is tracked in [#230](https://github.com/hakula139/oxide-code/issues/230).

## Sources

- `claude-code/src/constants/betas.ts`: `INTERLEAVED_THINKING_BETA_HEADER`, `REDACT_THINKING_BETA_HEADER`
- `claude-code/src/services/api/claude.ts`: streaming handler, delta accumulation, request construction
- `claude-code/src/utils/messages.ts`: `normalizeMessagesForAPI`, `filterTrailingThinkingFromLastAssistant`, `filterOrphanedThinkingOnlyMessages`, `stripSignatureBlocks`
- `claude-code/src/utils/thinking.ts`: thinking config types, model support detection
