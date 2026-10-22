# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults.**
2. **User config file**: `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **User drop-ins**: `*.toml` files in `~/.config/ox/config.d/` (or `$XDG_CONFIG_HOME/ox/config.d/`), in file-name order.
4. **Project config file**: `.ox/config.toml` in the nearest directory from the working directory up to the project root that has one. The project root is the nearest directory at or above the working directory that holds a `.git` directory or file, or the working directory outside a repository.
5. **Environment variables**: override every file. A command-line flag such as `--mode` overrides them in turn.

Run `/config` to see each resolved value next to the layer that set it: `default`, `user`, `config.d/<name>`, `project`, an environment variable such as `$ANTHROPIC_MODEL`, `--mode` for the permission mode set by that flag, or `session` after a `/model`, `/effort`, or `/mode` change or a **Shift+Tab** mode switch.

## Layers

### Merging

Each file sets only the keys it names, and a later layer wins key by key, so a drop-in that sets `client.effort` keeps the `client.model` from `config.toml`. Tables merge recursively, including `[tui.theme.overrides]`.

- **`permission.extra_roots` and the class lists under `permission.deny`, `permission.ask`, and `permission.allow`**, such as `permission.allow.exec`, concatenate, so the entries from every layer apply.
- **Other arrays**, such as `tui.status_line`, come whole from the last layer that sets them.
- **`auto_threshold_tokens` and `auto_threshold_percent`** act as one setting, so a layer that sets either replaces both.

### Drop-Ins

`config.d/` lets a dotfile manager, a Nix module, or a team script own one file without editing your `config.toml`. Drop-ins load after `config.toml`, sorted by file name byte by byte, so a numeric prefix such as `10-team.toml` or `50-nix.toml` sets their order. Hidden files, subdirectories, and files without a `.toml` extension are skipped, and a symlink loads its target.

Drop-ins share the `config.toml` schema and count as user config. An invalid drop-in, including a symlink whose target is missing, stops startup with an error that names the file. A malformed permission pattern or extra root names the file that holds it in the same way.

### Project Config

A project file comes from the checkout, so it can set only the `[client]`, `[permission]`, and `[tui]` tables. Within `[client]` it can set only `model`, `effort`, `max_tokens`, `max_tool_rounds`, `prompt_cache_ttl`, and the [`[client.compaction]`](#clientcompaction-context-compression) table, and within `[permission]` only `deny` and `ask`. Setting any other key fails the config load. This keeps a repository from redirecting your credentials, changing the endpoint or TLS trust, or widening what runs without asking. A project file may set `tui.theme.base` only to a built-in theme name, and the TUI refuses to start when it names a file. A TOML error in a project file gives only its line, column, and key, since the parser's message can quote a value.

The project file must also resolve inside the project root. A `.ox/config.toml` that resolves outside it, through a symlinked file or `.ox` directory, fails the config load with an error that names only the link. A link to a file inside the repository, such as a config shared across a monorepo, still works, unless the project root is your home directory or one of its ancestors, as with a dotfiles repository at `~`. There, a project file reached through any symlink fails the load.

## Config File

All fields are optional. Only specify the values you want to override.

```toml
# ~/.config/ox/config.toml, a drop-in in ~/.config/ox/config.d/,
# or .ox/config.toml in your project (except the user-only keys)

[client]
model = "claude-sonnet-5-5"
effort = "high"
max_tokens = 32000
prompt_cache_ttl = "1h"

[client.compaction]
auto_threshold_tokens = 400000

[tui]
show_thinking = true
status_line = [
  "current-dir",
  "git-branch",
  "pull-request",
  "model-with-effort",
  "context-used",
  "session-cost",
  "permission-mode",
  "run-state",
  "thread-title",
]
```

### `[client]`: API Connection

| Key                | Type    | Default                     | Description                                                                 |
| ------------------ | ------- | --------------------------- | --------------------------------------------------------------------------- |
| `api_key`          | string  | -                           | Anthropic API key, [user config only](#project-config)                      |
| `base_url`         | string  | `https://api.anthropic.com` | API base URL, [user config only](#project-config)                           |
| `extra_ca_certs`   | string  | -                           | PEM bundle appended to the trust store, [user config only](#project-config) |
| `model`            | string  | `claude-opus-5-5`           | Model to use                                                                |
| `effort`           | string  | per-model (see below)       | Intelligence-vs-latency tier                                                |
| `max_tokens`       | integer | per-model (see below)       | Max tokens per response                                                     |
| `max_tool_rounds`  | integer | unset (unbounded)           | Per-turn safety cap on tool rounds                                          |
| `prompt_cache_ttl` | string  | `"1h"`                      | Prompt-cache TTL (`"5m"` or `"1h"`)                                         |

#### `model`: Supported Models

oxide-code supports `claude-fable-5-1`, `claude-opus-5-5`, `claude-sonnet-5-5`, and `claude-haiku-5-5`. Each has a 1M-token context window and adaptive thinking. Fable 5.1 isn't available to every account, and some third-party gateways serve it. Requests to it fail where your account or gateway does not serve it. Any other ID, such as a gateway alias, is sent verbatim without `effort`, `thinking`, or context management. Its `max_tokens` defaults to `16000`, no cost estimate is provided, and automatic compaction stays off unless you set `auto_threshold_tokens`. oxide-code warns about unsupported IDs at startup. For a `claude-*` ID that names a model family or an ID with a `[1m]` suffix, it suggests the matching supported model.

#### `effort`: Intelligence Tier

`effort` maps 1:1 to the `output_config.effort` body field. Accepted values: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Every supported model accepts all five.

Per-model defaults when `effort` is unset, matching the API's own defaults:

| Model      | Default  |
| ---------- | -------- |
| Fable 5.1  | `high`   |
| Opus 5.5   | `medium` |
| Sonnet 5.5 | `high`   |
| Haiku 5.5  | `medium` |

A `/model` swap carries the current effort over to the new model.

This tier guide follows Anthropic's [effort documentation](https://platform.claude.com/docs/en/build-with-claude/effort) for Haiku 5.5, and for Sonnet 5.5 on agentic work:

- `max` / `xhigh`: Only where you have measured a quality gain, since turns run much longer and cost more.
- `high`: Knowledge work, harder or longer agent tasks, and strict instruction following.
- `medium`: The starting point for most work, including well-specified agentic coding and multistep tool use.
- `low`: Chat and short, latency-sensitive tasks. At this level, long agent tasks are more likely to stop early or skip verifying a change.

For Opus 5.5, the documentation names no starting tier and recommends an effort sweep on your own evaluations. Fable 5.1's recommended start is its `high` default.

#### `max_tokens`: Response Ceiling

When unset, `max_tokens` is `64000` on the supported models, half their 128K output ceiling. `effort` controls how much the model thinks, so this cap only bounds runaway output. Other IDs get `16000`, which fits the lower output ceilings of older or smaller models, and the default follows a `/model` swap. Set `max_tokens` (via TOML or `OX_MAX_TOKENS`) to change it.

#### `max_tool_rounds`: Agent-Loop Safety Cap

When unset, the agent loop has no per-turn round cap. Setting `max_tool_rounds = N` (or `OX_MAX_TOOL_ROUNDS=N`) ends the turn after `N` rounds with a runaway-loop error. The cap is a guard against tools stuck in a retry loop, since normal agent sessions routinely run hundreds of rounds.

#### `base_url`: Endpoint

Set `base_url` in user config or `ANTHROPIC_BASE_URL`. The URL must use HTTPS unless it points at localhost for a local proxy. Requests go to `/v1/messages` under it, so leave `/v1` off the URL. A trailing slash makes no difference.

#### `extra_ca_certs`: Corporate Trust Anchors

oxide-code verifies TLS certificates using the operating system's trust store, so a corporate CA installed system-wide works without extra configuration. For gateways and other private-CA endpoints whose root CA isn't in that store, set `extra_ca_certs` to a PEM bundle to add those roots to oxide-code's trust store:

```toml
[client]
base_url = "https://gw.llm.corp.example/anthropic"
extra_ca_certs = "~/.config/ox/corp-cachain.pem"
```

Use an absolute or `~`-rooted path. Relative paths resolve from the working directory `ox` starts in. Equivalent env var: `OX_EXTRA_CA_CERTS`, which wins over this field. `NODE_EXTRA_CA_CERTS` applies only when neither is set (see [LLM gateways](#llm-gateways)).

On macOS, a server certificate valid for longer than Apple's 825-day limit is accepted when it chains to a CA in the bundle, even one already installed system-wide, unless that CA is one of Apple's built-in roots.

#### `prompt_cache_ttl`: Cache Duration

Accepted values: `"5m"` (matches the server default as of 2026-03-06) and `"1h"` (higher write premium, bigger hit-rate win on long sessions). oxide-code defaults to `"1h"` because Anthropic's unannounced drop of the default TTL to 5 minutes in March 2026 cut typical prompt-caching savings from over 80% to 40-55%.

### `[client.compaction]`: Context Compression

Auto-compaction is enabled by default for the supported models. The default trigger leaves room for the next response and a safety buffer. Set one threshold override when you want compaction to happen earlier:

| Key                      | Type    | Default       | Description                                                                                |
| ------------------------ | ------- | ------------- | ------------------------------------------------------------------------------------------ |
| `auto_enabled`           | boolean | `true`        | Enable automatic context compaction                                                        |
| `auto_threshold_tokens`  | integer | model-derived | Token count that triggers compaction, clamped between `50000` and the model's safe trigger |
| `auto_threshold_percent` | integer | model-derived | Share of the context window that triggers compaction, clamped the same way                 |

`auto_threshold_tokens` and `auto_threshold_percent` are mutually exclusive. Percent must be between `1` and `100`. Either form resolves to a token count clamped into the usable range: values below `50000` tokens snap up to that floor, and values above the model-derived safe trigger snap down. The model-derived ceiling shifts with `/model`, so a global threshold sized for a larger context window keeps working after a swap.

For other model IDs, the default and percent-based automatic triggers stay off. An explicit token threshold still works, snapped to the `50000` floor when set lower.

If automatic compaction fails, oxide-code does not send the prompt, and the TUI puts it back in the input. Sending it again retries the compaction. After 3 failures in a row, automatic compaction pauses, so the next prompt goes out without it, and `/status` shows it as paused. A successful `/compact`, a `/model` or `/effort` change, `/clear`, `/archive`, or `/resume` turns it back on.

### `[tui]`: Terminal UI

| Key             | Type    | Default   | Description                               |
| --------------- | ------- | --------- | ----------------------------------------- |
| `show_thinking` | boolean | `false`   | Show extended thinking                    |
| `show_welcome`  | boolean | `true`    | Paint the welcome splash on an empty chat |
| `status_line`   | array   | see below | Ordered status-line segments              |

`show_thinking = true` opts the request into `thinking.display = "summarized"` so the API streams reasoning text, which the chat shows as it arrives and then collapses to a `Thought` header that **Ctrl+O** or a click expands. Otherwise, reasoning stays hidden, and the chat still shows the short progress notes the model writes before each tool call on every supported model except Haiku 5.5.

`show_welcome = false` blanks the chat region until you submit a prompt, which is useful when piping or screen-recording.

`status_line` lists segments from the table below in display order and must contain at least one. When unset, it is the list in the [example](#config-file): `current-dir`, `git-branch`, `pull-request`, `model-with-effort`, `context-used`, `session-cost`, `permission-mode`, `run-state`, and `thread-title`. Segments with no data, such as a missing git branch, an unopened pull request, context usage before the first completed turn, or usage limits under an API key, are omitted. The three pull request segments shell out to `gh pr view` so they require the GitHub CLI on PATH. Click one of them to open the PR, its checks, or its changed files. Terminals that open links on **Ctrl+click** or **Cmd+click** do the same. `pull-request-review` shows only on repositories whose branch protection requires a review, since GitHub reports no review decision otherwise. The separator takes its style from the active theme's `separator` slot and has no setting of its own.

| Segment               | Renders                                                       | Refresh         |
| --------------------- | ------------------------------------------------------------- | --------------- |
| `current-dir`         | Tildified working directory                                   | At startup      |
| `git-branch`          | Current branch (omitted on detached HEAD)                     | Every 5 s       |
| `pull-request`        | Open PR as clickable `#86` when OSC 8 works                   | Every 60 s      |
| `pull-request-checks` | `Checks: 2 failing`, `1 pending`, `1 cancelled`, or `passing` | Every 60 s      |
| `pull-request-review` | `Review: approved`, `changes requested`, or `required`        | Every 60 s      |
| `model`               | Compact model label (e.g., `Opus 5.5`)                        | On `/model`     |
| `model-with-effort`   | Model label plus the effort tier in parens                    | On `/model`     |
| `context-used`        | `Ctx: 10% (100k/1M)` after the first turn                     | Per turn        |
| `session-cost`        | `Sess: $0.4321` running USD estimate                          | Per turn        |
| `five-hour-limit`     | `5h: 42%` of a subscription's five-hour usage limit           | Per request     |
| `weekly-limit`        | `7d: 13%` of a subscription's weekly usage limit              | Per request     |
| `permission-mode`     | `Mode: plan` or `Mode: yolo`, omitted in `manual`             | On switch       |
| `run-state`           | `Ready` / spinner + label / **Ctrl+C** exit hint              | On state change |
| `queued-prompts`      | `Queued: 2` while prompts wait for the running turn           | On change       |
| `task-progress`       | `Tasks: 2/5` completed while the session keeps a task list    | On change       |
| `thread-title`        | Session title, AI-generated or set by `/rename`               | On change       |
| `session-id`          | `ID: 3f2a9c1e`, the prefix `ox -r` resumes                    | On change       |
| `theme`               | `Theme: mocha`, or a theme file's name                        | On `/theme`     |
| `current-time`        | `HH:MM` in the local timezone                                 | Per minute      |

The usage limits come from Claude subscription sign-ins, which report how much of each window is spent with every response. A window disappears when it resets and remains gone until the next response reports it again. The [theming guide](theming.md#status-line-segments) lists the colors used by the context and usage-limit segments as a window fills.

When the row is too narrow to fit every configured segment, low-utility ones drop in this order before the remaining content gets truncated: `theme` → `session-id` → `thread-title` → `current-time` → `pull-request-review` → `pull-request-checks` → `pull-request` → `git-branch` → `current-dir` → `task-progress` → `queued-prompts` → `session-cost` → `weekly-limit` → `five-hour-limit` → `context-used` → `model` → `model-with-effort` → `permission-mode` → `run-state`. Run state takes priority over every other segment, and the permission mode over the rest. The last remaining segment is truncated to fit rather than dropped.

`OX_STATUS_LINE` accepts the same segment names, comma-separated, with optional whitespace (`OX_STATUS_LINE="model, run-state"`). A value with no segments (`","`), an empty entry between commas (`"model,,run-state"`), or an unknown segment name fails startup with a parse error.

### `[tui.theme]`: Terminal Theme

| Key         | Type   | Default   | Description                                                   |
| ----------- | ------ | --------- | ------------------------------------------------------------- |
| `base`      | string | `"mocha"` | Built-in theme name, or a TOML theme file path in user config |
| `overrides` | table  | -         | Per-slot patches applied on top of the base                   |

```toml
[tui.theme]
base = "latte"

[tui.theme.overrides]
error = "#ff0000"
accent = { bold = false }
```

See [Theming](theming.md) for the full slot reference, color value formats (hex, ANSI names, indexed, `reset`), and override semantics.

### `[permission]`: Tool Approval

`[permission]` sets the permission mode, the deny, ask, and allow rules, the extra workspace roots, and which built-in rules apply. [Permissions](permissions.md) describes each key, the modes, the approval prompt, and the pattern grammar.

## Authentication

Each request carries one credential. oxide-code uses the first source that is set, in Claude Code's order with its own login ahead of the Claude Code login:

| Source                                               | Sent as                         | Typical use                                           |
| ---------------------------------------------------- | ------------------------------- | ----------------------------------------------------- |
| 1. `ANTHROPIC_AUTH_TOKEN`                            | `Authorization: Bearer`         | [LLM gateways](#llm-gateways) that take bearer tokens |
| 2. `ANTHROPIC_API_KEY`                               | `x-api-key`                     | Anthropic Console API key                             |
| 3. `api_key` under `[client]`                        | `x-api-key`                     | The same key, stored in the user config file          |
| 4. `CLAUDE_CODE_OAUTH_TOKEN`                         | `Authorization: Bearer` + OAuth | Long-lived token from `claude setup-token`            |
| 5. oxide-code login                                  | `Authorization: Bearer` + OAuth | Signed in with `/login`                               |
| 6. [Claude Code](https://code.claude.com/docs) login | `Authorization: Bearer` + OAuth | Claude Code installed and signed in                   |

- **`CLAUDE_CODE_OAUTH_TOKEN`** is sent as is, so it suits CI, headless machines, and secret managers. oxide-code never refreshes it and does not read either stored login while it is set. When the API rejects it as expired or revoked, the error names the variable, and `claude setup-token` issues a new one.
- **The oxide-code login** comes from [`/login`](slash-commands.md#signing-in), which stores it in `$XDG_DATA_HOME/ox/credentials.json` (default: `~/.local/share/ox/credentials.json`) with mode `0600`. `/logout` deletes that file and leaves the Claude Code login alone. When the oxide-code login exists but stops working, for example after it expires, oxide-code reports it with the fix and does not fall back to the Claude Code login.
- **The Claude Code login** comes from the `"Claude Code-credentials"` Keychain entry on macOS, falling back to `.credentials.json` in Claude Code's config directory, which is the only store on Linux. That directory is `~/.claude`, or `$CLAUDE_CONFIG_DIR` when you set it to run Claude Code under another account, and with the variable set, the Keychain entry is the one Claude Code keeps for that directory. Set the same `CLAUDE_CONFIG_DIR` for `ox` to use that account's login.
- **Both logins** refresh automatically near expiry and are sent only to the Anthropic API. With a custom `base_url` and no other credential, startup fails and names `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`, so a personal subscription token never reaches a third-party host.
- **Without a credential**, `ox` starts signed out and shows how to sign in, and prompts fail until `/login` succeeds. When a stored login failed only for a passing reason, such as an unreachable token endpoint, the next prompt tries it again. `ox -p` and `ox --no-tui` exit at startup with the list of credential options.
- **`/status` and `/config`** show the active source, such as `OAuth (CLAUDE_CODE_OAUTH_TOKEN)`, without its value.

The environment variables and the stored logins keep credentials out of config files. An `api_key` in `~/.config/ox/config.toml` works but sits on disk in plaintext.

Commands the assistant runs with `bash` cannot access `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, or `ANTHROPIC_CUSTOM_HEADERS`, so no command can print your credential from the environment. A script that needs its own Anthropic key, such as a test suite that calls the API, must read it from another variable or a file.

## LLM Gateways

oxide-code reads the gateway variables Claude Code uses:

```bash
export ANTHROPIC_BASE_URL=https://gateway.example.com/anthropic
export ANTHROPIC_AUTH_TOKEN=...
export ANTHROPIC_CUSTOM_HEADERS='X-Team: platform
X-Route: blue'
export NODE_EXTRA_CA_CERTS=~/.config/ox/corp-cachain.pem
```

- **`ANTHROPIC_AUTH_TOKEN`** authenticates with `Authorization: Bearer` and otherwise keeps the API-key request shape, without the OAuth beta.
- **`ANTHROPIC_CUSTOM_HEADERS`** takes one `Name: value` per line and skips blank lines. The headers go on every API request after the built-in ones, so they can replace defaults such as `User-Agent`. oxide-code owns `anthropic-beta`, `anthropic-version`, `content-type`, `x-claude-code-session-id`, and the header that carries the active credential (`x-api-key` for an API key, `Authorization` otherwise), so naming any of them is an error. The other credential header stays free, so a gateway can take its own `Authorization` alongside an API key. A line without a colon, an invalid name or value, a reserved name, or a repeated name stops startup with an error that gives the line number and never prints the value. `/config` lists the header names only.
- **`NODE_EXTRA_CA_CERTS`** works like [`extra_ca_certs`](#extra_ca_certs-corporate-trust-anchors) and ranks last, after `OX_EXTRA_CA_CERTS` and `client.extra_ca_certs`. Shells often export it for npm, so like Node, oxide-code logs a warning and skips the bundle when it cannot read it.
- **`CLAUDE_CODE_ATTRIBUTION_HEADER=0`** drops the `x-anthropic-billing-header` attribution block from the system prompt. By default, every request carries it, whatever the credential, since some gateways check it to recognize Claude Code requests. As in Claude Code, the block on a custom base URL omits the per-request `cch` hash, so it stays the same from request to request.
- **`CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS=1`** drops `output_config.format` from session-title requests, for gateways whose upstream rejects it. Titles then rely on the prompt alone to return JSON.

oxide-code is stricter than Claude Code in three ways. It never sends a stored login to a custom base URL, it sends one credential even when several are set, and it rejects malformed `ANTHROPIC_CUSTOM_HEADERS` lines where Claude Code skips them.

A gateway that does not serve the configured model may return HTTP 404 with a plain-text `404 page not found` response, which `ox` reports as `Anthropic API found no such endpoint or model`. The gateway may list the models it serves at `GET /v1/models`.

## Environment Variables

Environment variables override all config file values.

| Variable                                 | Config key                                 | Default                     | Description                  |
| ---------------------------------------- | ------------------------------------------ | --------------------------- | ---------------------------- |
| `ANTHROPIC_AUTH_TOKEN`                   | -                                          | -                           | Gateway bearer token         |
| `ANTHROPIC_API_KEY`                      | `client.api_key`                           | -                           | Anthropic API key            |
| `CLAUDE_CODE_OAUTH_TOKEN`                | -                                          | -                           | Long-lived OAuth token       |
| `CLAUDE_CONFIG_DIR`                      | -                                          | `~/.claude`                 | Claude Code's config dir     |
| `ANTHROPIC_BASE_URL`                     | `client.base_url`                          | `https://api.anthropic.com` | API base URL                 |
| `ANTHROPIC_MODEL`                        | `client.model`                             | `claude-opus-5-5`           | Model to use                 |
| `OX_EFFORT`                              | `client.effort`                            | per-model                   | Intelligence-vs-latency tier |
| `OX_MAX_TOKENS`                          | `client.max_tokens`                        | per-model                   | Max tokens per response      |
| `OX_EXTRA_CA_CERTS`                      | `client.extra_ca_certs`                    | -                           | Path to a PEM trust bundle   |
| `NODE_EXTRA_CA_CERTS`                    | -                                          | -                           | Fallback PEM trust bundle    |
| `ANTHROPIC_CUSTOM_HEADERS`               | -                                          | -                           | Extra request headers        |
| `CLAUDE_CODE_ATTRIBUTION_HEADER`         | -                                          | `true`                      | Send the attribution block   |
| `CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS` | -                                          | `false`                     | Drop title JSON schemas      |
| `OX_MAX_TOOL_ROUNDS`                     | `client.max_tool_rounds`                   | unset (unbounded)           | Per-turn tool-round cap      |
| `OX_PROMPT_CACHE_TTL`                    | `client.prompt_cache_ttl`                  | `1h`                        | Prompt-cache TTL             |
| `OX_COMPACTION_AUTO_ENABLED`             | `client.compaction.auto_enabled`           | `true`                      | Enable auto-compaction       |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`    | `client.compaction.auto_threshold_tokens`  | model-derived               | Absolute compaction trigger  |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`   | `client.compaction.auto_threshold_percent` | model-derived               | Percent compaction trigger   |
| `OX_PERMISSION_MODE`                     | `permission.mode`                          | `manual`                    | Tool permission mode         |
| `OX_SHOW_THINKING`                       | `tui.show_thinking`                        | `false`                     | Show extended thinking       |
| `OX_SHOW_WELCOME`                        | `tui.show_welcome`                         | `true`                      | Paint the welcome splash     |
| `OX_STATUS_LINE`                         | `tui.status_line`                          | see `[tui]`                 | Comma-separated segments     |

Every variable treats an empty value as unset. Boolean variables accept `1`, `true`, `yes`, or `on` to enable and `0`, `false`, `no`, or `off` to disable, in any letter case. Numeric variables take a whole number: at least `1` for `OX_MAX_TOKENS` and `OX_MAX_TOOL_ROUNDS`, at least `0` for `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`, and `1` to `100` for `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`. Any other value makes `ox` exit at startup with an error naming the variable and the values it accepts. The matching config keys accept the same ranges, and an out-of-range value there names its file and line.

## Logs

`ox` logs warnings and errors by default. Set `RUST_LOG=info` (or `debug`) for more detail. Where the log goes depends on the frontend:

- **TUI**: `$XDG_STATE_HOME/ox/log/oxide-code.log` (default `~/.local/state/ox/log/oxide-code.log`), which keeps log lines from painting over the full-screen frame. The file is readable only by your user. Once it passes 10 MiB, it moves to `oxide-code.log.1`, replacing the previous backup, so the log directory holds at most two files.
- **Bare REPL, headless runs, and `--list`**: stderr.
