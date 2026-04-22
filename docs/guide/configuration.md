# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults.**
2. **User config file** — `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **Project config file** — the nearest `ox.toml` found by walking up from the current directory.
4. **Environment variables** — always win.

## Config file

All fields are optional. Only specify the values you want to override.

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

### `[client]` — API connection

| Key          | Type    | Default                     | Description             |
| ------------ | ------- | --------------------------- | ----------------------- |
| `api_key`    | string  | —                           | Anthropic API key       |
| `model`      | string  | `claude-opus-4-7`           | Model to use            |
| `base_url`   | string  | `https://api.anthropic.com` | API base URL            |
| `max_tokens` | integer | `16384`                     | Max tokens per response |

#### 1M context window — `[1m]` tag

Append `[1m]` to `model` to opt into the 1M-token context window on models that support it (any Sonnet 4.x, plus Opus 4.6 and newer):

```toml
[client]
model = "claude-opus-4-7[1m]"
```

1M access depends on your subscription or gateway, so it is opt-in rather than automatic. The tag is silently ignored on models without 1M support (e.g. Haiku).

### `[tui]` — Terminal UI

| Key             | Type    | Default | Description            |
| --------------- | ------- | ------- | ---------------------- |
| `show_thinking` | boolean | `false` | Show extended thinking |

## Authentication

oxide-code checks three credential sources in order:

1. `ANTHROPIC_API_KEY` environment variable.
2. `api_key` under `[client]` in a config file.
3. Claude Code OAuth credentials, if [Claude Code](https://code.claude.com/docs) is installed and signed in:
    - **macOS** — the `"Claude Code-credentials"` Keychain entry (preferred), falling back to `~/.claude/.credentials.json`.
    - **Linux** — `~/.claude/.credentials.json`.

    Expired tokens are refreshed automatically. No configuration needed.

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
