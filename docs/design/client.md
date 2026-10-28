# Client

The client turns one conversation request into the wire shape the Anthropic API or a compatible gateway expects. It owns the request headers, the per-model beta set, the system and message prefix, the cache breakpoints, base-URL gating, and retry classification. [Authentication](../guide/authentication.md) covers credentials and gateway settings, and [Compaction](compaction.md) covers the summarizer request that reuses this path.

## Request Shape

Every request goes to `<base_url>/v1/messages?beta=true` with `anthropic-version: 2023-06-01`, the `anthropic-beta` list, `x-app: cli`, the Stainless telemetry headers, a `User-Agent` of `claude-cli/<version> (external, cli)`, and the credential header. The system prompt is an array of text blocks, where the identity prefix and the attribution block each occupy their own block before the rest of the prompt, which [System Prompt](prompt.md) covers. The API validates the first non-attribution block, so folding the identity text into the body changes the request the server sees.

## Beta Sets

Agentic turns on the supported models send the sequence Claude Code sends for them, captured from `claude -p` with an API key, so it includes betas for features this client leaves unused. Fable 5.1, which that capture lacks, sends the Opus 5.5 sequence. Haiku 5.5 moves the `claude-code-20250219` tag later in the sequence. On other model IDs, an agentic turn sends `claude-code-20250219`, `oauth-2025-04-20` under OAuth, and `prompt-caching-scope-2026-01-05`, and adds no effort, thinking, or context-management betas. Side queries, such as session titles, carry at most the `claude-code-20250219` tag and the OAuth extension. The thinking betas derive from the request body's `thinking` field, so the header and the body cannot disagree.

## Thinking Display

Every request sends `thinking.display`, since an upstream's default can differ from the API's. `show_thinking` selects `"summarized"`, and otherwise the supported models that write progress notes ask for `"updates"`, which adds the `thinking-display-updates-2026-08-18` beta, while Haiku 5.5 asks for `"omitted"`. The stream keeps thinking text only when the request asked for it, so an upstream that sends text anyway shows none, and a resumed session reads no progress notes from the reasoning of a catalog model that writes none.

## Cache Breakpoints

A request carries two ephemeral breakpoints, both with the configured [`prompt_cache_ttl`](../guide/configuration.md#prompt_cache_ttl-cache-duration):

- **The static prefix.** The first breakpoint sits on the static system-prompt prefix block, and it carries `scope: "global"` only on a first-party base URL.
- **The conversation tail.** The second sits on the last content block of the final message without a scope, so each turn reads the previous turn's transcript from cache.

Invalidation follows the API's order, `tools` then `system` then `messages`, so the prompt context is built once per session and the tools and system blocks stay stable from turn to turn. The caller decides the second breakpoint through `Request::cache_tail`, which `conversation_request` sets for every conversation round. The first compaction summary request reuses that request, so it reads the conversation's cache. An overflow retry sends a trimmed prefix that no later request extends, so it clears `cache_tail`.

## Base-URL Gating

The body-side `cache_control.scope: "global"` is the one part of the request shape gated on the host. A base URL whose host is `api.anthropic.com` or `api-staging.anthropic.com` sends it, and every other host sends `{"type": "ephemeral"}` without the scope field. The `prompt-caching-scope-2026-01-05` header still ships everywhere, and without the body field it is a server-side no-op. The gate keeps requests valid on gateways that reject a global scope after tool definitions.

The attribution block follows the same split. A first-party request carries the per-request `cch` hash, while a custom base URL carries the block without it, so a gateway that caches on the request body sees a stable prefix. Stored logins are sent only to a first-party endpoint, as [Authentication](../guide/authentication.md) describes.

## Retries

`with_retry` classifies each failed attempt. A transport failure and a 408, 409, 429, or 5xx status retry with exponential backoff and jitter, up to five retries, while an `x-should-retry` header overrides the status rule. A server-requested `retry-after` longer than a minute fails at once so the user sees an exhausted quota promptly. A builder error and every other status are fatal. Failure to produce a credential is fatal unless a token refresh is transient enough that a later attempt may complete it.

## Design Decisions

1. **Send the conversation's captured beta sequence.** Reproducing Claude Code's sequence, including betas for features this client leaves unused, keeps a gateway that fingerprints the client from rejecting the request. A test pins the release the sequence was captured from, so a version bump fails until it is recaptured.

2. **Two breakpoints with one TTL.** The static prefix and the conversation tail are the two levels that change independently. Capping tool output when the tool returns keeps the prefix stable, so the breakpoint pays cache-read rates for most of the input.

3. **Gate only the body-side scope on the base URL.** The header alone is a server-side no-op, so sending it everywhere costs nothing, while the scope field is what a gateway can reject.

4. **Classify before retrying.** A malformed request cannot succeed on a later attempt, and retrying it hides the error behind a minute of backoff. The classification reads the status and the server's own hint, so reworded message text changes nothing.

## Out of Scope

- **A different beta set for a gateway.** Configure the applicable header and endpoint instead.

## Sources

- `crates/ox-core/src/client.rs`: `Request`, the conversation request a wire sends, and its `cache_tail` flag.
- `crates/ox-core/src/client/anthropic.rs`: `Client`, `CLAUDE_CLI_VERSION`, the request builder, and the identity and attribution blocks.
- `crates/ox-core/src/client/anthropic/betas.rs`: `compute_betas` and the beta header constants.
- `crates/ox-core/src/client/anthropic/transport.rs`: `with_retry`, `retry_delay`, and the failure classification.
- `crates/ox-core/src/client/anthropic/wire.rs`: `CacheControl`, `static_prefix`, `conversation`, and `ThinkingConfig::asks_for_text`.
- `crates/ox-core/src/config.rs`: the prompt-cache TTL.
- `crates/ox-core/src/config/base_url.rs`: `BaseUrl::is_first_party`, the gate on the host-based cache scope.
