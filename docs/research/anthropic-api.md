# Anthropic API Authentication

Research notes on how to authenticate with the Anthropic Messages API using OAuth tokens from Claude Code. These findings are based on reverse-engineering [`claude-code`](https://github.com/hakula139/claude-code) (v2.1.88) and testing against the production API.

## Authentication Methods

### API Key

Standard approach. Set `x-api-key` header directly.

### OAuth (Claude Code Credentials)

Claude Code stores OAuth tokens at `~/.claude/.credentials.json` (plaintext on Linux, macOS Keychain with plaintext fallback on macOS).

```json
{
  "claudeAiOauth": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": 1234567890000,
    "scopes": ["user:inference", "user:profile", "..."]
  }
}
```

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

### 2. System prompt prefix

```text
You are Claude Code, Anthropic's official CLI for Claude.
```

This must be the start of the system prompt. The API server uses it to identify legitimate Claude Code clients and apply correct rate limits. **Without this prefix, OAuth requests for Opus and Sonnet models return 429.**

### 3. Client identity header

```text
x-app: cli
```

## What Happens Without These

| Missing                | Haiku 4.5 | Sonnet / Opus |
| ---------------------- | --------- | ------------- |
| `claude-code-20250219` | 200       | 429           |
| `oauth-2025-04-20`     | 401       | 401           |
| System prompt prefix   | 200       | 429           |

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
- `claude-code/src/constants/system.ts` — system prompt prefix
- `claude-code/src/services/api/client.ts` — SDK client construction
- `claude-code/src/services/oauth/client.ts` — token refresh endpoint and request format
- `claude-code/src/utils/auth.ts` — OAuth token retrieval and refresh
- `claude-code/src/utils/secureStorage/plainTextStorage.ts` — credential file I/O
