# Authentication

Each request carries one credential. oxide-code uses the first source that is set, in Claude Code's order with its own login ahead of the Claude Code login:

| Source                                               | Sent as                         | Typical use                                           |
| ---------------------------------------------------- | ------------------------------- | ----------------------------------------------------- |
| 1. `ANTHROPIC_AUTH_TOKEN`                            | `Authorization: Bearer`         | [LLM gateways](#llm-gateways) that take bearer tokens |
| 2. `ANTHROPIC_API_KEY`                               | `x-api-key`                     | Anthropic Console API key                             |
| 3. `[client.auth]`                                   | Configured scheme               | Private inline credential or secret file              |
| 4. `CLAUDE_CODE_OAUTH_TOKEN`                         | `Authorization: Bearer` + OAuth | Long-lived token from `claude setup-token`            |
| 5. oxide-code login                                  | `Authorization: Bearer` + OAuth | Signed in with `/login`                               |
| 6. [Claude Code](https://code.claude.com/docs) login | `Authorization: Bearer` + OAuth | Claude Code installed and signed in                   |

- **`CLAUDE_CODE_OAUTH_TOKEN`** is sent as is, so it suits CI, headless machines, and secret managers. oxide-code never refreshes it and does not read either stored login while it is set. When the API rejects it as expired or revoked, the error names the variable, and `claude setup-token` issues a new one.
- **The oxide-code login** comes from [`/login`](#browser-sign-in), which stores it in `$XDG_DATA_HOME/ox/credentials.json` (default: `~/.local/share/ox/credentials.json`) with mode `0600`. `/logout` deletes that file and leaves the Claude Code login alone. When the oxide-code login exists but stops working, for example after it expires, oxide-code reports it with the fix and does not fall back to the Claude Code login.
- **The Claude Code login** comes from the `"Claude Code-credentials"` Keychain entry on macOS, falling back to `.credentials.json` in Claude Code's config directory, which is the only store on Linux. That directory is `~/.claude`, or `$CLAUDE_CONFIG_DIR` when you set it to run Claude Code under another account, and with the variable set, the Keychain entry is the one Claude Code keeps for that directory. Set the same `CLAUDE_CONFIG_DIR` for `ox` to use that account's login.
- **Both logins** refresh automatically near expiry and are sent only to the Anthropic API. With a custom `base_url` and no other credential, startup fails and names `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, and `CLAUDE_CODE_OAUTH_TOKEN`, and points at `[client.auth]`, so a personal subscription token never reaches a third-party host.
- **Without a credential**, `ox` starts signed out and shows how to sign in, and prompts fail until `/login` succeeds. When a stored login failed only for a passing reason, such as an unreachable token endpoint, the next prompt tries it again. `ox -p` and `ox --no-tui` exit at startup with the list of credential options.
- **`/status` and `/config`** show the active source, such as `OAuth (CLAUDE_CODE_OAUTH_TOKEN)`, without its value.

## Browser Sign-In

`/login` opens the Claude sign-in page in your browser, which returns to `ox` when you finish. If the browser does not open or runs on another machine, such as when `ox` runs over SSH, open the printed URL in any browser instead. After you sign in there, the page shows a code. Paste it at the prompt and press **Enter**. While sign-in waits, the prompt takes only the code, so send other prompts after sign-in ends. Other text stays in the input, and slash commands still run. Press **Esc** to cancel. Sign-in also ends automatically after 5 minutes.

After you approve access, oxide-code stores the login in `$XDG_DATA_HOME/ox/credentials.json` (default: `~/.local/share/ox/credentials.json`) and uses it for the current and later sessions.

`/logout` deletes that file. A session using it switches to the Claude Code login if one is available. Otherwise, it stays open signed out until the next `/login`. If the Claude Code login exists but cannot be used, for example because it expired, `/logout` explains why.

Both commands end by naming the credential that requests will now use. An environment variable or `[client.auth]` takes precedence over either login, and `/login` tells you when one of them keeps the session on that credential and how to remove it so the new login takes over after a restart. The table above lists the full order.

## Fixed Credentials

User configuration can select a fixed credential with `[client.auth]`:

```toml
[client.auth]
type = "bearer"
file = "~/.secrets/gateway-token"
```

`type` is `"bearer"` for an `Authorization: Bearer` gateway token, `"api-key"` for `x-api-key`, or `"oauth"` for a fixed OAuth token with the OAuth request shape. Set exactly one of `file` or `value`. A `file` path must be absolute or start with `~/`, and ox reads only the winning file once during startup, trimming trailing line endings. Restart ox to read a rotated file. `ox config check` validates the declaration without opening the credential file. A fixed OAuth token can use the configured endpoint and is never refreshed.

An inline `value` sits on disk in plaintext, so keep that user config private, conventionally mode `0600`. ox does not change its permissions. `type = "login"` takes no other keys and selects the remaining environment and stored-login fallback chain. Stored logins retain their first-party endpoint restriction.

## Commands and Credentials

Commands the assistant runs with `bash` cannot access `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_CUSTOM_HEADERS`, so no command finds your credential in its own environment. On Linux, the built-in [`credentials` rule](permissions.md#built-in-rules) also covers each process's environment under `/proc`, including `ox`'s own. A script that needs its own Anthropic key, such as a test suite that calls the API, must read it from another variable or a file. [Commands the Assistant Runs](permissions.md#commands-the-assistant-runs) lists the rest of the environment a command sees.

## LLM Gateways

User configuration supports `client.headers` as a map of header names to string values, `client.attribution` (default `true`), and `client.structured_outputs` (default `true`). These settings, authentication, the endpoint, and CA trust are user-only. A project config cannot set them.

oxide-code reads the gateway variables Claude Code uses, which override the corresponding file settings:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example.com/anthropic
export ANTHROPIC_AUTH_TOKEN=...
export ANTHROPIC_CUSTOM_HEADERS='X-Team: platform
X-Route: blue'
export NODE_EXTRA_CA_CERTS=~/.config/ox/corp-cachain.pem
```

- **`ANTHROPIC_AUTH_TOKEN`** authenticates with `Authorization: Bearer` and otherwise keeps the API-key request shape, without the OAuth beta.
- **`ANTHROPIC_CUSTOM_HEADERS`** takes one `Name: value` per line and skips blank lines. The headers go on every API request after the built-in ones, so they can replace defaults such as `User-Agent`. oxide-code owns `anthropic-beta`, `anthropic-version`, `content-type`, `x-claude-code-session-id`, and the header that carries the active credential (`x-api-key` for an API key, `Authorization` otherwise), so naming any of them is an error. The other credential header stays free, so a gateway can take its own `Authorization` alongside an API key. A line without a colon, an invalid name or value, a reserved name, or a repeated name stops startup with an error that gives the line number and never prints the value. `/config` lists the header names only.
- **`NODE_EXTRA_CA_CERTS`** works like [`extra_ca_certs`](configuration.md#extra_ca_certs-corporate-trust-anchors) and ranks last, after `OX_EXTRA_CA_CERTS` and `client.extra_ca_certs`. Shells often export it for npm, so like Node, oxide-code logs a warning and skips the bundle when it cannot read it.
- **`CLAUDE_CODE_ATTRIBUTION_HEADER=0`** drops the `x-anthropic-billing-header` attribution block from the system prompt. By default, every request carries it, whatever the credential, since some gateways check it to recognize Claude Code requests. As in Claude Code, the block on a custom base URL omits the per-request `cch` hash, so it stays the same from request to request.
- **`CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS=1`** drops `output_config.format` from session-title requests, for gateways whose upstream rejects it. Titles then rely on the prompt alone to return JSON.

oxide-code ignores `ANTHROPIC_MODEL`, which Claude Code reads for its own model choice. Set `OX_MODEL` or `client.model` instead.

oxide-code is stricter than Claude Code in three ways. It never sends a stored login to a custom base URL, it sends one credential even when several are set, and it rejects malformed `ANTHROPIC_CUSTOM_HEADERS` lines where Claude Code skips them.

A gateway that does not serve the configured model may return HTTP 404 with a plain-text `404 page not found` response, which `ox` reports as `Anthropic API found no such endpoint or model`. The gateway may list the models it serves at `GET /v1/models`.
