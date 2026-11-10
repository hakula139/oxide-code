# Authentication

Each request carries one credential, from the first of these sources that is set:

| Source                                               | Sent as                         | Typical use                                           |
| ---------------------------------------------------- | ------------------------------- | ----------------------------------------------------- |
| 1. `ANTHROPIC_AUTH_TOKEN`                            | `Authorization: Bearer`         | [LLM gateways](#llm-gateways) that take bearer tokens |
| 2. `ANTHROPIC_API_KEY`                               | `x-api-key`                     | Anthropic Console API key                             |
| 3. `[client.auth]`                                   | Configured scheme               | Private inline credential or secret file              |
| 4. `CLAUDE_CODE_OAUTH_TOKEN`                         | `Authorization: Bearer` + OAuth | Long-lived token from `claude setup-token`            |
| 5. oxide-code login                                  | `Authorization: Bearer` + OAuth | Signed in with `/login`                               |
| 6. [Claude Code](https://code.claude.com/docs) login | `Authorization: Bearer` + OAuth | Claude Code installed and signed in                   |

- **`CLAUDE_CODE_OAUTH_TOKEN`** is sent as is, so it suits CI, headless machines, and secret managers. oxide-code never refreshes it and ignores both stored logins while it is set. When the API rejects it as expired or revoked, the error names the variable, and `claude setup-token` issues a new one.
- **The oxide-code login** comes from [`/login`](#browser-sign-in). When it exists but stops working, as after it expires, oxide-code reports it with the fix and does not fall back to the Claude Code login.
- **The Claude Code login** comes from the `"Claude Code-credentials"` Keychain entry on macOS, falling back to `.credentials.json` in Claude Code's config directory, the only store on Linux. That directory is `~/.claude`, or `$CLAUDE_CONFIG_DIR` when you run Claude Code under another account, which also selects the Keychain entry Claude Code keeps for it. Set the same `CLAUDE_CONFIG_DIR` for `ox` to use that account's login.
- **Both logins** refresh automatically near expiry and go only to the Anthropic API. With a custom `base_url` and no other credential, startup fails and names the other sources, so a personal subscription token never reaches a third-party host.
- **Without a credential**, `ox` starts signed out and shows how to sign in, and prompts fail until `/login` succeeds. When a stored login failed only for a passing reason, such as an unreachable token endpoint, the next prompt tries it again. `ox -p` exits at startup with the list of credential options.
- **`/status` and `/config`** show the active source, such as `OAuth (CLAUDE_CODE_OAUTH_TOKEN)`, without its value.

## Browser Sign-In

1. Run `/login`, which opens the Claude sign-in page in your browser and returns to `ox` when you finish.
2. If the browser does not open or runs on another machine, as when `ox` runs over SSH, open the printed URL in any browser, sign in, paste the code the page shows at the prompt, and press **Enter**.

While sign-in waits, the prompt takes only the code, so other text stays in the input until sign-in ends, while slash commands still run. Press **Esc** to cancel, and sign-in ends on its own after 5 minutes.

oxide-code stores the login in `$XDG_DATA_HOME/ox/credentials.json` (default: `~/.local/share/ox/credentials.json`) with mode `0600` and uses it for the current and later sessions. `/logout` deletes that file, and the session switches to the Claude Code login if one is available or otherwise stays open signed out until the next `/login`. If the Claude Code login exists but cannot be used, for example because it expired, `/logout` explains why.

Both commands end by naming the credential that requests now use. An environment variable or `[client.auth]` outranks either login, and `/login` tells you when one keeps the session on it and how to remove it so the new login takes over after a restart.

## Fixed Credentials

User configuration can select a fixed credential with `[client.auth]`:

```toml
[client.auth]
type = "bearer"
file = "~/.secrets/gateway-token"
```

| `type`      | Sends                                                                     |
| ----------- | ------------------------------------------------------------------------- |
| `"bearer"`  | A gateway token as `Authorization: Bearer`                                |
| `"api-key"` | An API key as `x-api-key`                                                 |
| `"oauth"`   | A fixed OAuth token with the OAuth request shape, never refreshed         |
| `"login"`   | Takes no other keys and selects the remaining environment and login chain |

Every type but `"login"` takes exactly one of `file` or `value`:

- **`file`** is a path that is absolute or starts with `~/`. `ox` reads only the winning file, once at startup, trimming trailing line endings, so restart `ox` after rotating it. `ox config check` validates the declaration without opening the file.
- **`value`** sits on disk in plaintext, so keep that user config private, conventionally mode `0600`. `ox` does not change its permissions.

A fixed OAuth token may go to the configured endpoint, while stored logins selected through `"login"` keep their Anthropic-only restriction.

## Commands and Credentials

Commands the assistant runs with `bash` cannot see `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_CUSTOM_HEADERS` in their environment. This does not isolate other processes' environments or credential files. A script that needs its own Anthropic key must read it from another variable or a file. [Commands the Assistant Runs](permissions.md#commands-the-assistant-runs) lists the rest of the environment a command sees.

## LLM Gateways

oxide-code reads the gateway variables Claude Code uses:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example.com/anthropic
export ANTHROPIC_AUTH_TOKEN=...
export ANTHROPIC_CUSTOM_HEADERS='X-Team: platform
X-Route: blue'
export NODE_EXTRA_CA_CERTS=~/.config/ox/corp-cachain.pem
```

Where a variable has a config key, set in user configuration only, the variable overrides it:

- **`ANTHROPIC_BASE_URL`** sets [`client.base_url`](configuration.md#base_url-endpoint).
- **`ANTHROPIC_AUTH_TOKEN`** authenticates with `Authorization: Bearer` and otherwise keeps the API-key request shape, without the OAuth beta.
- **`ANTHROPIC_CUSTOM_HEADERS`** (`client.headers`, a map of header names to string values) takes one `Name: value` per line and skips blank lines. The headers go on every API request after the built-in ones, so they can replace defaults such as `User-Agent`. Naming a header oxide-code owns is an error: `anthropic-beta`, `anthropic-version`, `content-type`, `x-claude-code-session-id`, and the active credential's header (`x-api-key` for an API key, `Authorization` otherwise). The other credential header stays free, so a gateway can take its own `Authorization` alongside an API key. A line without a colon, an invalid name or value, or a reserved or repeated name stops startup with an error that gives the line number and never prints the value. `/config` lists the header names only.
- **`NODE_EXTRA_CA_CERTS`** works like [`extra_ca_certs`](configuration.md#extra_ca_certs-corporate-trust-anchors), which gives its precedence. Shells often export it for npm, so like Node, oxide-code logs a warning and skips the bundle when it cannot read it.
- **`CLAUDE_CODE_ATTRIBUTION_HEADER=0`** (`client.attribution = false`) drops the `x-anthropic-billing-header` attribution block from the system prompt. By default, every request carries it, whatever the credential, since some gateways check it to recognize Claude Code requests. As in Claude Code, the block on a custom base URL omits the per-request `cch` hash, so it stays the same from request to request.
- **`CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS=1`** (`client.structured_outputs = false`) drops `output_config.format` from session-title and permission-classifier requests, for gateways whose upstream rejects it. Both then rely on the prompt alone to return JSON.

oxide-code ignores `ANTHROPIC_MODEL`, which Claude Code reads for its own model choice, so set `OX_MODEL` or `client.model` instead.

A gateway that does not serve the configured model may return HTTP 404 with a plain-text `404 page not found` response, which `ox` reports as `Anthropic API found no such endpoint or model`. The gateway may list the models it serves at `GET /v1/models`.
