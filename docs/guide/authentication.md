# Authentication

Each request uses the first configured credential source:

| Source                                               | Sent as                         | Typical use                                           |
| ---------------------------------------------------- | ------------------------------- | ----------------------------------------------------- |
| 1. `ANTHROPIC_AUTH_TOKEN`                            | `Authorization: Bearer`         | [LLM gateways](#llm-gateways) that take bearer tokens |
| 2. `ANTHROPIC_API_KEY`                               | `x-api-key`                     | Anthropic Console API key                             |
| 3. `[client.auth]`                                   | Configured scheme               | Private inline credential or secret file              |
| 4. `CLAUDE_CODE_OAUTH_TOKEN`                         | `Authorization: Bearer` + OAuth | Long-lived token from `claude setup-token`            |
| 5. oxide-code login                                  | `Authorization: Bearer` + OAuth | Signed in with `/login`                               |
| 6. [Claude Code](https://code.claude.com/docs) login | `Authorization: Bearer` + OAuth | Claude Code installed and signed in                   |

- **`CLAUDE_CODE_OAUTH_TOKEN`**: Sent verbatim without refresh, ignoring both stored logins (suited to CI, headless hosts, and secret managers). If rejected as expired or revoked, the error names the variable. Issue a new one with `claude setup-token`.
- **oxide-code login**: Created by [`/login`](#browser-sign-in). If it expires or fails, oxide-code reports the fix without falling back to the Claude Code login.
- **Claude Code login**: Read from the `"Claude Code-credentials"` macOS Keychain entry, falling back to `.credentials.json` in Claude Code's config directory (the only store on Linux). That directory is `~/.claude`, or `$CLAUDE_CONFIG_DIR` (for another Claude Code account, which also selects its Keychain entry).
- **Both logins**: Refresh automatically near expiry and go only to the Anthropic API. With a custom `base_url` and no other credential, startup fails and names valid sources so personal subscription tokens never reach third-party hosts.
- **Signed out**: Interactive `ox` starts signed out with sign-in instructions, failing prompts until `/login` succeeds (transient failures, such as an unreachable token endpoint, retry on the next prompt). `ox -p` exits at startup listing credential options.
- **Inspection**: `/status` and `/config` show the active source (such as `OAuth (CLAUDE_CODE_OAUTH_TOKEN)`) without its value.

## Browser Sign-In

1. Run `/login` to open the Claude sign-in page in your browser and return automatically when done.
2. If no browser opens (such as over SSH), open the printed URL elsewhere, sign in, paste the code at the prompt, and press **Enter**.

While waiting, the prompt accepts only the code (other text stays in the input box until sign-in ends, while slash commands still run). Press **Esc** to cancel, or wait for the 5-minute timeout.

The login saves to `$XDG_DATA_HOME/ox/credentials.json` (default `~/.local/share/ox/credentials.json`, mode `0600`) for current and future sessions. `/logout` deletes that file and switches to the Claude Code login if usable, or stays signed out until the next `/login` (explaining if an existing Claude Code login is expired or unusable).

Both commands report the active credential. If an environment variable or `[client.auth]` outranks the login, `/login` notes it and explains how to remove it before restarting.

## Fixed Credentials

Set a fixed credential in user config under `[client.auth]`:

```toml
[client.auth]
type = "bearer"
file = "~/.secrets/gateway-token"
```

| `type`      | Sends                                                             |
| ----------- | ----------------------------------------------------------------- |
| `"bearer"`  | A gateway token as `Authorization: Bearer`                        |
| `"api-key"` | An API key as `x-api-key`                                         |
| `"oauth"`   | A fixed OAuth token with the OAuth request shape, never refreshed |
| `"login"`   | The credential from sources 4 to 6 in the table above             |

`"login"` takes no other keys. Every other `type` requires exactly one of `file` or `value`:

- **`file`**: Absolute or `~/` path. `ox` reads only the winning file once at startup (trimming trailing line endings), so restart after rotating it. `ox config check` validates the declaration without opening the file.
- **`value`**: Plaintext on disk. Keep the config file private (conventionally mode `0600`). `ox` does not change its permissions.

Fixed OAuth tokens work with custom endpoints, while `"login"` keeps the Anthropic-only restriction for stored logins.

## Commands and Credentials

`bash` commands cannot see `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_CUSTOM_HEADERS`. This does not isolate other processes' environments or credential files from `bash` commands. Scripts needing an Anthropic key must read another variable or file. See [Commands the Assistant Runs](permissions.md#commands-the-assistant-runs) for the rest of the command environment.

## LLM Gateways

oxide-code supports Claude Code's gateway variables:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example.com/anthropic
export ANTHROPIC_AUTH_TOKEN=...
export ANTHROPIC_CUSTOM_HEADERS='X-Team: platform
X-Route: blue'
export NODE_EXTRA_CA_CERTS=~/.config/ox/corp-cachain.pem
```

Each variable overrides its user-only config key (in parentheses where applicable):

- **`ANTHROPIC_BASE_URL`** ([`client.base_url`](configuration.md#base_url-endpoint)): API endpoint.
- **`ANTHROPIC_AUTH_TOKEN`**: Sends `Authorization: Bearer` with the API-key request shape (no OAuth beta).
- **`ANTHROPIC_CUSTOM_HEADERS`** (`client.headers`, header-to-string map): One `Name: value` per line, skipping blank lines. Applied after built-in headers so they can replace defaults such as `User-Agent`. Reserved headers fail startup: `anthropic-beta`, `anthropic-version`, `content-type`, `x-claude-code-session-id`, and the active credential header (`x-api-key` for API keys, `Authorization` otherwise). The unused credential header stays free (such as a gateway's `Authorization` alongside an API key). Missing colons, invalid names or values, or reserved / duplicate names fail startup with the line number and never print values. `/config` lists header names only.
- **`NODE_EXTRA_CA_CERTS`**: Fallback for [`extra_ca_certs`](configuration.md#extra_ca_certs-corporate-trust-anchors) (see precedence there). Because shells often export it for npm, unreadable bundles log a warning and are skipped, matching Node.
- **`CLAUDE_CODE_ATTRIBUTION_HEADER=0`** (`client.attribution = false`): Drops the `x-anthropic-billing-header` system-prompt block. Sent by default regardless of credential so gateways recognize Claude Code requests. On custom base URLs, omits the per-request `cch` hash (matching Claude Code) so the block stays constant.
- **`CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS=1`** (`client.structured_outputs = false`): Drops `output_config.format` from session-title and permission-classifier requests for gateways that reject it, relying on prompts alone for JSON.

oxide-code ignores `ANTHROPIC_MODEL`. Set `OX_MODEL` or `client.model` instead.

If a gateway does not serve the configured model and returns HTTP 404 with plain-text `404 page not found`, `ox` reports `Anthropic API found no such endpoint or model`. Check `GET /v1/models` on the gateway for served models.
