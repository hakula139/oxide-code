# Extended Thinking

Research notes on how Claude Code handles extended thinking, content block types, and signature verification. Based on [Claude Code](https://github.com/hakula139/claude-code).

## Content Block Types

The Anthropic Messages API streams multiple content block types beyond `text` and `tool_use`. Claude Code handles all of them:

| Block type          | Delta type                           | Description                                                |
| ------------------- | ------------------------------------ | ---------------------------------------------------------- |
| `text`              | `text_delta`                         | Regular text output                                        |
| `tool_use`          | `input_json_delta`                   | Client-side tool call (accumulated JSON)                   |
| `server_tool_use`   | `input_json_delta`                   | Server-side tool call (same delta mechanism as `tool_use`) |
| `*_tool_result`     | (none)                               | Server-side tool result, such as `web_search_tool_result`  |
| `thinking`          | `thinking_delta` + `signature_delta` | Model reasoning (extended thinking)                        |
| `redacted_thinking` | (none)                               | Safety-redacted thinking (opaque, no content)              |

### Server Tool Use

Server tool use blocks stream identically to client `tool_use`: `input_json_delta` events accumulate the JSON input. The difference is execution, since the API runs server tools itself and returns each result in the reply as a block such as `web_search_tool_result` or `advisor_tool_result`, which names its call in `tool_use_id` and arrives whole in one `content_block_start`. Claude Code currently handles the `advisor` tool (internal). In the 2.1.87 source, its streaming handler keeps any block type it does not model whole, and `ensureToolResultPairing` strips a `server_tool_use` whose result is missing from the same assistant message.

Anthropic's [server tools guide](https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools), checked on 2026-10-18, pairs a call with its result by `tool_use_id` alone, so the two can land in different replies. A result usually follows its call in the same reply, but two cases leave the call waiting for the next reply to open with its result: a `pause_turn` stop, which the client continues by sending the paused reply back as-is, and a server call made alongside client tool calls, which the API runs once their `tool_result` blocks arrive. Once later content closes the turn, a call still without its result fails with a 400 such as `` `web_fetch` tool use with id ... was found without a corresponding `web_fetch_tool_result` block ``. A continuation must declare the waiting call's tool, and the [web search tool guide](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool) says a replayed result whose `encrypted_content` is missing or modified fails with a 400.

oxide-code stores each server tool result whole, with its API type and every other field, and sends it back as the API returned it. A streamed block of a type it does not model counts as a result when it carries a `tool_use_id`, so a new server tool whose call streams as `server_tool_use` needs no code change. The MCP connector's `mcp_tool_use` and `mcp_tool_result` blocks are out of scope, and the wire skips both. On resume, a `server_tool_use` survives only while some reply holds its result. A call that a `pause_turn` left waiting is dropped, because the next prompt would close the turn. oxide-code declares no server tools, so the API returns none of these blocks today.

## Thinking Configuration

The `thinking` field in the request body controls thinking. Every supported model uses adaptive thinking, deciding how much to think, while `output_config.effort` steers the depth:

```json
{
  "thinking": { "type": "adaptive" }
}
```

As of 2026-10-17, Anthropic's [models overview](https://platform.claude.com/docs/en/about-claude/models/overview) says the manual `{"type": "enabled", "budget_tokens": N}` mode is not accepted on the 5.5 models or Fable 5.1. Thinking is always on for Opus 5.5 and Fable 5.1.

### Beta Headers

On supported models, interleaved thinking, where thinking blocks appear between tool calls, needs no beta header. oxide-code still sends `interleaved-thinking-2025-05-14` because Claude Code's agentic beta sequence includes it.

### Display Modes

`thinking.display` accepts three wire values:

| Value          | Meaning                                                      |
| -------------- | ------------------------------------------------------------ |
| `"summarized"` | Thinking blocks stream summarized reasoning text.            |
| `"omitted"`    | Thinking blocks still ship but `thinking: ""` is empty.      |
| `"updates"`    | Only progress notes stream as text (all but Haiku 5.5, beta) |

Every supported model defaults to `"omitted"`. As a result, any UI that renders streaming reasoning, including oxide-code's `show_thinking` TUI mode, sees a long pause followed by the final answer unless it opts in:

```json
{
  "thinking": { "type": "adaptive", "display": "summarized" }
}
```

Claude Fable 5.1, Opus 5.5, and Sonnet 5.5 return the notes the model writes between tool calls as progress-update thinking blocks. These blocks are empty under `"omitted"`. Each note appears in its own thinking block immediately before the tool call it introduces. On Opus 5.5 and Sonnet 5.5, a response that stops early (`max_tokens`, context window, stop sequence) can end with a note whose text is exactly `This part of the response was interrupted before it finished.` To continue, pass that turn back unchanged with a new user message.

A gateway returned summarized Haiku 5.5 reasoning to requests that left `display` unset (observed 2026-10-20 with oxide-code 0.1.0-alpha.5), so a client that wants no reasoning text has to send the field. [Client](../../design/client.md#thinking-display) covers how oxide-code picks the mode.

`"summarized"` and `"omitted"` are GA. `"updates"` requires the `thinking-display-updates-2026-08-18` beta.

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

- **Trailing thinking**: A final assistant message must not end with a thinking block. Stripping can leave a thinking-only message empty, and Claude Code inserts a placeholder text block to keep the assistant turn. Deleting the message would merge the user messages around it into one turn, since the API [combines consecutive same-role turns](https://platform.claude.com/docs/en/api/messages/create).
- **Model switches**: [Switching models mid-conversation](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models) asks clients to keep passing thinking blocks back unchanged, since the API drops or ignores the blocks the request's model can't read. Opus 5.5 reads Sonnet 5.5 and Haiku 5.5 blocks, and Sonnet 5.5 reads Haiku 5.5 blocks, only on the Claude API and Google Cloud. Fable 5.1 reads blocks from earlier models, and only Fable 5.1 and Mythos 5.1 read its blocks, as [What's new in Claude Fable 5.1](https://platform.claude.com/docs/en/models/fable-5-1/whats-new-fable-5-1) described on 2026-10-17. A dropped block is neither billed nor counted as input. The docs suggest stripping prior blocks only to save input tokens on the models that ignore them, but a client-side strip changes the messages that every later block is bound to, which invalidates those blocks too.
- **Account binding**: Thinking blocks from Sonnet 5.5 and Haiku 5.5 work only in the account that produced them or an account linked to it, as the guide on [account-bound thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#account-bound-thinking) describes, and blocks from earlier models aren't affected. When another account sends one of these blocks, the API drops it before the model sees it, and the request succeeds. On the Claude API and Google Cloud, with the `thinking-binding-controls-2026-08-01` beta, the response lists each dropped block in `input_transformations` as a `thinking_dropped` entry with `reason: "organization_binding_mismatch"`. A signature that was tampered with or cannot be decrypted is a separate failure, which always gets a 400 whatever `prefix_mismatch_behavior` says. A client that strips thinking when the credential changes therefore loses the blocks the API would have kept: those sent back from the same or a linked account, and those from models that aren't bound to an account.

## Signatures

Every `thinking` block includes a `signature` field received via `signature_delta`. The signature lets the API check that the request's model can read the block, as [model switches](#constraints) describes, and that the `system`, `tools`, and earlier `messages` are unchanged since the block was produced. Signatures are:

- Received as a full value (not incremental like text deltas).
- Stored alongside the thinking content.
- Validated by the API on subsequent requests.
- Bound to the account that produced them for Sonnet 5.5 and Haiku 5.5 blocks, as [account binding](#constraints) describes.

Claude Code's `stripSignatureBlocks()` removes all thinking / redacted_thinking blocks whenever the `/login` dialog closes, even after a cancelled sign-in, which discards reasoning that the API would keep or drop on its own.

oxide-code implements the thinking data pipeline: typed `Thinking`, `RedactedThinking`, `ServerToolUse`, and `ServerToolResult` content blocks with streaming accumulation, signature handling, and round-trip preservation. Each reasoning block records the wire and model that produced it, and the Anthropic wire sends every Anthropic block back unchanged on every platform, whichever model produced it. A request ends on a user message, or on a paused reply that a `pause_turn` continuation must send back as-is, so oxide-code strips no trailing thinking. Adaptive thinking is enabled by default with `block_binding.prefix_mismatch_behavior = "drop_block"` (beta `thinking-binding-controls-2026-08-01`), so a change to the system prompt, the tools, or earlier messages drops the affected replayed blocks for that request. With `show_thinking` off, the chat renders each progress note as its tool use starts streaming, and a live or resumed turn that ends on the interrupted-work note asks the user to send a message.

oxide-code strips nothing when the credential changes, as on a resume under another API key or a `/login` to another account, since the API drops the blocks bound to another account itself. oxide-code does not read `input_transformations`, so such a drop shows only in the API response.

## Sources

- `claude-code/src/commands/login/login.tsx`: `stripSignatureBlocks` when the `/login` dialog closes
- `claude-code/src/constants/betas.ts`: `INTERLEAVED_THINKING_BETA_HEADER`, `REDACT_THINKING_BETA_HEADER`
- `claude-code/src/services/api/claude.ts`: streaming handler, delta accumulation, request construction
- `claude-code/src/utils/messages.ts`: `normalizeMessagesForAPI`, `filterTrailingThinkingFromLastAssistant`, `filterOrphanedThinkingOnlyMessages`, `stripSignatureBlocks`, `ensureToolResultPairing`
- `claude-code/src/utils/thinking.ts`: thinking config types, model support detection
