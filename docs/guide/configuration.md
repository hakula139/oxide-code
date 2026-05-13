# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults.**
2. **User config file**: `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **Project config file**: the nearest `ox.toml` found by walking up from the current directory. Project config cannot set `client.api_key`, `client.base_url`, or `client.extra_ca_certs`.
4. **Environment variables**: always win.

## Config file

All fields are optional. Only specify the values you want to override.

```toml
# ~/.config/ox/config.toml (user-wide)
# or ox.toml in your project root (per-project, except api_key/base_url)

[client]
model = "claude-sonnet-4-6"
effort = "high"
max_tokens = 32000
prompt_cache_ttl = "1h"

[client.compaction]
auto_threshold_tokens = 400000

[tui]
show_thinking = true
```

### `[client]`: API connection

| Key                | Type    | Default                     | Description                                                |
| ------------------ | ------- | --------------------------- | ---------------------------------------------------------- |
| `api_key`          | string  | -                           | Anthropic API key; user config only                        |
| `base_url`         | string  | `https://api.anthropic.com` | API base URL; user config only                             |
| `extra_ca_certs`   | string  | -                           | PEM bundle appended to the trust store; user config only   |
| `model`            | string  | `claude-opus-4-7[1m]`       | Model to use                                               |
| `effort`           | string  | per-model (see below)       | Intelligence-vs-latency tier                               |
| `max_tokens`       | integer | effort-derived (see below)  | Max tokens per response                                    |
| `max_tool_rounds`  | integer | unset (unbounded)           | Per-turn safety cap on tool rounds                         |
| `prompt_cache_ttl` | string  | `"1h"`                      | Prompt-cache TTL (`"5m"` or `"1h"`)                        |

#### `effort`: intelligence tier

`effort` maps 1:1 to the `output_config.effort` body field. Accepted values: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Values above a model's per-model ceiling are silently clamped down to the highest supported level (so `"xhigh"` on Sonnet 4.6 becomes `"high"`). Models that don't accept the parameter at all (Sonnet 4.5 and older, Haiku, Opus 4.5 and older) drop it entirely from the request.

Per-model defaults when `effort` is unset:

| Model           | Default |
| --------------- | ------- |
| Opus 4.7        | `xhigh` |
| Opus 4.6        | `high`  |
| Sonnet 4.6      | `high`  |
| Everything else | (unset) |

Tier guide (from the [Opus 4.7 migration guide](https://platform.claude.com/docs/en/about-claude/models/migration-guide)):

- `max`: Deepest reasoning, Opus-only, with diminishing returns on some tasks.
- `xhigh`: Recommended default for coding and agentic work on Opus 4.7.
- `high`: Balanced, minimum recommended for intelligence-sensitive tasks.
- `medium`: Cost-sensitive workloads.
- `low`: Scoped, latency-sensitive tasks.

#### `max_tokens`: response ceiling

When unset, oxide-code derives `max_tokens` from the resolved `effort`: 64 000 for `xhigh` / `max`, 32 000 for `high`, 16 000 otherwise. Setting `max_tokens` explicitly (via TOML or `ANTHROPIC_MAX_TOKENS`) overrides the derivation.

#### `max_tool_rounds`: agent-loop safety cap

When unset, the agent loop has no per-turn round cap. Setting `max_tool_rounds = N` (or `OX_MAX_TOOL_ROUNDS=N`) bails the turn after `N` rounds with a runaway-loop error. The cap is a guard against tools stuck in a retry loop, since normal agent sessions routinely run hundreds of rounds.

#### `base_url`: endpoint

Use `base_url` only in `~/.config/ox/config.toml` or `ANTHROPIC_BASE_URL`. Project `ox.toml` cannot set it, because project files are loaded from the checkout and should not be able to redirect credentials. The URL must use HTTPS unless it points at localhost for a local proxy.

#### `extra_ca_certs`: corporate trust anchors

oxide-code uses `rustls` with the built-in Mozilla CA bundle, so self-signed or private-CA endpoints like a corporate gateway fail with `invalid peer certificate: UnknownIssuer`. Point `extra_ca_certs` at a PEM bundle (one or more `-----BEGIN CERTIFICATE-----` blocks in one file) to append those roots to the trust store:

```toml
[client]
base_url = "https://gw.llm.corp.example/anthropic"
extra_ca_certs = "~/.config/ox/corp-cachain.pem"
```

The path accepts `~/` / `~` for `$HOME`. It is user-config only (and rejected in project `ox.toml`) because a checked-in trust-anchor path could widen TLS trust for the process. Equivalent env var: `OX_EXTRA_CA_CERTS`.

#### `prompt_cache_ttl`: cache duration

Accepted values: `"5m"` (matches the server default as of 2026-03-06) and `"1h"` (higher write premium, bigger hit-rate win on long sessions). oxide-code defaults to `"1h"` because Anthropic's silent 2026-03 TTL drop cut typical prompt-caching savings from 80 %+ to 40-55 %. See [Agentic Request Body Fields](../research/api/anthropic.md#agentic-request-body-fields) for the wire shape and cost analysis.

### `[client.compaction]`: context compression

Auto-compaction is enabled by default for known model context windows. The default trigger leaves room for the next response and a safety buffer. Set one threshold override when you want compaction to happen earlier:

| Key                      | Type    | Default       | Description                                               |
| ------------------------ | ------- | ------------- | --------------------------------------------------------- |
| `auto_enabled`           | boolean | `true`        | Enable automatic context compaction                       |
| `auto_threshold_tokens`  | integer | model-derived | Absolute trigger, snapped into `50000`-safe-trigger range |
| `auto_threshold_percent` | integer | model-derived | Percent of context, snapped into the same range           |

`auto_threshold_tokens` and `auto_threshold_percent` are mutually exclusive. Both are clamped into the usable range: values below `50000` tokens snap up to that floor, and values above the model-derived safe trigger snap down. The model-derived ceiling shifts with `/model`, so a global threshold sized for a larger context window keeps working after a swap. Percent must be `1-100`; resolved tokens follow the same snap-into-range rule.

For models without known context windows, the default and percent-based automatic triggers stay off. An explicit token threshold still works, snapped to the `50000` floor when set lower.

#### 1M Context Window: `[1m]` Tag

Append `[1m]` to `model` to opt into the 1M-token context window on models that support it (any Sonnet 4.x, plus Opus 4.6 and newer):

```toml
[client]
model = "claude-opus-4-7[1m]"
```

1M access depends on your subscription or gateway, so you have to opt in explicitly. The tag is silently ignored on models without 1M support (e.g. Haiku).

### `[tui]`: Terminal UI

| Key             | Type    | Default | Description                               |
| --------------- | ------- | ------- | ----------------------------------------- |
| `show_thinking` | boolean | `false` | Show extended thinking                    |
| `show_welcome`  | boolean | `true`  | Paint the welcome splash on an empty chat |

On Opus 4.7, `show_thinking = true` opts the request into `thinking.display = "summarized"` so the API streams reasoning text. Otherwise the 4.7 default of `"omitted"` applies and the UI sees nothing until the final answer arrives.

`show_welcome = false` blanks the chat region until you submit a prompt, which is useful when piping or screen-recording.

### `[tui.theme]`: Terminal theme

| Key         | Type   | Default   | Description                                      |
| ----------- | ------ | --------- | ------------------------------------------------ |
| `base`      | string | `"mocha"` | Built-in theme name OR filesystem path to a TOML |
| `overrides` | table  | -         | Per-slot patches applied on top of the base      |

```toml
[tui.theme]
base = "latte"

[tui.theme.overrides]
error = "#ff0000"
accent = { bold = false }
```

See [Theming](theming.md) for the full slot reference, color value formats (hex, ANSI names, indexed, `reset`), and override semantics.

## Authentication

oxide-code checks three credential sources in order:

1. `ANTHROPIC_API_KEY` environment variable.
2. `api_key` under `[client]` in the user config file.
3. Claude Code OAuth credentials, if [Claude Code](https://code.claude.com/docs) is installed and signed in:
   - **macOS**: the `"Claude Code-credentials"` Keychain entry (preferred), falling back to `~/.claude/.credentials.json`.
   - **Linux**: `~/.claude/.credentials.json`.

   Expired tokens are refreshed automatically. No configuration needed.

Prefer the environment variable (or OAuth) over `api_key` in a config file. `ox.toml` resolves by walking up from the current directory, so oxide-code rejects project-level `api_key` and `base_url`; user-level `~/.config/ox/config.toml` is safer but still plaintext on disk.

## Environment variables

Environment variables override all config file values.

| Variable                               | Config key                                 | Default                     | Description                  |
| -------------------------------------- | ------------------------------------------ | --------------------------- | ---------------------------- |
| `ANTHROPIC_API_KEY`                    | `client.api_key`                           | -                           | Anthropic API key            |
| `ANTHROPIC_BASE_URL`                   | `client.base_url`                          | `https://api.anthropic.com` | API base URL                 |
| `ANTHROPIC_MODEL`                      | `client.model`                             | `claude-opus-4-7[1m]`       | Model to use                 |
| `ANTHROPIC_EFFORT`                     | `client.effort`                            | per-model                   | Intelligence-vs-latency tier |
| `ANTHROPIC_MAX_TOKENS`                 | `client.max_tokens`                        | effort-derived              | Max tokens per response      |
| `OX_EXTRA_CA_CERTS`                    | `client.extra_ca_certs`                    | -                           | Path to a PEM trust bundle   |
| `OX_MAX_TOOL_ROUNDS`                   | `client.max_tool_rounds`                   | unset (unbounded)           | Per-turn tool-round cap      |
| `OX_PROMPT_CACHE_TTL`                  | `client.prompt_cache_ttl`                  | `1h`                        | Prompt-cache TTL             |
| `OX_COMPACTION_AUTO_ENABLED`           | `client.compaction.auto_enabled`           | `true`                      | Enable auto-compaction       |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`  | `client.compaction.auto_threshold_tokens`  | model-derived               | Absolute compaction trigger  |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT` | `client.compaction.auto_threshold_percent` | model-derived               | Percent compaction trigger   |
| `OX_SHOW_THINKING`                     | `tui.show_thinking`                        | `false`                     | Show extended thinking       |
| `OX_SHOW_WELCOME`                      | `tui.show_welcome`                         | `true`                      | Paint the welcome splash     |

Set `OX_SHOW_THINKING=1` to display the model's thinking process (dimmed text) when extended thinking is enabled for the model.

Set `OX_SHOW_WELCOME=0` to suppress the welcome splash on an empty chat.
