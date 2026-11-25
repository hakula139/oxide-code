# Extended Thinking

Research notes on how Claude Code and the Anthropic Messages API handle extended thinking, content block types, and signature verification.

## Content Block Types

The Anthropic Messages API streams these content block types, all handled by Claude Code:

| Block type          | Delta type                           | Description                                                |
| ------------------- | ------------------------------------ | ---------------------------------------------------------- |
| `text`              | `text_delta`                         | Regular text output                                        |
| `tool_use`          | `input_json_delta`                   | Client-side tool call (accumulated JSON)                   |
| `server_tool_use`   | `input_json_delta`                   | Server-side tool call (same delta mechanism as `tool_use`) |
| `*_tool_result`     | (none)                               | Server-side tool result, such as `web_search_tool_result`  |
| `thinking`          | `thinking_delta` + `signature_delta` | Model reasoning (extended thinking)                        |
| `redacted_thinking` | (none)                               | Safety-redacted thinking (opaque, no content)              |

### Server Tool Use

Server tool calls stream with `input_json_delta` like client `tool_use`, while the API runs the tool itself and returns each result whole in one `content_block_start` (such as `web_search_tool_result` or `advisor_tool_result`) referencing its call via `tool_use_id`. Claude Code handles the internal `advisor` tool. In Claude Code 2.1.87, the streaming handler preserves unrecognized block types whole, and message normalization strips any `server_tool_use` whose result is missing from the same assistant message.

Anthropic's [server tools guide](https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools) (checked 2026-10-18) pairs calls and results by `tool_use_id` alone, so they can land in different replies. Two cases leave a call waiting for the next reply to open with its result: a `pause_turn` stop (continued by resending the paused reply as-is) and a server call made alongside client tool calls (run once their `tool_result` blocks arrive). Once later content closes the turn, an unpaired call returns HTTP 400 (such as `` `web_fetch` tool use with id ... was found without a corresponding `web_fetch_tool_result` block ``). Continuations must declare the waiting call's tool, and the [web search tool guide](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool) notes that a replayed result with missing or modified `encrypted_content` returns HTTP 400. [Persistence](../../design/persistence.md#compatibility) covers how oxide-code stores and replays server tool blocks.

## Thinking Configuration

The `thinking` request field controls thinking. Every supported model uses adaptive thinking, deciding how much to think while `output_config.effort` steers depth:

```json
{
  "thinking": { "type": "adaptive" }
}
```

Per Anthropic's [models overview](https://platform.claude.com/docs/en/about-claude/models/overview) (2026-10-17), manual `{"type": "enabled", "budget_tokens": N}` is rejected on the 5.5 models and Fable 5.1, and thinking is always on for Opus 5.5 and Fable 5.1.

### Beta Headers

Interleaved thinking (thinking blocks between tool calls) needs no beta header on supported models ([Client](../../design/client.md#beta-sets)).

### Display Modes

`thinking.display` accepts three wire values:

| Value          | Meaning                                                      |
| -------------- | ------------------------------------------------------------ |
| `"summarized"` | Thinking blocks stream summarized reasoning text             |
| `"omitted"`    | Thinking blocks ship with empty `thinking: ""`               |
| `"updates"`    | Only progress notes stream as text (all but Haiku 5.5, beta) |

Every supported model defaults to `"omitted"`, so a UI that renders streaming reasoning (such as oxide-code's `show_thinking` mode) sees a pause before the final answer unless it opts in:

```json
{
  "thinking": { "type": "adaptive", "display": "summarized" }
}
```

Claude Fable 5.1, Opus 5.5, and Sonnet 5.5 emit notes between tool calls as progress-update thinking blocks (empty under `"omitted"`), each placed immediately before the tool call it introduces. On Opus 5.5 and Sonnet 5.5, a response that stops early (`max_tokens`, context window, or stop sequence) can end with a note reading `This part of the response was interrupted before it finished.` Continue by passing that turn back unchanged with a new user message.

A gateway returned summarized Haiku 5.5 reasoning when `display` was unset (observed 2026-10-20 with oxide-code 0.1.0-alpha.5), so clients that want no reasoning text must send the field ([Client](../../design/client.md#thinking-display)).

`"summarized"` and `"omitted"` are GA. `"updates"` requires the `thinking-display-updates-2026-08-18` beta.

## Thinking Block Lifecycle

### Streaming

1. `content_block_start` with `type: "thinking"` initializes empty `thinking: ""` and `signature: ""`.
2. `content_block_delta` with `thinking_delta` appends to `thinking`.
3. `content_block_delta` with `signature_delta` sets `signature` to its complete value.
4. `content_block_stop` marks the block complete.

### Redacted Thinking

`redacted_thinking` blocks arrive in a single `content_block_start` with no deltas, carry no visible content, and must be kept verbatim for round-tripping.

### Round-Tripping

Conversation history sent back to the API must preserve `thinking` and `redacted_thinking` blocks, because stripping them causes request rejection or degraded responses.

Before each API request, Claude Code runs a four-pass message normalization pipeline:

1. **Drop orphaned thinking-only messages**: removes thinking-only assistant messages with no same-`message.id` partner carrying non-thinking content (handles resume / compaction artifacts).
2. **Strip trailing thinking from the last assistant message**: removes trailing `thinking` and `redacted_thinking` blocks, inserting a `[No message content]` placeholder if nothing remains to preserve user / assistant alternation.
3. **Drop whitespace-only assistant messages**: removes assistant messages containing only whitespace text.
4. **Ensure non-empty assistant content**: safety net for empty assistant messages.

Trailing thinking must be stripped before whitespace filtering. Otherwise a message like `[text("\n\n"), thinking("...")]` survives the whitespace filter and then loses its thinking block, leaving `[text("\n\n")]`, which the API rejects.

### Constraints

- **Trailing thinking**: A final assistant message must not end with a thinking block. Stripping can leave a thinking-only message empty, so Claude Code inserts a placeholder text block to keep the assistant turn. Deleting the message would merge surrounding user messages into one turn because the API [combines consecutive same-role turns](https://platform.claude.com/docs/en/api/messages/create).
- **Model switches**: [Switching models mid-conversation](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#switching-models) recommends resending thinking blocks unchanged, since the API drops or ignores blocks the target model cannot read. On the Claude API and Google Cloud only, Opus 5.5 reads Sonnet 5.5 and Haiku 5.5 blocks and Sonnet 5.5 reads Haiku 5.5 blocks. Fable 5.1 reads blocks from earlier models, and only Fable 5.1 and Mythos 5.1 read Fable 5.1 blocks ([What's new in Claude Fable 5.1](https://platform.claude.com/docs/en/models/fable-5-1/whats-new-fable-5-1), 2026-10-17). Dropped blocks are neither billed nor counted as input. Client-side stripping changes the messages every later block is bound to and invalidates those blocks too.
- **Account binding**: Per [account-bound thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking#account-bound-thinking), thinking blocks from Sonnet 5.5 and Haiku 5.5 work only in the producing account or a linked account (earlier models are unaffected). If another account sends them, the API drops them and the request succeeds. On the Claude API and Google Cloud, the `thinking-binding-controls-2026-08-01` beta lists each dropped block in `input_transformations` as a `thinking_dropped` entry with `reason: "organization_binding_mismatch"`. A signature that was tampered with or cannot be decrypted always returns 400 regardless of `prefix_mismatch_behavior`. Stripping thinking on credential changes therefore discards blocks the API would have kept (from the same account, a linked account, or unbound models).

## Signatures

Every `thinking` block carries a `signature` arriving whole in one `signature_delta`, stored beside the thinking text and validated by the API on replay. It verifies model readability ([model switches](#constraints)), unchanged `system`, `tools`, and earlier `messages`, and account binding on Sonnet 5.5 and Haiku 5.5 ([account binding](#constraints)).

Claude Code strips all `thinking` and `redacted_thinking` blocks whenever the `/login` dialog closes, even after a cancelled sign-in, discarding reasoning the API would keep or drop on its own.

[Client](../../design/client.md#thinking-block-binding), [Persistence](../../design/persistence.md#compatibility), and [Terminal UI](../../design/tui.md#compact-and-expanded-views) cover how oxide-code configures `block_binding`, replays thinking blocks across model and credential changes, and displays progress notes.
