# Extended Thinking

Research notes on how Claude Code handles extended thinking, content block types, and signature verification. Based on [`claude-code`](https://github.com/hakula139/claude-code) (v2.1.87).

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

Server tool use blocks stream identically to client tool_use — `input_json_delta` events accumulate the JSON input. The difference is execution: server tools are handled by the API, not the client. Claude Code currently handles the `advisor` tool (internal).

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

- `interleaved-thinking-2025-05-14` — enables thinking blocks interleaved with text / tool_use.
- Without this header, thinking blocks appear only at the start of the response.

## Thinking Block Lifecycle

### Streaming

1. `content_block_start` with `type: "thinking"` — initialize with empty `thinking: ""` and `signature: ""`.
2. `content_block_delta` with `thinking_delta` — append to `thinking` text.
3. `content_block_delta` with `signature_delta` — set `signature` (full value, not incremental).
4. `content_block_stop` — block is complete.

### Redacted Thinking

`redacted_thinking` blocks arrive as a single `content_block_start` with no deltas — they have no visible content. They must be preserved verbatim for round-tripping.

### Round-Tripping

**Critical**: Thinking and redacted_thinking blocks must be included in the conversation history sent back to the API. Stripping them causes the API to reject subsequent requests or produce degraded responses.

Claude Code preserves these blocks through two normalization functions:

- `normalizeContentFromAPI()` — converts SDK response blocks into storable content.
- `normalizeMessagesForAPI()` — prepares stored messages for the next API request.

### Constraints

- **Trailing thinking**: Assistant messages must not end with a thinking block. Claude Code strips trailing thinking blocks before sending.
- **Credential rotation**: Signatures are cryptographically bound to the API key that generated them. When credentials change (e.g., user logs in with a different account), all thinking and redacted_thinking blocks must be stripped from the conversation history — their signatures are now invalid and the API will reject them with 400.

## Signatures

Every `thinking` block includes a `signature` field received via `signature_delta`. Signatures are authentication markers that prove the thinking was genuinely generated under a specific API key. They are:

- Received as a full value (not incremental like text deltas).
- Stored alongside the thinking content.
- Validated by the API on subsequent requests.
- Invalidated when API credentials change.

Claude Code handles credential rotation in `stripSignatureBlocks()`, which removes all thinking / redacted_thinking blocks when the active credential changes.

## Implementation Implications for oxide-code

To properly support extended thinking, oxide-code needs:

1. **New content block types**: `Thinking { thinking, signature }`, `RedactedThinking { data }`, `ServerToolUse { id, name, input }` in both `ContentBlockInfo` (streaming) and `ContentBlock` (message history).
2. **New delta types**: `ThinkingDelta { thinking }`, `SignatureDelta { signature }` in `Delta`.
3. **Block accumulators**: For thinking (text + signature) and server_tool_use (JSON, same as tool_use).
4. **Request parameter**: `thinking` field in `CreateMessageRequest`.
5. **Round-trip preservation**: Thinking / redacted_thinking blocks stored in `Message.content` and sent back in subsequent requests.
6. **Trailing thinking removal**: Strip thinking blocks from the end of assistant messages before sending.
7. **Credential rotation**: Strip all thinking / redacted_thinking blocks when OAuth credentials change.

The current `#[serde(other)] Unknown` approach prevents crashes but silently drops thinking blocks, which would break conversation continuity if thinking were enabled.

## Sources

- `claude-code/src/services/api/claude.ts` — streaming handler, delta accumulation, request construction
- `claude-code/src/utils/messages.ts` — `normalizeContentFromAPI`, `normalizeMessagesForAPI`, `stripSignatureBlocks`
- `claude-code/src/utils/thinking.ts` — thinking config types, model support detection
- `claude-code/src/constants/betas.ts` — `INTERLEAVED_THINKING_BETA_HEADER`, `REDACT_THINKING_BETA_HEADER`
