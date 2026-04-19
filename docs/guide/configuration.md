# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults** — hardcoded in the binary.
2. **User config file** — `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **Project config file** — the nearest `ox.toml` found by walking from the current directory upward.
4. **Environment variables** — highest priority, always win.

Each source only needs to specify the values it wants to override. Unset fields fall through to the next lower-priority source.

## Config file

Create a TOML file at either location:

```toml
# ~/.config/ox/config.toml (user-wide)
# or ox.toml in your project root (per-project)

[client]
model = "claude-sonnet-4-6"
base_url = "https://api.anthropic.com"
max_tokens = 8192
# api_key = "sk-ant-..."   # prefer the environment variable for secrets

[tui]
show_thinking = true
```

All sections and fields are optional. Project config (`ox.toml`) overrides user config (`~/.config/ox/config.toml`).

### `[client]` — API connection

| Key          | Type    | Default                     | Description             |
| ------------ | ------- | --------------------------- | ----------------------- |
| `api_key`    | string  | —                           | Anthropic API key       |
| `model`      | string  | `claude-opus-4-7`           | Model to use            |
| `base_url`   | string  | `https://api.anthropic.com` | API base URL            |
| `max_tokens` | integer | `16384`                     | Max tokens per response |

### `[tui]` — Terminal UI

| Key             | Type    | Default | Description            |
| --------------- | ------- | ------- | ---------------------- |
| `show_thinking` | boolean | `false` | Show extended thinking |

## Authentication

oxide-code checks three credential sources in order:

### 1. API key (environment variable)

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

This is the highest-priority method. The key is sent directly in the `x-api-key` header.

### 2. API key (config file)

Set `api_key` under `[client]` in your user or project config file. The environment variable takes precedence if both are set.

### 3. Claude Code OAuth

If no API key is found, oxide-code reads OAuth credentials created by [Claude Code](https://code.claude.com/docs):

1. **macOS Keychain** — the `"Claude Code-credentials"` service entry, accessed via the `security-framework` crate.
2. **Credentials file** — `~/.claude/.credentials.json`.

On macOS, the Keychain is the authoritative source — preferred whenever present, with the credentials file as a fallback so a local file with inflated `expiresAt` cannot override a valid Keychain entry. Expired tokens are refreshed automatically. On Linux, only the file source is available (no Keychain support).

You do not need to configure anything — if Claude Code is installed and authenticated, oxide-code picks up its credentials automatically.

## Environment variables

Environment variables override all config file values.

| Variable               | Config key          | Default                     | Description             |
| ---------------------- | ------------------- | --------------------------- | ----------------------- |
| `ANTHROPIC_API_KEY`    | `client.api_key`    | —                           | Anthropic API key       |
| `ANTHROPIC_MODEL`      | `client.model`      | `claude-opus-4-7`           | Model to use            |
| `ANTHROPIC_BASE_URL`   | `client.base_url`   | `https://api.anthropic.com` | API base URL            |
| `ANTHROPIC_MAX_TOKENS` | `client.max_tokens` | `16384`                     | Max tokens per response |
| `OX_SHOW_THINKING`     | `tui.show_thinking` | `false`                     | Show extended thinking  |

Set `OX_SHOW_THINKING=1` to display the model's thinking process (dimmed text) when extended thinking is enabled for the model.
