# Client

The client turns each conversation request into the wire format expected by the Anthropic API or a compatible gateway, owning headers, per-model beta sets, system and message prefixes, cache breakpoints, base-URL gating, and retry classification. [Authentication](../guide/authentication.md) covers credentials and gateways, and [Compaction](compaction.md) covers the summarizer reusing this path.

## Request Shape

Requests target `<base_url>/v1/messages?beta=true` with `anthropic-version: 2023-06-01`, `anthropic-beta`, `x-app: cli`, Stainless telemetry headers, `User-Agent: claude-cli/<version> (external, cli)`, and the credential header. The system prompt is an array of text blocks placing the attribution block and identity prefix in their own leading blocks before the prompt body ([System Prompt](prompt.md)), because the API validates the first non-attribution block.

## Beta Sets

Agentic turns on catalog models send the beta sequence captured from `claude -p` with an API key (including unused feature betas): Fable 5.1 reuses Opus 5.5's sequence, and Haiku 5.5 moves `claude-code-20250219` later in the list. Unknown model IDs send only `claude-code-20250219`, `oauth-2025-04-20` under OAuth, and `prompt-caching-scope-2026-01-05` (no effort, thinking, or context-management betas). Side queries (such as session titles) send at most `claude-code-20250219` and OAuth. Thinking betas derive directly from the request body's `thinking` field so headers and body cannot disagree.

## Thinking Display

Every request sends `thinking.display` explicitly because upstream defaults vary: `show_thinking` sets `"summarized"`, catalog models that emit progress notes otherwise ask for `"updates"` (adding `thinking-display-updates-2026-08-18`), and Haiku 5.5 asks for `"omitted"`. The stream keeps thinking text only when requested, and resume reads no progress notes from catalog models that emit none.

## Thinking Block Binding

Adaptive thinking requests set `thinking.block_binding` to `{"prefix_mismatch_behavior": "drop_block"}` with `thinking-binding-controls-2026-08-01`, dropping only affected replayed blocks after a system-prompt, tool, or earlier-message change.

The wire replays every Anthropic `thinking` and `redacted_thinking` block unchanged across model swaps and credential changes, letting the API drop unreadable blocks. Because requests end on a user message or a `pause_turn` reply that must replay verbatim, the client strips no trailing thinking blocks and ignores `input_transformations`.

## Cache Breakpoints

Requests set two ephemeral breakpoints using [`prompt_cache_ttl`](../guide/configuration.md#prompt_cache_ttl-cache-duration):

- **Static prefix.** Placed on the static system-prompt block, adding `scope: "global"` only on first-party hosts.
- **Conversation tail.** Placed unscoped on the last content block of the final message when `Request::cache_tail` is set (`conversation_request` sets it for turns and initial compaction summaries, while overflow retries clear it because their trimmed prefix is never extended).

Cache invalidation follows `tools` → `system` → `messages`, so prompt context is built once per session and tools and static system blocks stay stable across turns.

## Base-URL Gating

For prompt-caching scope, only the body-side `cache_control.scope: "global"` is host-gated: `api.anthropic.com` and `api-staging.anthropic.com` send it, while custom base URLs send `{"type": "ephemeral"}` (avoiding gateways that reject global scope after tool definitions). The `prompt-caching-scope-2026-01-05` header ships everywhere as a no-op without the body field.

Similarly, first-party requests include the per-request `cch` hash in the attribution block while custom base URLs omit it so body-caching gateways see a stable prefix. Stored logins go only to first-party hosts ([Authentication](../guide/authentication.md)).

## Retries

`with_retry` classifies failed attempts:

- **Retried.** Transport errors and HTTP 408, 409, 429, and 5xx retry up to five times with exponential backoff and jitter, overridden by `x-should-retry`.
- **Fatal.** Builder errors, other HTTP statuses, `retry-after` over 60 s (surfacing exhausted quotas promptly), and permanent credential errors fail immediately, while transient token-refresh failures retry.

## Design Decisions

1. **Send the conversation's captured beta sequence.** Matching Claude Code's sequence avoids rejections from gateways fingerprinting the client, pinned by a test tied to the captured CLI version.
2. **Two breakpoints with one TTL.** Separates the static prefix from the growing transcript tail, while capping tool output on return keeps prefix cache reads intact.
3. **Gate only the body-side scope on the base URL.** The header alone is a no-op, whereas the body field triggers gateway rejections.
4. **Classify before retrying.** Retrying malformed requests wastes a minute hiding the error, and keying on HTTP status and `x-should-retry` stays immune to error-text changes.

## Out of Scope

- **A different beta set for a gateway.** Set the applicable header and endpoint in config.

## Sources

- `crates/ox-core/src/client.rs`: `Request`, the conversation request a wire sends, and its `cache_tail` flag.
- `crates/ox-core/src/client/anthropic.rs`: `Client`, `CLAUDE_CLI_VERSION`, the request builder, and the identity and attribution blocks.
- `crates/ox-core/src/client/anthropic/betas.rs`: `compute_betas` and the beta header constants.
- `crates/ox-core/src/client/anthropic/billing.rs`: `compute_fingerprint`, `build_billing_header`, and `inject_cch`.
- `crates/ox-core/src/client/anthropic/transport.rs`: `with_retry`, `retry_delay`, and the failure classification.
- `crates/ox-core/src/client/anthropic/wire.rs`: `BlockBinding`, `CacheControl`, `static_prefix`, `conversation`, and `ThinkingConfig::asks_for_text`.
- `crates/ox-core/src/config.rs`: the prompt-cache TTL.
- `crates/ox-core/src/config/base_url.rs`: `BaseUrl::is_first_party`, the gate on the host-based cache scope.
