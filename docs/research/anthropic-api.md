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

| Header                            | Purpose                        |
| --------------------------------- | ------------------------------ |
| `interleaved-thinking-2025-05-14` | Extended thinking support      |
| `context-1m-2025-08-07`           | 1M context window              |
| `context-management-2025-06-27`   | Context management             |
| `prompt-caching-scope-2026-01-05` | Prompt caching                 |
| `effort-2025-11-24`               | Effort control                 |
| `advanced-tool-use-2025-11-20`    | Tool search (first-party only) |

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

### 3. Attribution header (optional, recommended)

Claude Code prepends an attribution header as the very first system block:

```json
{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.87.a3f; cc_entrypoint=cli;"}
```

Format: `x-anthropic-billing-header: cc_version=<VERSION>.<FINGERPRINT>; cc_entrypoint=<ENTRYPOINT>;`

The fingerprint is a 3-character hex value computed per request:

1. Extract characters at indices `[4, 7, 20]` from the first user message text (use `"0"` if index is out of bounds).
2. Compute `SHA256(SALT + chars + VERSION)`, take the first 3 hex characters.
3. Salt: `59cf53e54c78` (hardcoded, must match server).

The entrypoint is `cli` for interactive sessions.

When `NATIVE_CLIENT_ATTESTATION` is enabled (compile-time feature flag in Bun), the header also includes a `cch=00000` placeholder that Bun's native HTTP stack (Zig) overwrites with a computed attestation token before sending. This is a tamper-proof mechanism that third-party tools cannot replicate.

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

## Third-Party Tool Restrictions

As of April 4, 2026, Anthropic enforces that OAuth subscription credits (Pro / Max) are only valid for official Claude Code and claude.ai clients. Third-party tools that reuse the OAuth flow are classified as "third-party harness traffic" and must use either:

- **API key** (`ANTHROPIC_API_KEY`) with standard per-token billing.
- **Extra Usage** billing enabled on the account, which allows OAuth but bills per-token beyond the subscription.

The native client attestation (`cch` in the attribution header) is the primary technical enforcement mechanism. Third-party tools cannot compute the attestation token since it requires Anthropic's custom Bun binary. Without valid attestation, subscription-tier rate limits are not applied.

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
