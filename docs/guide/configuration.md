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
effort = "high"
max_tokens = 32000
prompt_cache_ttl = "1h"
# api_key = "sk-ant-..."   # prefer the environment variable for secrets

[tui]
show_thinking = true
```

### `[client]` — API connection

| Key                | Type    | Default                     | Description                         |
| ------------------ | ------- | --------------------------- | ----------------------------------- |
| `api_key`          | string  | —                           | Anthropic API key                   |
| `base_url`         | string  | `https://api.anthropic.com` | API base URL                        |
| `model`            | string  | `claude-opus-4-7`           | Model to use                        |
| `effort`           | string  | per-model (see below)       | Intelligence-vs-latency tier        |
| `max_tokens`       | integer | effort-derived (see below)  | Max tokens per response             |
| `prompt_cache_ttl` | string  | `"1h"`                      | Prompt-cache TTL (`"5m"` or `"1h"`) |

#### `effort` — intelligence tier

`effort` maps 1:1 to the `output_config.effort` body field. Accepted values: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Values above a model's per-model ceiling are silently clamped down to the highest supported level (so `"xhigh"` on Sonnet 4.6 becomes `"high"`). Models that don't accept the parameter at all (Sonnet 4.5 and older, Haiku, Opus 4.5 and older) drop it entirely from the request.

Per-model defaults when `effort` is unset:

| Model           | Default |
| --------------- | ------- |
| Opus 4.7        | `xhigh` |
| Opus 4.6        | `high`  |
| Sonnet 4.6      | `high`  |
| Everything else | (unset) |

Tier guide (from the [Opus 4.7 migration guide](https://platform.claude.com/docs/en/about-claude/models/migration-guide)):

- `max` — deepest reasoning, Opus-only; diminishing returns on some tasks.
- `xhigh` — recommended default for coding and agentic work on Opus 4.7.
- `high` — balanced; minimum recommended for intelligence-sensitive tasks.
- `medium` — cost-sensitive workloads.
- `low` — scoped, latency-sensitive tasks.

#### `max_tokens` — response ceiling

When unset, oxide-code derives `max_tokens` from the resolved `effort` to match the claude-code reference: 64 000 for `xhigh` / `max`, 32 000 for `high`, 16 384 otherwise. Setting `max_tokens` explicitly (via TOML or `ANTHROPIC_MAX_TOKENS`) overrides the derivation.

#### `prompt_cache_ttl` — cache duration

Accepted values: `"5m"` (matches the server default as of 2026-03-06) and `"1h"` (higher write premium, bigger hit-rate win on long sessions). oxide-code defaults to `"1h"` because Anthropic's silent 2026-03 TTL drop cut typical prompt-caching savings from 80 %+ to 40-55 %. See [Agentic Request Body Fields](../research/anthropic-api.md#agentic-request-body-fields) for the wire shape and cost analysis.

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

On Opus 4.7, `show_thinking = true` additionally opts the request into `thinking.display = "summarized"` so the API streams reasoning text; otherwise the 4.7 default (`"omitted"`) applies and the UI sees nothing until the final answer starts.

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

| Variable               | Config key                | Default                     | Description                  |
| ---------------------- | ------------------------- | --------------------------- | ---------------------------- |
| `ANTHROPIC_API_KEY`    | `client.api_key`          | —                           | Anthropic API key            |
| `ANTHROPIC_BASE_URL`   | `client.base_url`         | `https://api.anthropic.com` | API base URL                 |
| `ANTHROPIC_MODEL`      | `client.model`            | `claude-opus-4-7`           | Model to use                 |
| `ANTHROPIC_EFFORT`     | `client.effort`           | per-model                   | Intelligence-vs-latency tier |
| `ANTHROPIC_MAX_TOKENS` | `client.max_tokens`       | effort-derived              | Max tokens per response      |
| `OX_PROMPT_CACHE_TTL`  | `client.prompt_cache_ttl` | `1h`                        | Prompt-cache TTL             |
| `OX_SHOW_THINKING`     | `tui.show_thinking`       | `false`                     | Show extended thinking       |

Set `OX_SHOW_THINKING=1` to display the model's thinking process (dimmed text) when extended thinking is enabled for the model.
