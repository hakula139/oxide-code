# Anthropic API Authentication

Research notes on authenticating with and calling the Anthropic Messages API using Claude Code's credentials and request shape. Unqualified Claude Code behaviors reflect version 2.1.87 unless a later release or external reference is noted.

## Authentication Methods

### API Key

Standard authentication sets the `x-api-key` header.

### OAuth (Claude Code Credentials)

Claude Code stores OAuth tokens in platform-specific storage with a plaintext fallback:

- **macOS**: Keychain (service `"Claude Code-credentials"`), falling back to `~/.claude/.credentials.json`.
- **Linux**: `~/.claude/.credentials.json` only.

`CLAUDE_CONFIG_DIR` moves the config directory and credential file away from `~/.claude`. Claude Code reads it in Unicode NFC, and when it is set, the macOS Keychain service becomes `Claude Code-credentials-<hash>`, where `<hash>` is the first 8 hex digits of the SHA-256 of its value.

Both backends store the same JSON:

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

On macOS, the Keychain and the file can diverge. Claude Code reads the Keychain first, so refreshing only the file leaves the file token stale, and macOS consumers must read the Keychain for the canonical token.

OAuth requests send `Authorization: Bearer <token>` in place of `x-api-key`.

### Long-Lived OAuth Token

`claude setup-token` issues a long-lived subscription token that Claude Code reads from `CLAUDE_CODE_OAUTH_TOKEN`, sends with the OAuth request shape, and never refreshes. It ranks below `ANTHROPIC_API_KEY` and above the stored login ([authentication precedence](https://code.claude.com/docs/en/authentication#authentication-precedence)).

### Auth Token (LLM Gateways)

`ANTHROPIC_AUTH_TOKEN` sends `Authorization: Bearer <token>` with the API-key request shape (keeping `oauth-2025-04-20` off) and ranks above every other credential. Gateways may also require extra headers, which Claude Code reads from `ANTHROPIC_CUSTOM_HEADERS` as one `Name: value` per line.

## Required Headers and Parameters

Four parts make up the canonical Claude Code request shape. Without either of the first two, `api.anthropic.com` answers OAuth requests on non-Haiku models with 401 or 429 ([What Happens Without These](#what-happens-without-these)). The other two ship on every request. At a third-party relay gateway, the attribution header can decide admission while client identity headers have no effect on their own ([Third-Party Gateway Validation](#third-party-gateway-validation)).

### 1. Beta Headers

```text
anthropic-beta: claude-code-20250219,oauth-2025-04-20
```

- `claude-code-20250219`: identifies the request as a Claude Code client.
- `oauth-2025-04-20`: enables OAuth authentication.

Other useful betas:

| Header                            | Purpose                                            |
| --------------------------------- | -------------------------------------------------- |
| `interleaved-thinking-2025-05-14` | Extended thinking support                          |
| `context-1m-2025-08-07`           | 1M context window                                  |
| `context-management-2025-06-27`   | Context management                                 |
| `prompt-caching-scope-2026-01-05` | Prompt caching                                     |
| `effort-2025-11-24`               | Effort control                                     |
| `structured-outputs-2025-12-15`   | JSON-schema-constrained responses (one-shot calls) |
| `advanced-tool-use-2025-11-20`    | Tool search (first-party only)                     |
| `task-budgets-2026-03-13`         | Advisory token budget across an agentic loop       |

#### Per-Model Beta Sets

Haiku is the only family whose beta set differs: Claude Code moves `claude-code-20250219` later in the agentic sequence on Haiku and (in 2.1.87) omits it on Haiku one-shots.

- **`prompt-caching-scope` ships on every agentic request.** Claude Code 2.1.121 sent it to custom base URLs and 2.1.296 sends it on every model (2.1.87 sent it only to the first-party API). The header alone is a server-side no-op without a matching `cache_control.scope` field.
- **`effort` and `context-management` betas need a body field.** Without `output_config.effort` or `context_management.edits` ([Agentic Request Body Fields](#agentic-request-body-fields)), each header is a silent no-op and the request runs at the server default.

On agentic Opus 5.5 and Sonnet 5.5 API-key requests (observed 2026-10-15 on Claude Code 2.1.296), Claude Code sends `claude-code-20250219, interleaved-thinking-2025-05-14, thinking-token-count-2026-05-13, context-management-2025-06-27, prompt-caching-scope-2026-01-05, mid-conversation-system-2026-04-07, per-turn-control-2026-07-01, advisor-tool-2026-03-01, effort-2025-11-24`. On Haiku 5.5 it moves `claude-code-20250219` to immediately after `mid-conversation-system-2026-04-07`. On Claude Code 2.1.301 (verified 2026-10-17), Fable 5.1 sends the same betas as Opus 5.5. Under OAuth, 2.1.87 places `oauth-2025-04-20` right after the leading `claude-code-20250219` (or first on Haiku), which has not been rechecked on later releases.

Claude 5.5 models and Fable 5.1 make effort, interleaved thinking, the 1M context window, and structured outputs GA. On 2026-10-18, [What's new in Claude Fable 5.1](https://platform.claude.com/docs/en/models/fable-5-1/whats-new-fable-5-1) listed Fable 5.1 as available to all customers on the Claude API and partner platforms, except under zero data retention without express Anthropic authorization. Claude Code's [model configuration docs](https://code.claude.com/docs/en/model-config) (same date) add that organization admins can restrict models, Fable usage can bill to usage credits depending on plan and seat tier, and gateways not configured for `claude-fable-5-1` reject it. [Client](../../design/client.md) covers oxide-code's derived beta sets.

### 2. System Prompt Prefix (as a Separate Block)

The `system` parameter must be an array of text blocks (a plain string is rejected even with the same text), with the identity prefix in its own block:

```json
"system": [
  {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
  {"type": "text", "text": "...rest of prompt..."}
]
```

The API checks that the first non-attribution text block matches one of the known prefixes:

- `"You are Claude Code, Anthropic's official CLI for Claude."`
- `"You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK."`
- `"You are a Claude agent, built on Anthropic's Claude Agent SDK."`

Because the server enforces the block boundary, concatenating the prefix into the prompt body returns 429 on OAuth requests.

### 3. Attribution Header

Claude Code prepends an attribution header as the first system block (2.1.121 example):

```json
{
  "type": "text",
  "text": "x-anthropic-billing-header: cc_version=2.1.121.91c; cc_entrypoint=cli; cch=8f81f;"
}
```

Format (`cli` for interactive sessions):

```text
x-anthropic-billing-header: cc_version=<VERSION>.<FINGERPRINT>; cc_entrypoint=<ENTRYPOINT>; cch=<HASH>;
```

#### Fingerprint (3-Char Version Suffix)

A 3-character hex value derived from conversation content:

1. Select the first non-meta user message and take its string content or first text block, or `""` when it has neither. Claude Code 2.1.301 selects before normalization merges adjacent user messages, skipping the meta context message to read the user's first prompt. Version 2.1.87 selected after normalization, starting with the context message.
2. Extract characters at UTF-16 code-unit indices `[4, 7, 20]`, using `"0"` when an index is out of bounds. An index inside a surrogate pair picks a lone surrogate, which UTF-8 encoding turns into U+FFFD under both Bun and Node.
3. Compute `SHA256(SALT + chars + VERSION)` and take the first 3 hex characters, where `SALT` is `59cf53e54c78` (must match the server's).

#### cch (5-Char Request Integrity Hash)

The `cch` field is a 5-character request integrity hash used for feature gating (fast mode) and billing attribution, documented for Claude Code 2.1.37 in February 2026 ([a10k.co writeup](https://a10k.co/b/reverse-engineering-claude-code-cch.html)):

1. Write a `cch=00000` placeholder into the billing header and serialize the request body to JSON.
2. Compute `xxHash64(body_bytes, 0x6E52736AC806831E) & 0xFFFFF` (20 bits, formatted as 5 zero-padded hex characters) and overwrite the five `0` characters in place before sending.

Because the hash covers the entire serialized body, only non-billing system blocks (excluded from server verification) may change afterward. `system` must serialize before `messages` so the billing header's placeholder appears first and a tool result containing `cch=00000` does not take the replacement.

#### Known Bug: cch Substitution Breaks Prompt Cache

[anthropics/claude-code#40652](https://github.com/anthropics/claude-code/issues/40652) reports that Claude Code replaces `cch=` values across the entire serialized body, including historical tool results. Any tool result containing `cch=XXXXX` (such as proxy logs or session JSONL files) gets rewritten on every turn, invalidating the prompt cache and wasting 30-50K+ tokens per turn (partially mitigated in v2.1.90-91).

### 4. Client Identity Headers

```text
User-Agent: claude-cli/<version> (external, cli)
x-app: cli
```

Claude Code formats `User-Agent` as `claude-cli/<version> (<user_type>, <entrypoint>)`, where `user_type` is `external` (or `ant` for Anthropic employees) and `entrypoint` is `cli` (unchanged in 2.1.296).

In Claude Code 2.1.121, the Stainless TypeScript SDK also emitted `x-stainless-lang`, `-package-version`, `-runtime`, `-runtime-version`, `-os`, `-arch`, `-timeout`, and `-retry-count`.

## What Happens Without These

| Missing                  | Status |
| ------------------------ | ------ |
| `claude-code-20250219`   | 429    |
| `oauth-2025-04-20`       | 401    |
| Prefix as separate block | 429    |
| Prefix in body string    | 429    |

These OAuth statuses were measured on Claude 4 Opus and Sonnet and have not been rechecked on the 5.5 models. A prefix inside a concatenated string fails like a missing one.

## Prompt Caching Scope

Blocks in the `system` array can carry `cache_control`, and `scope` sets the sharing level:

| Value      | Shape                                      | Shared across             |
| ---------- | ------------------------------------------ | ------------------------- |
| `global`   | `{"type": "ephemeral", "scope": "global"}` | All users on the 1P API   |
| _(absent)_ | `{"type": "ephemeral"}`                    | The caller's organization |
| `null`     | no `cache_control` at all                  | _(not cached)_            |

### Prefix Invariance

The server evaluates request elements in this order:

```text
[tool definitions] → [system blocks...] → [messages...]
```

On 2026-04-23, a third-party gateway rejected agentic requests whose static system block carried `scope: "global"` after tool definitions with HTTP 400 ([#23](https://github.com/hakulab/oxide-code/pull/23)):

> `cache_control.scope: "global"` is only valid when every preceding block is also globally scoped. A block with `scope: "global"` was found after content with a narrower cache scope.

Other servers accept the same layout. Both the Anthropic API ([#123](https://github.com/hakulab/oxide-code/issues/123), oxide-code `01b4823`) and the relay gateway probed on 2026-10-17 accepted `scope: "global"` after tool definitions ([Third-Party Gateway Validation](#third-party-gateway-validation)).

Claude Code 2.1.301 (verified 2026-10-17) sends `scope: "global"` and the `cch` placeholder only when the base URL is `api.anthropic.com`, whereas 2.1.87 checked only the provider (which still reads as first-party when `ANTHROPIC_BASE_URL` points at a gateway).

## Agentic Request Body Fields

Some capabilities live in a body field beside the `anthropic-beta` header that gates them, and the header alone is a silent no-op. Unless noted otherwise, the Claude Code 2.1.301 values below were verified on 2026-10-17 across all supported models.

### `output_config.effort`

Sets the intelligence-versus-latency level of agentic turns to `low`, `medium`, `high`, `xhigh`, or `max`:

```json
{
  "output_config": { "effort": "xhigh" }
}
```

- **GA body field.** Anthropic's [effort docs](https://platform.claude.com/docs/en/build-with-claude/effort) (2026-10-17) list effort as GA with no beta header, and every supported model accepts all five levels. The `effort-2025-11-24` header alone is a no-op, though Claude Code still sends it.
- **Per-model defaults.** API defaults are `medium` on Opus 5.5 and Haiku 5.5 and `high` on Sonnet 5.5 and Fable 5.1, while Claude Code 2.1.301 sends `medium` on Sonnet 5.5 ([Configuration](../../guide/configuration.md#effort-reasoning-effort)).
- **`max_tokens`.** Claude Code 2.1.301 sends 128K on the 5.5 models and 64K on Fable 5.1 ([Configuration](../../guide/configuration.md#max_tokens-response-ceiling)).

### `context_management.edits`

Partners `context-management-2025-06-27`. Claude Code 2.1.301 sends this directive on every agentic request to supported models:

```json
{
  "context_management": {
    "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
  }
}
```

### `cache_control.ttl`

Anthropic dropped the default ephemeral-cache TTL from 1 hour to 5 minutes on 2026-03-06, a 40-55% savings regression on sessions longer than 5 minutes. The opt-in is a GA body field requiring no beta header (observed 2026-04-24 on Claude Code 2.1.119 with `claude-opus-4-7`):

```json
{
  "cache_control": { "type": "ephemeral", "ttl": "1h" }
}
```

It accepts `"5m"` (the server default, equivalent to omitting the field) and `"1h"` (at a higher write premium). Per [Anthropic's prompt-caching docs](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching), invalidation follows `tools → system → messages`, so a change at one level busts that level and every level after it ([Client](../../design/client.md)).

### `thinking.display`

See [Extended Thinking § Display Modes](extended-thinking.md#display-modes). The default is `"omitted"`.

## Third-Party Tool Restrictions

As of April 4, 2026, Anthropic accepts OAuth subscription credits (Pro / Max) only from official Claude Code and claude.ai clients. Third-party tools reusing the OAuth flow count as "third-party harness traffic" and must use either:

- **API key** (`ANTHROPIC_API_KEY`) with standard per-token billing.
- **Extra Usage** billing on the account, which allows OAuth with per-token billing beyond the subscription.

The `cch` hash is the primary technical enforcement, and both its algorithm (non-cryptographic xxHash64) and constants are public. No TLS fingerprinting, binary attestation, pre-registration handshake, replay detection, or connection association is used. Anthropic could tighten enforcement at any time, though today's scheme acts as billing plumbing without a security boundary.

Claude Code attaches the attribution block on turns and side queries (in both 2.1.87 and 2.1.301), and `CLAUDE_CODE_ATTRIBUTION_HEADER=0` drops it everywhere ([Client](../../design/client.md#base-url-gating), [Authentication](../../guide/authentication.md#llm-gateways)).

- **`cch` by destination.** Requests to the Anthropic API carry a valid `cch` hash. Since 2.1.181, custom base URLs receive the billing block without `cch`, so body-caching gateways see a stable prefix.
- **Version-specific constants.** The fingerprint salt and xxHash64 seed may change across releases. Version 2.1.301 keeps the salt and character indices while changing message selection ([Fingerprint](#fingerprint-3-char-version-suffix)), and its xxHash64 seed is unverified ([#123](https://github.com/hakulab/oxide-code/issues/123#issuecomment-6210505896)).
- **Verification.** On 2026-10-16, the Anthropic API accepted oxide-code's requests carrying a `cch` computed with the seed above, confirming acceptance without proving server-side verification.

## Third-Party Gateway Validation

On 2026-10-17, API-key probes against a third-party relay gateway replayed oxide-code's request (`main` at `419d3b9a`, Claude Code 2.1.296 shape), adding or removing one element at a time.

The gateway admits a request carrying either the billing block or a `metadata.user_id` JSON object with `session_id`. A request with neither gets HTTP 400 `invalid_request_error` ("Please use authentic Claude Code client or upgrade your version"). A plain-string `user_id` or an object with only `device_id` fails, while field order, JSON spacing, and `device_id` format go unchecked.

When present, the billing block must also pass these checks:

- Missing `.<fingerprint>` suffix or `cc_version=1.0.0` gets the same 400 (version floor).
- `cc_version` of `2.1.87` or `9.9.9` with a `2.1.296` fingerprint passes the gate and then returns HTTP 503 "All providers unavailable" (indicating an upstream version-fingerprint check).
- A billing block in the second system block returns the same 503.

None of these changed the outcome on its own: `User-Agent`, `x-app`, `anthropic-beta`, Stainless headers, `x-claude-code-session-id`, `?beta=true`, identity block, tools, and streaming. The static block passed with `scope: "global"`, without `ttl`, or without `cache_control`, as did oxide-code's thinking, effort, and `context_management` fields. Up to 20k characters of system text unrelated to Claude Code passed with or without the identity block. Unserved models return plain-text `404 page not found`, and `GET /v1/models` lists served models.

oxide-code passed every path on Opus 5.5, Sonnet 5.5, and Haiku 5.5: headless, tool-using turns, REPL, and TUI (including Haiku 5.5 session titles, which at `419d3b9a` omitted the billing block on API keys and passed on `metadata.user_id` alone). oh-my-pi 18.11.0 gets 400 on API keys because it sends neither.

## API Version

`anthropic-version` is `2023-06-01` across all Claude Code endpoints, the only stable API version.

## Model IDs

Anthropic's [models overview](https://platform.claude.com/docs/en/about-claude/models/overview) lists the dateless IDs `claude-fable-5-1`, `claude-opus-5-5`, `claude-sonnet-5-5`, and `claude-haiku-5-5` as pinned snapshots. All four have a 1M-token context window without `context-1m-2025-08-07`. The `[1m]` suffix (`claude-opus-5-5[1m]`) is a client-side convention that Claude Code strips before sending.

## Pricing

Anthropic's [pricing page](https://platform.claude.com/docs/en/about-claude/pricing) lists first-party Claude API prices in USD per million tokens (as of 2026-10-17):

| Model                     | Input | 5m cache write | 1h cache write | Cache read | Output |
| ------------------------- | ----- | -------------- | -------------- | ---------- | ------ |
| Fable 5.1                 | $10   | $12.50         | $20            | $0.25      | $50    |
| Opus 5.5                  | $4    | $5             | $8             | $0.20      | $20    |
| Sonnet 5.5                | $2    | $2.50          | $4             | $0.10      | $10    |
| Haiku 5.5 (≤ 100K prompt) | $0.10 | $0.125         | $0.20          | $0.01      | $0.50  |
| Haiku 5.5 (> 100K prompt) | $0.50 | $0.625         | $1             | $0.05      | $2.50  |

Haiku 5.5 picks its tier per request from total prompt size (input + cache creation + cache read), so session cost must be summed per request. Cost display stays best-effort because account discounts, marketplace billing, data residency, fast mode, and server-side tool pricing can change the final bill.

## SDK vs Raw HTTP

For OAuth, Claude Code passes the Anthropic TypeScript SDK an `authToken`, and the SDK:

- Sends `Authorization: Bearer <token>`.
- Appends `?beta=true` to the URL.
- Adds `x-stainless-*` telemetry headers.
- Retries `x-should-retry: true` responses with exponential backoff.

A raw HTTP client must replicate these headers and `?beta=true` ([Client](../../design/client.md#request-shape)).

## Token Refresh

OAuth tokens expire at `expiresAt` (in milliseconds). Claude Code refreshes them 5 minutes before expiry by sending a `POST` with `refresh_token` to `platform.claude.com/v1/oauth/token`. Across processes, `proper-lockfile` creates a `.lock` directory beside the config directory's real path (`~/.claude.lock/`), refreshes its mtime every 5 s while held, and treats a lock untouched for 10 s as stale (its `update` and `stale` defaults).

[Login](../../design/login.md) covers oxide-code's refresh flow.
