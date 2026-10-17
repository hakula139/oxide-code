# Anthropic API Authentication

Research notes on how to authenticate with the Anthropic Messages API using OAuth tokens from Claude Code. These findings are based on reverse-engineering [Claude Code](https://github.com/hakula139/claude-code) and testing against the production API.

## Authentication Methods

### API Key

Standard approach. Set `x-api-key` header directly.

### OAuth (Claude Code Credentials)

Claude Code stores OAuth tokens in **platform-specific secure storage** with a plaintext fallback:

- **macOS**: macOS Keychain (service `"Claude Code-credentials"`), falling back to `~/.claude/.credentials.json`.
- **Linux**: `~/.claude/.credentials.json` (plaintext only).

Both backends store the same JSON structure:

```json
{
  "claudeAiOauth": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": 1234567890000,
    "scopes": ["user:inference", "user:profile", "..."],
    "subscriptionType": "team",
    "rateLimitTier": "default_claude_max_5x"
  }
}
```

**Important**: On macOS, the Keychain and the file can hold **different tokens**. Claude Code reads from the Keychain first. If an external tool refreshes the token via the file only (without updating the Keychain), the file token becomes stale while the Keychain token remains valid. Token consumers on macOS must read from the Keychain to get the canonical token.

OAuth requests use `Authorization: Bearer <token>` (not `x-api-key`).

### Long-lived OAuth token

`claude setup-token` issues a long-lived subscription token. Claude Code reads it from `CLAUDE_CODE_OAUTH_TOKEN`, sends it with the same OAuth request shape as a stored login, and never refreshes it. It ranks below `ANTHROPIC_API_KEY` and above the stored login ([authentication precedence](https://code.claude.com/docs/en/authentication#authentication-precedence)).

### Auth token (LLM gateways)

`ANTHROPIC_AUTH_TOKEN` sends `Authorization: Bearer <token>` with the API-key request shape, so the `oauth-2025-04-20` beta stays off. It ranks above every other credential. Gateways may also require extra headers, which Claude Code reads from `ANTHROPIC_CUSTOM_HEADERS` as one `Name: value` per line.

## Required Headers and Parameters

Four things make up the canonical Claude Code request shape. The first two are OAuth-load-bearing because their absence triggers 401 / 429 against `api.anthropic.com` on non-Haiku models (see [What Happens Without These](#what-happens-without-these)). The latter two ship on every request. A third-party relay gateway checked more than the client identity headers (see [Third-Party Gateway Validation](#third-party-gateway-validation)).

### 1. Beta headers

```text
anthropic-beta: claude-code-20250219,oauth-2025-04-20
```

- `claude-code-20250219`: Identifies the request as a Claude Code client.
- `oauth-2025-04-20`: Enables OAuth authentication.

Additional useful betas:

| Header                            | Purpose                                                      |
| --------------------------------- | ------------------------------------------------------------ |
| `interleaved-thinking-2025-05-14` | Extended thinking support                                    |
| `context-1m-2025-08-07`           | 1M context window                                            |
| `context-management-2025-06-27`   | Context management                                           |
| `prompt-caching-scope-2026-01-05` | Prompt caching                                               |
| `effort-2025-11-24`               | Effort control                                               |
| `structured-outputs-2025-12-15`   | JSON-schema-constrained responses (one-shot calls)           |
| `advanced-tool-use-2025-11-20`    | Tool search (first-party only)                               |
| `task-budgets-2026-03-13`         | Advisory token budget across an agentic loop (Opus 4.7 only) |

#### Per-model beta sets

The accepted beta set differs per model family and call type (agentic chat vs one-shot utility). Claude Code applies this mapping in `claude-code/src/utils/betas.ts`:

Rows are grouped by role: identity / auth → universal agentic → model-tier-gated. Within each group the broadest support comes first.

Cell legend: `✓` always on, `-` not supported (or stripped), `[1m]` opt-in via the model suffix, `*` caller opt-in (body field + beta ship together, see rules below).

| Beta                              | Opus 4 (base) | Opus 4.1 / 4.5 | Opus 4.6+ | Sonnet 4 (base) | Sonnet 4.5 | Sonnet 4.6+ | Haiku 4 (base) | Haiku 4.5 (agentic) | Haiku 4.5 (one-shot) |
| --------------------------------- | ------------- | -------------- | --------- | --------------- | ---------- | ----------- | -------------- | ------------------- | -------------------- |
| `claude-code-20250219`            | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | -                    |
| `oauth-2025-04-20` (OAuth only)   | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | ✓                    |
| `context-management-2025-06-27`   | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | -                    |
| `prompt-caching-scope-2026-01-05` | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | -                    |
| `interleaved-thinking-2025-05-14` | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | -              | -                   | -                    |
| `context-1m-2025-08-07`           | -             | -              | `[1m]`    | `[1m]`          | `[1m]`     | `[1m]`      | -              | -                   | -                    |
| `effort-2025-11-24`               | -             | -              | ✓         | -               | -          | ✓           | -              | -                   | -                    |
| `structured-outputs-2025-12-15`   | -             | `*`            | `*`       | -               | `*`        | `*`         | -              | `*`                 | `*`                  |

Claude Code applies these rules to 4.x models:

- **Haiku + `context-1m`**: Rejected because Haiku has a 200K window. Claude Code silently strips the `[1m]` tag before sending.
- **Haiku one-shots** (title generation, compaction classifier): Strip agentic markers entirely. `claude-code-20250219` is re-added only when the call is agentic.
- **`prompt-caching-scope` ships unconditionally.** The header alone is a server-side no-op without a matching `cache_control.scope` field.
- **`context-1m` is user opt-in via `[1m]`.** Appending `[1m]` to the model string (e.g., `claude-opus-4-8[1m]`) adds the 1M beta and strips the tag before the request hits the wire. Family-based auto-enable would 400 on subscriptions that do not carry 1M access.
- **`effort` is Opus 4.6+ and Sonnet 4.6+ only.** Opus 4.5 and older, Sonnet 4.5 and older, and all Haiku variants reject it per upstream's `modelSupportsEffort`.
- **`effort` and `context-management` betas need a body field.** Sending the header alone is a silent no-op: the request runs at the server default. See [Agentic Request Body Fields](#agentic-request-body-fields) for the matching `output_config.effort` and `context_management.edits` shapes.
- **`structured-outputs` is per-version and caller-opt-in.** The upstream allowlist is Opus 4.1+, Sonnet 4.5+, Haiku 4.5. Claude Code ships the beta only when a caller supplies an `output_config.format`.

On agentic Opus 5.5 and Sonnet 5.5 requests, Claude Code 2.1.296 sends `claude-code-20250219, interleaved-thinking-2025-05-14, thinking-token-count-2026-05-13, context-management-2025-06-27, prompt-caching-scope-2026-01-05, mid-conversation-system-2026-04-07, per-turn-control-2026-07-01, advisor-tool-2026-03-01, effort-2025-11-24`. On Haiku 5.5 it moves `claude-code-20250219` to just after `mid-conversation-system-2026-04-07`. Both sequences were captured from `claude --bare -p` with an API key on 2026-10-15. Under OAuth, the frozen 2.1.87 source puts `oauth-2025-04-20` right after the leading `claude-code-20250219`, or first on Haiku. That slot was not captured.

This client supports only the Claude 5.5 models, where effort, interleaved thinking, the 1M window, and structured outputs are GA. On their agentic turns, `client::anthropic::compute_betas` sends the sequence above, which includes betas for features this client leaves unused. Its test pins `CLAUDE_CLI_VERSION`, the release the user agent and billing block report, so a version bump fails until the sequence is recaptured. It then appends `thinking-binding-controls-2026-08-01`, plus `thinking-display-updates-2026-08-18` when the request sets `thinking.display = "updates"`. Agentic turns on other model ids carry `claude-code-20250219`, `oauth-2025-04-20` (OAuth only), and `prompt-caching-scope-2026-01-05`, and go without effort, thinking, or context management. One-shots carry `claude-code-20250219` (skipped on Haiku) and `oauth-2025-04-20` (OAuth only). The body-side `cache_control.scope: "global"` stays gated on the base URL, as [Prompt Caching Scope](#prompt-caching-scope) describes.

### 2. System prompt prefix (as a separate block)

The `system` parameter must be sent as an **array of text blocks**. A plain string is rejected, even with the same content. The identity prefix must occupy its own block:

```json
"system": [
  {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
  {"type": "text", "text": "...rest of prompt..."}
]
```

The API validates that the **first non-attribution text block** matches one of the known prefix values:

- `"You are Claude Code, Anthropic's official CLI for Claude."`
- `"You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK."`
- `"You are a Claude agent, built on Anthropic's Claude Agent SDK."`

**Critical**: Concatenating the prefix into the prompt body as a single string causes the API to reject OAuth requests with 429, even though the same prefix content is present. The block-level separation is what the server checks.

### 3. Attribution header

Claude Code prepends an attribution header as the very first system block:

```json
{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.121.91c; cc_entrypoint=cli; cch=8f81f;"}
```

Format:

```text
x-anthropic-billing-header: cc_version=<VERSION>.<FINGERPRINT>; cc_entrypoint=<ENTRYPOINT>; cch=<HASH>;
```

#### Fingerprint (3-char version suffix)

A 3-character hex value derived from conversation content:

1. Select the first user message that is not meta, and take its string content or its first text block, or `""` when it has neither. Claude Code 2.1.301 selects from the messages before normalization merges adjacent user messages, so it skips the context message, which is meta, and reads the user's first prompt. 2.1.87 took the first user message after normalization, which began with the context message. oxide-code skips the user context it prepends and reads the first user message of the transcript.
2. Extract the characters at indices `[4, 7, 20]` of that text, using `"0"` for an index out of bounds. Indices count UTF-16 code units. An index inside a surrogate pair picks a lone surrogate, which the hash input's UTF-8 encoding turns into U+FFFD under both Bun and Node.
3. Compute `SHA256(SALT + chars + VERSION)`, take the first 3 hex characters.
4. Salt: `59cf53e54c78` (hardcoded in `claude-code/src/utils/fingerprint.ts`, must match server).

The entrypoint is `cli` for interactive sessions.

#### cch (5-char request integrity hash)

The `cch` field is a request integrity hash used for feature gating (fast mode) and billing attribution. It was reverse-engineered from Anthropic's custom Bun binary in February 2026 ([a10k.co writeup](https://a10k.co/b/reverse-engineering-claude-code-cch.html)).

How it works:

1. The JavaScript layer writes a `cch=00000` placeholder into the billing header (controlled by `feature('NATIVE_CLIENT_ATTESTATION')` compile-time flag).
2. The request body is serialized to JSON with the placeholder in place.
3. Bun's native HTTP stack (compiled Zig, `bun-anthropic/src/http/Attestation.zig`) intercepts the `fetch()` call, detects the placeholder, and computes `xxHash64(body_bytes, seed) & 0xFFFFF`.
4. The five `0` characters are overwritten in-place with the 5-char hex result before sending.

Constants:

- **Seed**: `0x6E52736AC806831E` (64-bit, embedded in the binary's data section).
- **Mask**: `& 0xFFFFF` (20 bits → 5 hex chars, zero-padded).

The hash covers the entire serialized body (messages, tools, metadata, model, thinking config). The only safe post-hash modification is to non-billing system blocks, which the server excludes from its verification.

**JSON key ordering matters**: `system` must be serialized before `messages` so the placeholder in the billing header appears first in the JSON. If tool results contain the literal `cch=00000`, serializing `messages` first would cause the replacement to hit the wrong occurrence.

#### Known bug: cch substitution breaks prompt cache

The Bun binary performs a global find-and-replace of `cch=` values across the entire serialized request body, including historical tool results. If any tool result in the conversation contains a `cch=XXXXX` string (e.g., from reading proxy logs or session JSONL files), the substitution rewrites those historical bytes on every turn, changing the conversation prefix and permanently invalidating the prompt cache. This wastes 30-50K+ tokens per turn and never self-heals. Tracked as [anthropics/claude-code#40652](https://github.com/anthropics/claude-code/issues/40652), partially mitigated in v2.1.90-91.

oxide-code avoids this entirely: we serialize with `serde_json`, replace only the first occurrence via `str::replacen`, and never mutate historical message content.

### 4. Client identity headers

```text
User-Agent: claude-cli/<version> (external, cli)
x-app: cli
```

The `User-Agent` must start with `claude-cli/`. Claude Code constructs it as `claude-cli/<version> (<user_type>, <entrypoint>)` where `user_type` is `external` (or `ant` for Anthropic employees) and `entrypoint` is `cli`.

The Stainless TS SDK also emits `x-stainless-lang`, `-package-version`, `-runtime`, `-runtime-version`, `-os`, `-arch`, `-timeout`, and `-retry-count`, and oxide-code sends the same set.

## What Happens Without These

| Missing                  | Haiku 4.5 | Sonnet / Opus |
| ------------------------ | --------- | ------------- |
| `claude-code-20250219`   | 200       | 429           |
| `oauth-2025-04-20`       | 401       | 401           |
| Prefix as separate block | 200       | 429           |
| Prefix in body string    | 200       | 429           |

The last two rows are the critical distinction: having the prefix present in a concatenated string is **not sufficient**. It must be a separate `{"type": "text", "text": "..."}` block in the system array.

## Prompt Caching Scope

Blocks in the `system` array can carry `cache_control` for prompt caching. The `scope` field controls the sharing level:

| Value      | Shape                                      | Shared across             |
| ---------- | ------------------------------------------ | ------------------------- |
| `global`   | `{"type": "ephemeral", "scope": "global"}` | All users on the 1P API   |
| _(absent)_ | `{"type": "ephemeral"}`                    | The caller's organization |
| `null`     | no `cache_control` at all                  | _(not cached)_            |

### Prefix invariance

`scope: "global"` is only valid when **every preceding request element is also globally scoped or unscoped**. The order the server sees is:

```text
[tool definitions] → [system blocks...] → [messages...]
```

Tool definitions render before system blocks. If any earlier block carries a narrower cache scope, the server rejects the global block with HTTP 400:

> `cache_control.scope: "global"` is only valid when every preceding block is also globally scoped. A block with `scope: "global"` was found after content with a narrower cache scope.

The Anthropic API accepts `scope: "global"` on the static system block even when tools are present.

### oxide-code gating

oxide-code ships `prompt-caching-scope-2026-01-05` on every agentic request, as Claude Code does, and gates only the body-side `cache_control.scope` on `is_first_party_base_url(&config.base_url)`:

- Base URL host matches `api.anthropic.com` or `api-staging.anthropic.com` → `{"type": "ephemeral", "scope": "global"}`.
- Any other host (gateways, self-hosted, malformed URLs) → `{"type": "ephemeral"}`. The header still ships, and without the scope field it is a server-side no-op.

Shape is otherwise identical in both modes. Only the body-side `scope` field toggles.

Same pattern as fine-grained tool streaming and client-request-ID injection: gate on the base URL, since the provider flag only says "not Bedrock / not Vertex" and a user pointing `ANTHROPIC_BASE_URL` at a gateway still parses as first-party under that check.

## Agentic Request Body Fields

Some capabilities live in the request body alongside the `anthropic-beta` header that gates them. Shipping the header but omitting the body field is a silent no-op because the feature does not activate. All three fields below were captured live from Claude Code 2.1.119 and cross-checked against the official migration guide.

### `output_config.effort`

GA as of Opus 4.6. Controls the intelligence-vs-latency tier of agentic turns via one of five tokens: `low`, `medium`, `high`, `xhigh`, `max`.

```json
{
  "output_config": { "effort": "xhigh" }
}
```

- **The `effort-2025-11-24` beta header is necessary but not sufficient.** The header alone is a no-op, so the model runs at its default effort until the body carries `output_config.effort`.
- **Per-model ceiling.** `max` is Opus-only, and Sonnet 4.6 400s on it. `xhigh` is Opus 4.7 / 4.8-only. Every Claude 5.5 model accepts all five.
- **Per-model default.** On the 4.x models the raw API default was `high` for every effort-capable model (equivalent to omitting the field), and Claude Code bumped Opus 4.7 to `xhigh`. The Claude 5.5 API defaults are `medium` on Opus and Haiku and `high` on Sonnet, and oxide-code sends them explicitly.
- **`max_tokens` should scale with effort.** Claude Code uses 64 K on Opus 4.7 at `xhigh`, 32 K on Sonnet 4.6 at `high`. oxide-code defaults to 64 K on the supported models, since effort already steers thinking depth, and to 16 K on other ids.

### `context_management.edits`

Partners the `context-management-2025-06-27` beta header. Claude Code ships the same directive on every 4.6+ request:

```json
{
  "context_management": {
    "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
  }
}
```

oxide-code applies the body-header coupling as an invariant: the body field is populated on every request whose model is in the supported catalogue, the same condition that enables the beta header. One-shot completions (the `complete` path in `client::anthropic`) skip both, matching the reference wire and keeping the title-generation path minimal.

### `cache_control.ttl`

Anthropic silently dropped the default ephemeral-cache TTL from 1 h to 5 m on 2026-03-06, a 40-55 % savings regression on any session longer than 5 min. The opt-in uses a body field:

```json
{
  "cache_control": { "type": "ephemeral", "ttl": "1h" }
}
```

Accepted values: `"5m"` (server default, equivalent to omitting the field) and `"1h"` (opt-in at higher write premium). No beta header is required because the field is GA.

**oxide-code default.** `prompt_cache_ttl = "1h"`. The hit-rate recovery on real agent sessions (tool-use loops, resumed conversations) dominates the write premium, so 1 h is the right safe default. Users opt down via `[client].prompt_cache_ttl = "5m"` or `OX_PROMPT_CACHE_TTL=5m`.

Invalidation order (from the Anthropic caching docs) is `tools → system → messages`: any change at a level busts that level and every level after it. oxide-code sends two breakpoints with the configured TTL. The first sits on the static system-prompt prefix block (scope-gated on 1 P / 3 P per the previous section). The second sits on the last content block of the final message, without `scope`, so each turn reads the previous turn's transcript from cache. The prompt context is built once per session, which keeps that prefix stable from turn to turn. Compaction summaries are one-off requests that no later request extends, so they skip the second breakpoint.

### `thinking.display`

See [Extended Thinking § Display modes (Opus 4.7+)](extended-thinking.md#display-modes-opus-47). Opus 4.7 silently flipped the default to `"omitted"`. `show_thinking=true` in oxide-code opts back into `"summarized"`. With it off, Opus 5.5 and Sonnet 5.5 requests use `"updates"`.

## Third-Party Tool Restrictions

As of April 4, 2026, Anthropic enforces that OAuth subscription credits (Pro / Max) are only valid for official Claude Code and claude.ai clients. Third-party tools that reuse the OAuth flow are classified as "third-party harness traffic" and must use either:

- **API key** (`ANTHROPIC_API_KEY`) with standard per-token billing.
- **Extra Usage** billing enabled on the account, which allows OAuth but bills per-token beyond the subscription.

The `cch` hash is the primary technical enforcement mechanism, and both the algorithm (xxHash64, non-cryptographic) and the constants are publicly known. No additional protections exist: no TLS fingerprinting, binary attestation, pre-registration handshake, replay detection, or connection association. Anthropic could escalate enforcement at any time, but today's scheme reads as billing plumbing with no security boundary behind it.

oxide-code attaches the attestation block to every streaming request (agent turns and `/compact` summaries) under every auth mode, and to non-streaming one-shots (session titles) under OAuth. `CLAUDE_CODE_ATTRIBUTION_HEADER=0` drops it everywhere. Claude Code emits the block under both auth modes. Requests to the Anthropic API carry a valid `cch` hash. Since 2.1.181, Claude Code sends custom base URLs the block without `cch`, so a gateway that caches on the request body sees a stable prefix, and oxide-code matches that. The fingerprint salt and xxHash64 seed are version-specific constants, so they may change with Claude Code releases. The 2.1.301 bundle keeps the same salt and character indices and changes the message selection, as [Fingerprint](#fingerprint-3-char-version-suffix) describes. Its seed lives in the native Bun HTTP stack, outside the JavaScript bundle, and a scan of the 2.1.301 binary did not find it ([#123](https://github.com/hakula139/oxide-code/issues/123#issuecomment-6210505896)). On 2026-10-16, the Anthropic API accepted oxide-code's requests carrying a `cch` computed with the seed above, which shows that the API accepts the hash but not that it verifies it.

## Third-Party Gateway Validation

On 2026-10-17, a third-party relay gateway accepted an API-key request from an `ox -p` build of `main`, which mirrors Claude Code 2.1.296. The request carried the `claude-cli/2.1.296 (external, cli)` user agent, the billing block, a static system block of about 7.2k characters, and the user context in the first user message. Hand-built requests to `/v1/messages` with the same user agent and `x-app: cli` but no billing block got HTTP 400 `invalid_request_error` with "Please use authentic Claude Code client or upgrade your version". That happened with system blocks of 0, 2000, 3500, and 8000 characters, with and without the identity block. The gateway checks more of the request than its user agent, and the hint to upgrade misleads, since it accepted the same version from oxide-code. With every hand-built request rejected, a system-block size limit can't be probed outside a real client request.

The same gateway answered an unsupported model name with HTTP 404 and the plain-text body `404 page not found`, so the error looks like a wrong base URL.

## API Version

The `anthropic-version` header is `2023-06-01` across all Claude Code endpoints. This is the only stable API version.

## Model IDs

The API model ID for Opus 4.6 is `claude-opus-4-6`. The `[1m]` suffix (e.g., `claude-opus-4-6[1m]`) is a client-side convention that Claude Code strips before sending to the API via `normalizeModelStringForAPI()`. The `context-1m-2025-08-07` beta header activates the 1M context window.

## SDK vs Raw HTTP

Claude Code uses the Anthropic TypeScript SDK with `authToken` (not `apiKey`) for OAuth. The SDK:

- Sends `Authorization: Bearer <token>` internally.
- Adds `?beta=true` query parameter to the URL.
- Includes `x-stainless-*` headers (SDK telemetry).
- Retries on `x-should-retry: true` responses with exponential backoff.

For raw HTTP (as in oxide-code), replicate the headers manually. oxide-code mirrors `?beta=true` and the Stainless headers.

## Token Refresh

OAuth tokens expire (check `expiresAt` in milliseconds). Claude Code refreshes them automatically with a 5-minute buffer before expiry, using a `POST` to `platform.claude.com/v1/oauth/token` with the `refresh_token`. Cross-process safety is handled via directory-based locking (`proper-lockfile` creates a `~/.claude.lock/` directory).

oxide-code implements the same refresh flow: proactive refresh with the 5-minute buffer, directory-based locking compatible with Claude Code, and credential write-back preserving unknown fields.

## Sources

- `claude-code/src/constants/betas.ts`: beta header constants
- `claude-code/src/constants/oauth.ts`: OAuth client ID, token URL, scopes
- `claude-code/src/constants/system.ts`: system prompt prefix, attribution header construction
- `claude-code/src/services/api/claude.ts`: system block assembly, `buildSystemPromptBlocks`
- `claude-code/src/services/api/client.ts`: SDK client construction
- `claude-code/src/services/oauth/client.ts`: token refresh endpoint and request format
- `claude-code/src/utils/api.ts`: `splitSysPromptPrefix`, cache scope assignment
- `claude-code/src/utils/auth.ts`: OAuth token retrieval and refresh
- `claude-code/src/utils/betas.ts`: per-model beta header computation
- `claude-code/src/utils/fingerprint.ts`: 3-char SHA-256 fingerprint (salt, indices, computation)
- `claude-code/src/utils/http.ts`: auth headers, User-Agent construction
- `claude-code/src/utils/userAgent.ts`: `claude-cli/<version>` User-Agent format
- `claude-code/src/utils/secureStorage/index.ts`: platform-specific storage dispatch
- `claude-code/src/utils/secureStorage/macOsKeychainStorage.ts`: macOS Keychain backend
- `claude-code/src/utils/secureStorage/plainTextStorage.ts`: credential file I/O

Body-field research is empirical. The `output_config.effort`, `context_management.edits`, and `cache_control.ttl` wire shapes documented above were captured live from a `claude-code --bare -p --model claude-opus-4-7` session against a local SSE proxy on 2026-04-24 and cross-referenced with the [Opus 4.7 migration guide](https://platform.claude.com/docs/en/about-claude/models/migration-guide) and [Anthropic prompt-caching docs](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching).
