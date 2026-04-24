# Anthropic API Authentication

Research notes on how to authenticate with the Anthropic Messages API using OAuth tokens from Claude Code. These findings are based on reverse-engineering [`claude-code`](https://github.com/hakula139/claude-code) (v2.1.87) and testing against the production API.

## Authentication Methods

### API Key

Standard approach. Set `x-api-key` header directly.

### OAuth (Claude Code Credentials)

Claude Code stores OAuth tokens in **platform-specific secure storage** with a plaintext fallback:

- **macOS**: macOS Keychain (service `"Claude Code-credentials"`), falling back to `~/.claude/.credentials.json`.
- **Linux**: `~/.claude/.credentials.json` (plaintext only; libsecret support is planned but not yet implemented).

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

## Required Headers and Parameters

Three things are required for OAuth tokens to work with non-Haiku models:

### 1. Beta headers

```text
anthropic-beta: claude-code-20250219,oauth-2025-04-20
```

- `claude-code-20250219` — identifies the request as a Claude Code client.
- `oauth-2025-04-20` — enables OAuth authentication.

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

The accepted beta set differs per model family and per call type (agentic chat vs one-shot utility). Sending an unsupported beta — most commonly `context-1m-2025-08-07` to Haiku — trips gateway validation with HTTP 400 `invalid_request_error`. The mapping claude-code applies in `claude-code/src/utils/betas.ts`:

Rows are grouped by role: identity / auth → universal agentic → model-tier-gated. Within each group the broadest support comes first, producing a visual staircase of narrowing checkmarks.

Cell legend: `✓` always on, `—` not supported (or stripped), `[1m]` opt-in via the model suffix, `*` caller opt-in (body field + beta ship together, see rules below).

| Beta                              | Opus 4 (base) | Opus 4.1 / 4.5 | Opus 4.6+ | Sonnet 4 (base) | Sonnet 4.5 | Sonnet 4.6+ | Haiku 4 (base) | Haiku 4.5 (agentic) | Haiku 4.5 (one-shot) |
| --------------------------------- | ------------- | -------------- | --------- | --------------- | ---------- | ----------- | -------------- | ------------------- | -------------------- |
| `claude-code-20250219`            | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | —                    |
| `oauth-2025-04-20` (OAuth only)   | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | ✓                    |
| `context-management-2025-06-27`   | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | —                    |
| `prompt-caching-scope-2026-01-05` | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | ✓              | ✓                   | —                    |
| `interleaved-thinking-2025-05-14` | ✓             | ✓              | ✓         | ✓               | ✓          | ✓           | —              | —                   | —                    |
| `context-1m-2025-08-07`           | —             | —              | `[1m]`    | `[1m]`          | `[1m]`     | `[1m]`      | —              | —                   | —                    |
| `effort-2025-11-24`               | —             | —              | ✓         | —               | —          | ✓           | —              | —                   | —                    |
| `structured-outputs-2025-12-15`   | —             | `*`            | `*`       | —               | `*`        | `*`         | —              | `*`                 | `*`                  |

Key rules:

- **Haiku + `context-1m`** — rejected (Haiku has a 200K window); the `[1m]` tag is silently stripped rather than forwarded.
- **Haiku + `interleaved-thinking`** — third-party gateways reject it; first-party accepts.
- **Haiku one-shots** (title generation, compaction classifier) — strip agentic markers entirely. `claude-code-20250219` is re-added only when the call is agentic.
- **`prompt-caching-scope` requires a 1P base URL** — the beta only matters when a block carries `cache_control.scope: "global"`, which 3P gateways reject (see [Prompt Caching Scope](#prompt-caching-scope)). oxide-code gates the header on `is_first_party_base_url()` so requests going through a proxy ship neither the scope field nor its beta.
- **`context-1m` is user opt-in via `[1m]`** — appending `[1m]` to the model string (e.g., `claude-opus-4-7[1m]`) adds the 1M beta and strips the tag before the request hits the wire. Family-based auto-enable would 400 on subscriptions or gateways that don't carry 1M access. Convention matches claude-code.
- **`effort` is Opus 4.6+ and Sonnet 4.6+ only** — Opus 4.5 and older, Sonnet 4.5 and older, and all Haiku variants reject it per upstream's `modelSupportsEffort`. The per-level ceiling (`xhigh` on 4.7, `max` on Opus 4.6 / 4.7) is separately encoded in `Capabilities::effort_xhigh` / `effort_max`.
- **`effort` and `context-management` betas need a body field.** Sending the header alone is a silent no-op — the request runs at the server default. See [Agentic Request Body Fields](#agentic-request-body-fields) for the matching `output_config.effort` and `context_management.edits` shapes. oxide-code pairs each capability with both its beta and its body field so the two stay in sync.
- **`structured-outputs` is per-version and caller-opt-in** — the upstream allowlist is Opus 4.1 / 4.5 / 4.6+, Sonnet 4.5 / 4.6+, Haiku 4.5. The beta ships only when a caller supplies an `output_config.format` (today: the AI-title generator). The body field and header are paired on the same capability flag: a schema passed to an unsupported model silently falls back to free-form text, mirroring the `[1m]` × `context_1m` silent-strip pattern.
- **Unknown model aliases** fall through substring matching on the family stem. `claude-opus-5-x` would miss every row and ship with only the identity / caching betas; bump the `MODELS` table when a new family lands.

oxide-code gates each beta header on the target model in `client::anthropic::compute_betas`, which consults the ground-truth `Capabilities` flags in `crate::model::MODELS`. New models ship by adding a row to that table — no changes to the beta logic needed.

### 2. System prompt prefix (as a separate block)

The `system` parameter must be sent as an **array of text blocks**, not a plain string. The identity prefix must occupy its own block:

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
{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.87.a3f; cc_entrypoint=cli; cch=1b4e2;"}
```

Format: `x-anthropic-billing-header: cc_version=<VERSION>.<FINGERPRINT>; cc_entrypoint=<ENTRYPOINT>; cch=<HASH>;`

#### Fingerprint (3-char version suffix)

A 3-character hex value derived from conversation content:

1. Extract characters at indices `[4, 7, 20]` from the first user message text (use `"0"` if index is out of bounds).
2. Compute `SHA256(SALT + chars + VERSION)`, take the first 3 hex characters.
3. Salt: `59cf53e54c78` (hardcoded in `claude-code/src/utils/fingerprint.ts`, must match server).

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

Tool definitions render before system blocks. If any earlier block carries a narrower cache scope — or if the gateway treats missing `cache_control` on tools as narrower — the server rejects the global block with HTTP 400:

> `cache_control.scope: "global"` is only valid when every preceding block is also globally scoped. A block with `scope: "global"` was found after content with a narrower cache scope.

### Gateway behavior differs

- **1P (`api.anthropic.com`)**: accepts `scope: "global"` on the static system block even when tools are present — the server model is lenient about tool-definition scope.
- **3P proxies / self-hosted gateways**: enforce strict prefix invariance. Any `scope: "global"` block downstream of tools is rejected. The fix path is to drop the scope field (the block still caches at the default org level).

### oxide-code gating

oxide-code gates `scope: "global"` on `is_first_party_base_url(&config.base_url)`:

- Base URL host matches `api.anthropic.com` or `api-staging.anthropic.com` → `{"type": "ephemeral", "scope": "global"}` + `prompt-caching-scope-2026-01-05` beta.
- Any other host (proxies, self-hosted, malformed URLs) → `{"type": "ephemeral"}`; the beta is dropped since it's a no-op without the scope field.

The shape is otherwise identical in both modes: same static / dynamic section split, same boundary marker, same block order. Only the two 1P-only elements toggle.

This matches the broader pattern of gating features like fine-grained tool streaming and client-request-ID injection on base URL rather than on the provider enum alone — the provider flag says "not Bedrock / not Vertex", but a user pointing `ANTHROPIC_BASE_URL` at a proxy still parses as first-party by that check.

## Agentic Request Body Fields

Some capabilities live in the request body alongside (not instead of) the `anthropic-beta` header that gates them. Shipping the header but omitting the body field is a silent no-op — the feature doesn't activate. All three fields below were captured live from `claude-code 2.1.119` and cross-checked against the official migration guide.

### `output_config.effort`

GA as of Opus 4.6. Controls the intelligence-vs-latency tier of agentic turns via one of five tokens: `low`, `medium`, `high`, `xhigh`, `max`.

```json
{
  "output_config": { "effort": "xhigh" }
}
```

- **The `effort-2025-11-24` beta header is necessary but not sufficient.** oxide-code used to send the header without the body field; the header became a no-op and the model ran at an undefined default.
- **Per-model ceiling.** `max` is Opus-only; Sonnet 4.6 400s on it. `xhigh` is Opus 4.7-only. The `Capabilities::effort_max` / `effort_xhigh` flags encode this; `Capabilities::clamp_effort` clamps a user pick down to the highest supported level at or below it.
- **Per-model default.** claude-code 2.1.119 sends `xhigh` on Opus 4.7, `high` on Opus 4.6 and Sonnet 4.6, omits the field entirely on earlier models. oxide-code mirrors this via `Capabilities::default_effort`.
- **`max_tokens` should scale with effort.** claude-code uses 64 K on Opus 4.7 at `xhigh`, 32 K on Sonnet 4.6 at `high`. oxide-code's `default_max_tokens(effort)` matches the upper tiers and uses 16 K otherwise when the user hasn't set `ANTHROPIC_MAX_TOKENS` explicitly.

### `context_management.edits`

Partners the `context-management-2025-06-27` beta header. claude-code ships the same directive on every 4.6+ request:

```json
{
  "context_management": {
    "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
  }
}
```

oxide-code applies the body-header coupling as an invariant: the body field is populated on every request whose model has `Capabilities::context_management` set, i.e. the same condition that enables the beta header. One-shot completions (the `complete` path in `client::anthropic`) skip both — matches the reference wire and keeps the title-generation path minimal.

### `cache_control.ttl`

Anthropic silently dropped the default ephemeral-cache TTL from 1 h to 5 m on 2026-03-06 — a 40-55 % savings regression on any session longer than 5 min. The opt-in is a body field, not a beta:

```json
{
  "cache_control": { "type": "ephemeral", "ttl": "1h" }
}
```

Accepted values: `"5m"` (server default, equivalent to omitting the field) and `"1h"` (opt-in at higher write premium). No beta header is required — the field is GA.

**oxide-code default.** `prompt_cache_ttl = "1h"`. The hit-rate recovery on real agent sessions (tool-use loops, resumed conversations) dominates the write premium, so 1 h is the right safe default. Users opt down via `[client].prompt_cache_ttl = "5m"` or `OX_PROMPT_CACHE_TTL=5m`.

Invalidation order (from the Anthropic caching docs) is `tools → system → messages` — any change at a level busts that level and every level after it. oxide-code attaches a single `cache_control` to the static system-prompt prefix block (scope-gated on 1 P / 3 P per the previous section); the TTL rides through on both paths.

### `thinking.display`

See [Extended Thinking § Display modes (Opus 4.7+)](./extended-thinking.md#display-modes-opus-47). Opus 4.7 silently flipped the default to `"omitted"`; `show_thinking=true` in oxide-code opts back into `"summarized"`.

## Third-Party Tool Restrictions

As of April 4, 2026, Anthropic enforces that OAuth subscription credits (Pro / Max) are only valid for official Claude Code and claude.ai clients. Third-party tools that reuse the OAuth flow are classified as "third-party harness traffic" and must use either:

- **API key** (`ANTHROPIC_API_KEY`) with standard per-token billing.
- **Extra Usage** billing enabled on the account, which allows OAuth but bills per-token beyond the subscription.

The `cch` hash is the primary technical enforcement mechanism. The algorithm (xxHash64, non-cryptographic) and constants are publicly known. No additional protections exist: no TLS fingerprinting, binary attestation, pre-registration handshake, replay detection, or connection association. Anthropic could escalate enforcement at any time — the current scheme is billing plumbing, not a security boundary.

oxide-code computes valid `cch` hashes for OAuth requests. The fingerprint salt and xxHash64 seed are version-specific constants; they may change with Claude Code releases.

## API Version

The `anthropic-version` header is `2023-06-01` across all claude-code endpoints. This is the only stable API version.

## Model IDs

The API model ID for Opus 4.6 is `claude-opus-4-6`. The `[1m]` suffix (e.g., `claude-opus-4-6[1m]`) is a client-side convention that claude-code strips before sending to the API via `normalizeModelStringForAPI()`. The 1M context window is activated by the `context-1m-2025-08-07` beta header, not the model ID.

## SDK vs Raw HTTP

Claude Code uses the Anthropic TypeScript SDK with `authToken` (not `apiKey`) for OAuth. The SDK:

- Sends `Authorization: Bearer <token>` internally.
- Adds `?beta=true` query parameter to the URL.
- Includes `x-stainless-*` headers (SDK telemetry).
- Retries on `x-should-retry: true` responses with exponential backoff.

For raw HTTP (as in oxide-code), replicate the headers manually. The `?beta=true` query parameter and `x-stainless-*` headers are not required.

## Token Refresh

OAuth tokens expire (check `expiresAt` in milliseconds). Claude Code refreshes them automatically with a 5-minute buffer before expiry, using a `POST` to `platform.claude.com/v1/oauth/token` with the `refresh_token`. Cross-process safety is handled via directory-based locking (`proper-lockfile` creates a `~/.claude.lock/` directory).

oxide-code implements the same refresh flow: proactive refresh with the 5-minute buffer, directory-based locking compatible with Claude Code, and credential write-back preserving unknown fields.

## Sources

- `claude-code/src/constants/betas.ts` — beta header constants
- `claude-code/src/constants/oauth.ts` — OAuth client ID, token URL, scopes
- `claude-code/src/constants/system.ts` — system prompt prefix, attribution header construction
- `claude-code/src/services/api/claude.ts` — system block assembly, `buildSystemPromptBlocks`
- `claude-code/src/services/api/client.ts` — SDK client construction
- `claude-code/src/services/oauth/client.ts` — token refresh endpoint and request format
- `claude-code/src/utils/api.ts` — `splitSysPromptPrefix`, cache scope assignment
- `claude-code/src/utils/auth.ts` — OAuth token retrieval and refresh
- `claude-code/src/utils/betas.ts` — per-model beta header computation
- `claude-code/src/utils/fingerprint.ts` — 3-char SHA-256 fingerprint (salt, indices, computation)
- `claude-code/src/utils/http.ts` — auth headers, User-Agent construction
- `claude-code/src/utils/userAgent.ts` — `claude-cli/<version>` User-Agent format
- `claude-code/src/utils/secureStorage/index.ts` — platform-specific storage dispatch
- `claude-code/src/utils/secureStorage/macOsKeychainStorage.ts` — macOS Keychain backend
- `claude-code/src/utils/secureStorage/plainTextStorage.ts` — credential file I/O

Body-field research is empirical rather than source-backed: the `output_config.effort`, `context_management.edits`, and `cache_control.ttl` wire shapes documented above were captured live from a `claude-code --bare -p --model claude-opus-4-7` session against a local SSE proxy on 2026-04-24 and cross-referenced with the [Opus 4.7 migration guide](https://platform.claude.com/docs/en/about-claude/models/migration-guide) and [Anthropic prompt-caching docs](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching).
