# Anthropic API Authentication

Research notes on how to authenticate with the Anthropic Messages API using OAuth tokens from Claude Code. These findings are based on reverse-engineering [Claude Code](https://github.com/hakula139/claude-code) and testing against the production API. Unless another source is cited, a claim about what Claude Code sends that names no release was checked against the frozen 2.1.87 source that [Sources](#sources) lists.

## Authentication Methods

### API Key

Standard approach. Set `x-api-key` header directly.

### OAuth (Claude Code Credentials)

Claude Code stores OAuth tokens in **platform-specific secure storage** with a plaintext fallback:

- **macOS**: macOS Keychain (service `"Claude Code-credentials"`), falling back to `~/.claude/.credentials.json`.
- **Linux**: `~/.claude/.credentials.json` (plaintext only).

`CLAUDE_CONFIG_DIR` moves the config directory, and so the file, away from `~/.claude`. Claude Code reads the variable in Unicode NFC, and when it is set, the Keychain service becomes `Claude Code-credentials-<hash>`, where `<hash>` is the first 8 hex digits of the SHA-256 of the variable's value.

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

### Long-Lived OAuth Token

`claude setup-token` issues a long-lived subscription token. Claude Code reads it from `CLAUDE_CODE_OAUTH_TOKEN`, sends it with the same OAuth request shape as a stored login, and never refreshes it. It ranks below `ANTHROPIC_API_KEY` and above the stored login ([authentication precedence](https://code.claude.com/docs/en/authentication#authentication-precedence)).

### Auth Token (LLM Gateways)

`ANTHROPIC_AUTH_TOKEN` sends `Authorization: Bearer <token>` with the API-key request shape, so the `oauth-2025-04-20` beta stays off. It ranks above every other credential. Gateways may also require extra headers, which Claude Code reads from `ANTHROPIC_CUSTOM_HEADERS` as one `Name: value` per line.

## Required Headers and Parameters

Four things make up the canonical Claude Code request shape. The first two are OAuth-load-bearing because their absence triggers 401 / 429 against `api.anthropic.com` on non-Haiku models (see [What Happens Without These](#what-happens-without-these)). The latter two ship on every request. At a third-party relay gateway, the attribution header can decide whether a request passes, while the identity headers have no effect on their own (see [Third-Party Gateway Validation](#third-party-gateway-validation)).

### 1. Beta Headers

```text
anthropic-beta: claude-code-20250219,oauth-2025-04-20
```

- `claude-code-20250219`: Identifies the request as a Claude Code client.
- `oauth-2025-04-20`: Enables OAuth authentication.

Additional useful betas:

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

Haiku is the one family whose beta set differs: Claude Code moves `claude-code-20250219` later in the agentic sequence on Haiku and, per the frozen 2.1.87 source, leaves it off Haiku one-shots.

- **`prompt-caching-scope` ships on every agentic request.** On agentic requests, Claude Code 2.1.121 sent it to a custom base URL and 2.1.296 sends it on every model. The 2.1.87 source sent it only to the first-party API. The header alone is a server-side no-op without a matching `cache_control.scope` field.
- **`effort` and `context-management` betas need a body field.** Sending the header alone is a silent no-op: the request runs at the server default. See [Agentic Request Body Fields](#agentic-request-body-fields) for the matching `output_config.effort` and `context_management.edits` shapes.

On agentic Opus 5.5 and Sonnet 5.5 requests, and by inference Fable 5.1, Claude Code 2.1.296 sends `claude-code-20250219, interleaved-thinking-2025-05-14, thinking-token-count-2026-05-13, context-management-2025-06-27, prompt-caching-scope-2026-01-05, mid-conversation-system-2026-04-07, per-turn-control-2026-07-01, advisor-tool-2026-03-01, effort-2025-11-24`. On Haiku 5.5 it moves `claude-code-20250219` to just after `mid-conversation-system-2026-04-07`. Both sequences were captured from `claude --bare -p` with an API key on 2026-10-15. Fable 5.1 was not captured on 2.1.296, but on Claude Code 2.1.301 `claude --bare -p` sent Fable 5.1 and Opus 5.5 identical betas against a local listener on 2026-10-17. Under OAuth, the frozen 2.1.87 source puts `oauth-2025-04-20` right after the leading `claude-code-20250219`, or first on Haiku. That slot was not captured.

This client supports the Claude 5.5 models and Fable 5.1, where effort, interleaved thinking, the 1M window, and structured outputs are GA. On 2026-10-18, [What's new in Claude Fable 5.1](https://platform.claude.com/docs/en/models/fable-5-1/whats-new-fable-5-1) listed Fable 5.1 as available to all customers on the Claude API and partner platforms, except under zero data retention unless Anthropic expressly authorizes it. Claude Code's [model configuration docs](https://code.claude.com/docs/en/model-config), read the same day, add that organization admins can restrict which models members run, that Fable usage can bill to usage credits depending on the plan and seat tier, and that a gateway not configured to serve `claude-fable-5-1` rejects requests for it. On agentic turns, `client::anthropic::compute_betas` sends the sequence above, which includes betas for features this client leaves unused. Its test pins `CLAUDE_CLI_VERSION`, the release the user agent and billing block report, so a version bump fails until the sequence is recaptured for every model, Fable 5.1 included. It then appends `thinking-binding-controls-2026-08-01`, plus `thinking-display-updates-2026-08-18` when the request sets `thinking.display = "updates"`. Agentic turns on other model IDs carry `claude-code-20250219`, `oauth-2025-04-20` (OAuth only), and `prompt-caching-scope-2026-01-05`, and go without effort, thinking, or context management. Side queries, such as session titles, carry `claude-code-20250219` (skipped on Haiku) and `oauth-2025-04-20` (OAuth only). The body-side `cache_control.scope: "global"` stays gated on the base URL, as [Prompt Caching Scope](#prompt-caching-scope) describes.

### 2. System Prompt Prefix (as a Separate Block)

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

### 3. Attribution Header

Claude Code prepends an attribution header as the very first system block, as in this 2.1.121 example:

```json
{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.121.91c; cc_entrypoint=cli; cch=8f81f;"}
```

Format:

```text
x-anthropic-billing-header: cc_version=<VERSION>.<FINGERPRINT>; cc_entrypoint=<ENTRYPOINT>; cch=<HASH>;
```

#### Fingerprint (3-Char Version Suffix)

A 3-character hex value derived from conversation content:

1. Select the first user message that is not meta, and take its string content or its first text block, or `""` when it has neither. Claude Code 2.1.301 selects from the messages before normalization merges adjacent user messages, so it skips the context message, which is meta, and reads the user's first prompt. 2.1.87 took the first user message after normalization, which began with the context message. oxide-code skips the user context it prepends and reads the first user message of the transcript.
2. Extract the characters at indices `[4, 7, 20]` of that text, using `"0"` for an index out of bounds. Indices count UTF-16 code units. An index inside a surrogate pair picks a lone surrogate, which the hash input's UTF-8 encoding turns into U+FFFD under both Bun and Node.
3. Compute `SHA256(SALT + chars + VERSION)`, take the first 3 hex characters.
4. Salt: `59cf53e54c78` (hardcoded in `claude-code/src/utils/fingerprint.ts`, must match server).

The entrypoint is `cli` for interactive sessions.

#### cch (5-Char Request Integrity Hash)

The `cch` field is a request integrity hash used for feature gating (fast mode) and billing attribution. It was reverse-engineered in February 2026 from Anthropic's custom Bun binary, with examples from Claude Code 2.1.37 ([a10k.co writeup](https://a10k.co/b/reverse-engineering-claude-code-cch.html)).

How it works in the 2.1.37 binary the writeup analyzed:

1. The JavaScript layer writes a `cch=00000` placeholder into the billing header. The 2.1.87 source gates this step on the `feature('NATIVE_CLIENT_ATTESTATION')` compile-time flag.
2. The request body is serialized to JSON with the placeholder in place.
3. Bun's native HTTP stack (compiled Zig, `bun-anthropic/src/http/Attestation.zig`) intercepts the `fetch()` call, detects the placeholder, and computes `xxHash64(body_bytes, seed) & 0xFFFFF`.
4. The five `0` characters are overwritten in-place with the 5-char hex result before sending.

Constants from the writeup:

- **Seed**: `0x6E52736AC806831E` (64-bit, embedded in the binary's data section).
- **Mask**: `& 0xFFFFF` (20 bits → 5 hex chars, zero-padded).

The hash covers the entire serialized body (messages, tools, metadata, model, thinking config). The only safe post-hash modification is to non-billing system blocks, which the server excludes from its verification.

**JSON key ordering matters**: `system` must be serialized before `messages` so the placeholder in the billing header appears first in the JSON. If tool results contain the literal `cch=00000`, serializing `messages` first would cause the replacement to hit the wrong occurrence.

#### Known Bug: cch Substitution Breaks Prompt Cache

[anthropics/claude-code#40652](https://github.com/anthropics/claude-code/issues/40652) reports that the Bun binary performs a global find-and-replace of `cch=` values across the entire serialized request body, including historical tool results. If any tool result in the conversation contains a `cch=XXXXX` string (e.g., from reading proxy logs or session JSONL files), the substitution rewrites those historical bytes on every turn, changing the conversation prefix and permanently invalidating the prompt cache. This wastes 30-50K+ tokens per turn and never self-heals. The issue reports the behavior partially mitigated in v2.1.90-91.

oxide-code avoids this: it serializes with `serde_json`, replaces only the first occurrence via `str::replacen`, and never mutates historical message content.

### 4. Client Identity Headers

```text
User-Agent: claude-cli/<version> (external, cli)
x-app: cli
```

Claude Code constructs the `User-Agent` as `claude-cli/<version> (<user_type>, <entrypoint>)` where `user_type` is `external` (or `ant` for Anthropic employees) and `entrypoint` is `cli`. The 2.1.296 bundle keeps this format.

In Claude Code 2.1.121, the Stainless TS SDK also emitted `x-stainless-lang`, `-package-version`, `-runtime`, `-runtime-version`, `-os`, `-arch`, `-timeout`, and `-retry-count`, and oxide-code sends the same set.

## What Happens Without These

| Missing                  | Status |
| ------------------------ | ------ |
| `claude-code-20250219`   | 429    |
| `oauth-2025-04-20`       | 401    |
| Prefix as separate block | 429    |
| Prefix in body string    | 429    |

These OAuth statuses were measured on the Claude 4 Opus and Sonnet models and have not been rechecked on the 5.5 models. The last two rows are the critical distinction: having the prefix present in a concatenated string is **not sufficient**. It must be a separate `{"type": "text", "text": "..."}` block in the system array.

## Prompt Caching Scope

Blocks in the `system` array can carry `cache_control` for prompt caching. The `scope` field controls the sharing level:

| Value      | Shape                                      | Shared across             |
| ---------- | ------------------------------------------ | ------------------------- |
| `global`   | `{"type": "ephemeral", "scope": "global"}` | All users on the 1P API   |
| _(absent)_ | `{"type": "ephemeral"}`                    | The caller's organization |
| `null`     | no `cache_control` at all                  | _(not cached)_            |

### Prefix Invariance

The server sees request elements in this order:

```text
[tool definitions] → [system blocks...] → [messages...]
```

On 2026-04-23, a third-party gateway rejected agentic requests whose static system block carried `scope: "global"` after tool definitions, with HTTP 400 ([#23](https://github.com/hakula139/oxide-code/pull/23)):

> `cache_control.scope: "global"` is only valid when every preceding block is also globally scoped. A block with `scope: "global"` was found after content with a narrower cache scope.

Other servers accept the same layout. The Anthropic API accepted `scope: "global"` after tool definitions in [#123](https://github.com/hakula139/oxide-code/issues/123)'s live run on 2026-10-17 with oxide-code at 01b4823, and so did the relay gateway probed that day (see [Third-Party Gateway Validation](#third-party-gateway-validation)).

### oxide-code Gating

oxide-code ships `prompt-caching-scope-2026-01-05` on every agentic request, as Claude Code does, and gates only the body-side `cache_control.scope` on `is_first_party_base_url(&config.base_url)`:

- Base URL host matches `api.anthropic.com` or `api-staging.anthropic.com` → `{"type": "ephemeral", "scope": "global"}`.
- Any other host (gateways, self-hosted, malformed URLs) → `{"type": "ephemeral"}`. The header still ships, and without the scope field it is a server-side no-op.

Shape is otherwise identical in both modes. Only the body-side `scope` field toggles.

This mirrors the Claude Code 2.1.301 bundle, which, as read on 2026-10-17, sends `scope: "global"` and the `cch` placeholder only when the base URL is `api.anthropic.com`. The 2.1.87 source checked only the provider, which still reads as first-party when `ANTHROPIC_BASE_URL` points at a gateway. The gate also keeps requests valid on gateways like the one in [#23](https://github.com/hakula139/oxide-code/pull/23) that reject the global scope after tool definitions, though the relay gateway probed on 2026-10-17 accepted it.

## Agentic Request Body Fields

Some capabilities live in the request body alongside the `anthropic-beta` header that gates them. Shipping the header but omitting the body field is a silent no-op because the feature does not activate. [Sources](#sources) records when each field was captured.

### `output_config.effort`

Controls the intelligence-vs-latency tier of agentic turns via one of five tokens: `low`, `medium`, `high`, `xhigh`, `max`.

```json
{
  "output_config": { "effort": "xhigh" }
}
```

- **The body field does the work.** Anthropic's [effort docs](https://platform.claude.com/docs/en/build-with-claude/effort) list effort as GA with no beta header, and the `effort-2025-11-24` header alone is a no-op. Claude Code still sends the header, so oxide-code does too.
- **Levels.** Every supported model accepts all five, per the effort docs on 2026-10-17.
- **Per-model default.** The API defaults are `medium` on Opus 5.5 and Haiku 5.5 and `high` on Sonnet 5.5 and Fable 5.1, and oxide-code sends them explicitly. Claude Code 2.1.301 sends `medium` on Sonnet 5.5 instead.
- **`max_tokens`.** Claude Code 2.1.301 sends 128K on the 5.5 models and 64K on Fable 5.1. oxide-code defaults to 64K on the supported models, since effort already steers thinking depth, and to 16K on other IDs.

### `context_management.edits`

Partners the `context-management-2025-06-27` beta header. Claude Code 2.1.301 ships the same directive on every agentic request to the supported models:

```json
{
  "context_management": {
    "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
  }
}
```

oxide-code applies the body-header coupling as an invariant: the body field is populated on every request whose model is in the supported catalog, the same condition that enables the beta header. Side queries (the `complete` path in `client::anthropic`) skip both, matching the reference wire and keeping the title-generation path minimal.

### `cache_control.ttl`

Anthropic silently dropped the default ephemeral-cache TTL from 1 hour to 5 minutes on 2026-03-06, a 40-55% savings regression on any session longer than 5 minutes. The opt-in uses a body field:

```json
{
  "cache_control": { "type": "ephemeral", "ttl": "1h" }
}
```

Accepted values: `"5m"` (server default, equivalent to omitting the field) and `"1h"` (opt-in at higher write premium). No beta header is required because the field is GA.

**oxide-code default.** `prompt_cache_ttl = "1h"`. The hit-rate recovery on real agent sessions (tool-use loops, resumed conversations) dominates the write premium, so 1 hour is the default. Users opt down via `[client].prompt_cache_ttl = "5m"` or `OX_PROMPT_CACHE_TTL=5m`.

Invalidation order (from the Anthropic caching docs) is `tools → system → messages`: any change at a level busts that level and every level after it. oxide-code sends two breakpoints with the configured TTL. The first sits on the static system-prompt prefix block (with `scope: "global"` only on first-party base URLs, as [oxide-code Gating](#oxide-code-gating) describes). The second sits on the last content block of the final message, without `scope`, so each turn reads the previous turn's transcript from cache. The prompt context is built once per session, which keeps that prefix stable from turn to turn. Compaction summaries are one-off requests that no later request extends, so they skip the second breakpoint.

### `thinking.display`

See [Extended Thinking § Display modes](extended-thinking.md#display-modes). The default is `"omitted"`. `show_thinking=true` in oxide-code opts into `"summarized"`. With it off, requests on every supported model except Haiku 5.5 use `"updates"`.

## Third-Party Tool Restrictions

As of April 4, 2026, Anthropic enforces that OAuth subscription credits (Pro / Max) are only valid for official Claude Code and claude.ai clients. Third-party tools that reuse the OAuth flow are classified as "third-party harness traffic" and must use either:

- **API key** (`ANTHROPIC_API_KEY`) with standard per-token billing.
- **Extra Usage** billing enabled on the account, which allows OAuth but bills per-token beyond the subscription.

The `cch` hash is the primary technical enforcement mechanism, and both the algorithm (xxHash64, non-cryptographic) and the constants are publicly known. No additional protections exist: no TLS fingerprinting, binary attestation, pre-registration handshake, replay detection, or connection association. Anthropic could escalate enforcement at any time, but today's scheme reads as billing plumbing with no security boundary behind it.

oxide-code attaches the attestation block to every request under every auth mode, as Claude Code does on its turns and side queries in both the 2.1.87 source and the 2.1.301 bundle. `CLAUDE_CODE_ATTRIBUTION_HEADER=0` drops it everywhere. A relay gateway checks its version and fingerprint (see [Third-Party Gateway Validation](#third-party-gateway-validation)). Requests to the Anthropic API carry a valid `cch` hash. Since 2.1.181, Claude Code sends custom base URLs the block without `cch`, so a gateway that caches on the request body sees a stable prefix, and oxide-code matches that. The fingerprint salt and xxHash64 seed are version-specific constants, so they may change with Claude Code releases. The 2.1.301 bundle keeps the same salt and character indices and changes the message selection, as [Fingerprint](#fingerprint-3-char-version-suffix) describes. Its seed lives in the native Bun HTTP stack, outside the JavaScript bundle, and a scan of the 2.1.301 binary did not find it ([#123](https://github.com/hakula139/oxide-code/issues/123#issuecomment-6210505896)). On 2026-10-16, the Anthropic API accepted oxide-code's requests carrying a `cch` computed with the seed above, which shows that the API accepts the hash but not that it verifies it.

## Third-Party Gateway Validation

On 2026-10-17, probes against a third-party relay gateway with an API key replayed oxide-code's request, built from `main` at 419d3b9a in Claude Code 2.1.296's shape, and removed or added one element at a time.

The gateway admits a request that carries the billing block or a `metadata.user_id` whose JSON object has a `session_id`. A request with neither gets HTTP 400 `invalid_request_error` with "Please use authentic Claude Code client or upgrade your version". A plain-string `user_id`, or an object with only `device_id`, does not count. Field order, JSON spacing, and the `device_id` format go unchecked.

A billing block that is present must also pass these checks:

- A `cc_version` without its `.<fingerprint>` suffix, or `cc_version=1.0.0`, gets the same 400, so the gateway enforces a version floor.
- A `cc_version` of 2.1.87 or 9.9.9 with the fingerprint computed for 2.1.296 passes the gate and then gets HTTP 503 "All providers unavailable", which points to an upstream check that the fingerprint matches the version.
- A billing block in the second system block gets the same 503.

None of these changes the outcome on its own: the user agent, `x-app`, `anthropic-beta`, the Stainless headers, `x-claude-code-session-id`, `?beta=true`, the identity block, tools, and streaming. The static block passed with `cache_control` carrying `scope: "global"`, lacking `ttl`, or absent, and so did oxide-code's thinking, effort, and `context_management` fields. System blocks of up to 20k characters of text unrelated to Claude Code passed, with or without the identity block. A model the gateway does not serve gets a plain-text `404 page not found` from the correct base URL, and `GET /v1/models` lists the models it serves.

oxide-code passed every path through the gateway on Opus 5.5, Sonnet 5.5, and Haiku 5.5: headless, a tool-using turn, the REPL, and the TUI with Haiku 5.5 session titles. At 419d3b9a, its API-key session titles carried no billing block and passed on `metadata.user_id` alone. oh-my-pi 18.11.0 with an API key gets the 400, since its API-key requests carry neither the billing block nor `metadata.user_id`.

## API Version

The `anthropic-version` header is `2023-06-01` across all Claude Code endpoints. This is the only stable API version.

## Model IDs

oxide-code sends the dateless IDs `claude-fable-5-1`, `claude-opus-5-5`, `claude-sonnet-5-5`, and `claude-haiku-5-5`, which Anthropic's [models overview](https://platform.claude.com/docs/en/about-claude/models/overview) lists as pinned snapshots. All four have a 1M-token context window without the `context-1m-2025-08-07` beta. The `[1m]` suffix (e.g., `claude-opus-5-5[1m]`) is a client-side convention that Claude Code strips before sending to the API via `normalizeModelStringForAPI()`. oxide-code treats such an ID as unknown and suggests the ID without the suffix.

## SDK vs Raw HTTP

Claude Code uses the Anthropic TypeScript SDK with `authToken` (not `apiKey`) for OAuth. The SDK:

- Sends `Authorization: Bearer <token>` internally.
- Adds `?beta=true` query parameter to the URL.
- Includes `x-stainless-*` headers (SDK telemetry).
- Retries on `x-should-retry: true` responses with exponential backoff.

For raw HTTP (as in oxide-code), replicate the headers manually. oxide-code mirrors `?beta=true` and the Stainless headers.

## Token Refresh

OAuth tokens expire (check `expiresAt` in milliseconds). Claude Code refreshes them automatically with a 5-minute buffer before expiry, using a `POST` to `platform.claude.com/v1/oauth/token` with the `refresh_token`. Cross-process safety is handled via directory-based locking: `proper-lockfile` creates a `.lock` directory beside the real path of the config directory, as in `~/.claude.lock/`, refreshes its mtime every 5 seconds while held, and treats a lock untouched for 10 seconds as stale, its `update` and `stale` defaults.

oxide-code implements the same refresh flow: proactive refresh with the 5-minute buffer, directory-based locking compatible with Claude Code, and credential write-back preserving unknown fields.

## Sources

The `claude-code/src/` paths are files in the frozen 2.1.87 source.

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

Body-field research is empirical. The `cache_control.ttl` wire shape was captured live from a Claude Code 2.1.119 `claude-code --bare -p --model claude-opus-4-7` session against a local SSE proxy on 2026-04-24 and cross-referenced with [Anthropic prompt-caching docs](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching). The `output_config.effort`, `context_management.edits`, and `max_tokens` values were captured from `claude --bare -p` on Claude Code 2.1.301 against a local listener on 2026-10-17, once per supported model, and cross-referenced with Anthropic's [effort docs](https://platform.claude.com/docs/en/build-with-claude/effort).
