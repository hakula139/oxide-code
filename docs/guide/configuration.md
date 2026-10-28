# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults.**
2. **User config file**: `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **User drop-ins**: `*.toml` files in `~/.config/ox/config.d/` (or `$XDG_CONFIG_HOME/ox/config.d/`), in file-name order.
4. **Project config file**: `.ox/config.toml` in the nearest directory from the working directory up to the project root that has one. The project root is the nearest directory at or above the working directory that holds a `.git` directory or file, or the working directory outside a repository.
5. **[Environment variables](#environment-variables)**: override every file. A command-line flag such as `--mode` overrides them in turn.

Run `/config` to see each resolved value next to the layer that set it: `default`, `user`, `config.d/<name>`, `project`, an environment variable such as `$OX_MODEL`, `--mode` for the permission mode set by that flag, or `session` after a `/model`, `/effort`, `/mode`, or `/theme` change or a **Tab** or **Shift+Tab** mode switch. [Permissions](permissions.md) covers the extra roots, rules, and turned-off default groups it also lists, each with the layer that added it.

## Layers

### Merging

Each file sets only the keys it names, and a later layer wins key by key, so a drop-in that sets `client.effort` keeps the `client.model` from `config.toml`. Tables merge recursively, including `[tui.theme.overrides]`, with these replacement and list rules:

- **`client.auth` and `client.headers`** come whole from the last layer that sets them. An empty header map clears the lower map.
- **`permission.extra_roots` and the class lists under `permission.deny`, `permission.ask`, and `permission.allow`**, such as `permission.allow.exec`, concatenate, so the entries from every layer apply.
- **Other arrays**, such as `tui.status_line`, come whole from the last layer that sets them.
- **`auto_threshold_tokens` and `auto_threshold_percent`** act as one setting, so a layer that sets either replaces both.

### Drop-Ins

`config.d/` lets a dotfile manager or a team script own one file without editing your `config.toml`. When the [Home Manager module](install.md#home-manager) writes `config.toml` itself, drop-ins hold your local overrides. Drop-ins load after `config.toml`, sorted by file name byte by byte, so a numeric prefix such as `10-team.toml` or `50-local.toml` sets their order. Hidden files, subdirectories, and files without a `.toml` extension are skipped, and a symlink loads its target.

Drop-ins share the `config.toml` schema and count as user config. An invalid drop-in, including a symlink whose target is missing, stops startup with an error that names the file. A malformed permission pattern or extra root names the file that holds it in the same way.

### Project Config

A project file comes from the checkout, so it can set only the `[client]`, `[permission]`, and `[tui]` tables. Within `[client]` it can set only `model`, `effort`, `max_tokens`, `max_tool_rounds`, `prompt_cache_ttl`, and the [`[client.compaction]`](#clientcompaction-context-compression) table, and within `[permission]` only `deny` and `ask`. Setting any other key fails the config load. This keeps a repository from redirecting your credentials, changing the endpoint or TLS trust, or widening what runs without asking. A project file may set `tui.theme.base` only to a built-in theme name, and the TUI refuses to start when it names a file. A TOML error in a project file gives only its line, column, and key, since the parser's message can quote a value.

The project file must also resolve inside the project root. A `.ox/config.toml` that resolves outside it, through a symlinked file or `.ox` directory, fails the config load with an error that names only the link. An unreadable candidate, an entry that is not a regular file, or a link whose target cannot resolve also fails the load. A link to a file inside the repository, such as a config shared across a monorepo, still works, unless the project root is your home directory or one of its ancestors, as with a dotfiles repository at `~`. There, a project file reached through any symlink fails the load.

## Checking the Config

`ox config check` loads the config files that apply in the working directory along with the environment variables, validates them as startup does, and prints each file it read. Warnings, such as an unsupported model or an ignored theme override, go to stderr. An invalid value exits with status 1 and the startup error.

The check skips credential files, stored logins, and CA bundles, so it never opens the keychain or connects, which lets it run in CI or a Nix build. It covers only the config, so instruction files such as `AGENTS.md` go unchecked. Other paths, such as a theme file or an extra root, resolve against the filesystem where the check runs.

## Config File

All fields are optional, so set only the values you want to override. [Config Keys](#config-keys) lists every key with its type and default.

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

#### `model`: Supported Models

oxide-code supports `claude-fable-5-1`, `claude-opus-5-5`, `claude-sonnet-5-5`, and `claude-haiku-5-5`. Each has a 1M-token context window and adaptive thinking. Fable 5.1 is not available to every account or gateway. Any other ID, such as a gateway alias, is sent verbatim without `effort`, `thinking`, or context management. Its `max_tokens` defaults to `16000`, no cost estimate is provided, and automatic compaction stays off unless you set `auto_threshold_tokens`. oxide-code warns about unsupported IDs at startup. For a `claude-*` ID that names a model family or an ID with a `[1m]` suffix, it suggests the matching supported model.

#### `effort`: Reasoning Effort

`effort` maps 1:1 to the `output_config.effort` body field. Accepted values: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Every supported model accepts all five.

Per-model defaults when `effort` is unset, matching the API's own defaults:

| Model      | Default  |
| ---------- | -------- |
| Fable 5.1  | `high`   |
| Opus 5.5   | `medium` |
| Sonnet 5.5 | `high`   |
| Haiku 5.5  | `medium` |

A `/model` swap carries the current effort over to the new model.

This guide follows Anthropic's [effort documentation](https://platform.claude.com/docs/en/build-with-claude/effort) for Haiku 5.5, and for Sonnet 5.5 on agentic work:

- `max` / `xhigh`: Only where you have measured a quality gain, since turns run much longer and cost more.
- `high`: Knowledge work, harder or longer agent tasks, and strict instruction following.
- `medium`: The starting point for most work, including well-specified agentic coding and multistep tool use.
- `low`: Chat and short, latency-sensitive tasks. At this level, long agent tasks are more likely to stop early or skip verifying a change.

For Opus 5.5, the documentation names no starting level and recommends an effort sweep on your own evaluations. Fable 5.1's recommended start is its `high` default.

#### `max_tokens`: Response Ceiling

When unset, `max_tokens` is `64000` on the supported models, half their 128K output ceiling. `effort` controls how much the model thinks, so this cap only bounds runaway output. Other IDs get `16000`, which fits the lower output ceilings of older or smaller models, and the default follows a `/model` swap. Set `max_tokens` (via TOML or `OX_MAX_TOKENS`) to change it.

#### `max_tool_rounds`: Agent-Loop Safety Cap

When unset, the agent loop has no per-turn round cap. Setting `max_tool_rounds = N` (or `OX_MAX_TOOL_ROUNDS=N`) ends the turn after `N` rounds with a runaway-loop error. The cap is a guard against tools stuck in a retry loop, since normal agent sessions routinely run hundreds of rounds.

#### `base_url`: Endpoint

Set `base_url` in user config or `ANTHROPIC_BASE_URL`. The URL must use HTTPS unless it points at localhost for a local proxy, and it must omit query strings, fragments, and a username or password. Requests go to `/v1/messages` under it, so leave `/v1` off the URL. A path prefix is supported, and a trailing slash makes no difference. To authenticate against a gateway that takes its own token, set `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, or `[client.auth]`, which [LLM gateways](authentication.md#llm-gateways) covers.

#### `extra_ca_certs`: Corporate Trust Anchors

oxide-code verifies TLS certificates using the operating system's trust store, so a corporate CA installed system-wide works without extra configuration. For gateways and other private-CA endpoints whose root CA isn't in that store, set `extra_ca_certs` to a PEM bundle to add those roots to oxide-code's trust store:

```toml
[client]
base_url = "https://gw.llm.corp.example/anthropic"
extra_ca_certs = "~/.config/ox/corp-cachain.pem"
```

The path must be absolute or start with `~/`, and `ox` rejects a relative one, which would depend on the directory it starts in. `OX_EXTRA_CA_CERTS` wins over this field, and `NODE_EXTRA_CA_CERTS` applies only when neither is set. Both variables may hold a relative path, which resolves from the working directory as in Node (see [LLM gateways](authentication.md#llm-gateways)).

On macOS, a server certificate valid for longer than Apple's 825-day limit is accepted when it chains to a CA in the bundle, even one already installed system-wide, unless that CA is one of Apple's built-in roots.

#### `prompt_cache_ttl`: Cache Duration

Accepted values: `"5m"` (matches the server default) and `"1h"` (higher write premium, bigger hit-rate win on long sessions). oxide-code defaults to `"1h"`, which costs more per cache write and pays off in sessions with pauses over 5 minutes.

### `[client.compaction]`: Context Compression

Auto-compaction is enabled by default for the supported models, and `auto_enabled = false` turns it off. The default trigger leaves room for the next response and a safety buffer. Set `auto_threshold_tokens` or `auto_threshold_percent` when you want compaction to happen earlier.

`auto_threshold_tokens` and `auto_threshold_percent` are mutually exclusive. Percent must be between `1` and `100`. Either form resolves to a token count clamped into the usable range: values below `50000` tokens snap up to that floor, and values above the model-derived safe trigger snap down. The model-derived ceiling shifts with `/model`, so a global threshold sized for a larger context window keeps working after a swap.

For other model IDs, the default and percent-based automatic triggers stay off. An explicit token threshold still works, snapped to the `50000` floor when set lower.

Automatic compaction runs before a prompt and between the tool rounds of a long turn, which then continues from the summary on its own. If the compaction before a prompt fails, oxide-code does not send the prompt, and the TUI puts it back in the input. Sending it again retries the compaction. If the compaction between tool rounds fails, the turn stops, so run `/compact`, then send a message to continue. After 3 failures in a row, automatic compaction pauses, so the next prompt goes out without it, and `/status` shows it as paused. A successful `/compact`, a `/model` or `/effort` change, `/clear`, `/archive`, or `/resume` turns it back on.

### `[tui]`: Terminal UI

`show_thinking = true` opts the request into `thinking.display = "summarized"`, so the API streams reasoning text. The chat shows it as it arrives, then collapses it to a `Thought` header that **Ctrl+O** or a click expands.

With the default `false`, reasoning stays hidden. The chat still shows the progress notes the model writes before each tool call on every supported model except Haiku 5.5. Haiku 5.5 writes none, so its requests ask for `thinking.display = "omitted"` and the chat shows no reasoning.

`show_welcome = false` blanks the chat region until you submit a prompt, which is useful when piping or screen-recording.

`status_line` lists segments from the table below in display order and must contain at least one. Add, remove, or reorder any segment, including `permission-mode`, to control its presence and position. When unset, it is the list in the [example](#config-file): `current-dir`, `git-branch`, `pull-request`, `model-with-effort`, `context-used`, `session-cost`, `permission-mode`, `run-state`, and `thread-title`.

Segments with no data, such as a missing git branch, an unopened pull request, context usage before the first completed turn, or usage limits under an API key, are omitted. The three pull request segments shell out to `gh pr view`, so they require the GitHub CLI on `PATH`. Click one of them to open the PR, its checks, or its changed files, and terminals that open links on **Ctrl+click** or **Cmd+click** do the same. `pull-request-review` shows only on repositories whose branch protection requires a review, since GitHub reports no review decision otherwise. The separator takes its style from the active theme's `separator` slot and has no setting of its own.

| Segment               | Renders                                                       | Refresh         |
| --------------------- | ------------------------------------------------------------- | --------------- |
| `current-dir`         | Tildified working directory                                   | At startup      |
| `git-branch`          | Current branch (omitted on detached HEAD)                     | Every 5 s       |
| `pull-request`        | Open PR as clickable `#86` when OSC 8 works                   | Every 60 s      |
| `pull-request-checks` | `Checks: 2 failing`, `1 pending`, `1 cancelled`, or `passing` | Every 60 s      |
| `pull-request-review` | `Review: approved`, `changes requested`, or `required`        | Every 60 s      |
| `model`               | Compact model label, such as `Opus 5.5`                       | On `/model`     |
| `model-with-effort`   | Model label plus the effort level in parens                   | On `/model`     |
| `context-used`        | `Ctx: 10% (100k/1M)` after the first turn                     | Per turn        |
| `session-cost`        | `Cost: $0.43` running USD estimate                            | Per turn        |
| `five-hour-limit`     | `5h: 42%` of a subscription's five-hour usage limit           | Per request     |
| `weekly-limit`        | `7d: 13%` of a subscription's weekly usage limit              | Per request     |
| `permission-mode`     | `manual`, `plan`, or `yolo`                                   | On switch       |
| `run-state`           | `Ready` / spinner + label / **Ctrl+C** exit hint              | On state change |
| `queued-prompts`      | `Queued: 2` while prompts wait for the running turn           | On change       |
| `task-progress`       | `Tasks: 2/5` completed while the session keeps a task list    | On change       |
| `thread-title`        | Session title, AI-generated or set by `/rename`               | On change       |
| `session-id`          | `ID: 3f2a9c1e`, the prefix `ox -r` resumes                    | On change       |
| `theme`               | `Theme: mocha`, or a theme file's name                        | On `/theme`     |
| `current-time`        | `HH:MM` in the local timezone                                 | Per minute      |

The usage limits come from Claude subscription sign-ins, which report how much of each window is spent with every response. A window disappears when it resets and remains gone until the next response reports it again. The [theming guide](theming.md#status-line-segments) lists the colors used by the context and usage-limit segments as a window fills.

On a narrow row, lower-priority segments drop first, and the highest-priority segment is truncated if needed. [Status Line](../design/status-line.md#narrow-rows) lists the utility order.

`OX_STATUS_LINE` accepts the same segment names, comma-separated, with optional whitespace (`OX_STATUS_LINE="model, run-state"`). A value with no segments (`","`), an empty entry between commas (`"model,,run-state"`), or an unknown segment name fails startup with a parse error.

### `[tui.theme]`: Terminal Theme

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

## Logs

`ox` logs warnings and errors by default. Set `RUST_LOG=info` (or `debug`) for more detail. `ox` skips a directive it cannot parse and names it on stderr at startup. Where the log goes depends on the frontend:

- **TUI**: `$XDG_STATE_HOME/ox/log/oxide-code.log` (default `~/.local/state/ox/log/oxide-code.log`), which keeps log lines from painting over the full-screen frame. Before an event would push a nonempty file past 10 MiB, it moves to `oxide-code.log.1`, replacing the previous backup. An oversized event stays whole in the active file. Concurrent sessions share these two log files and coordinate writes and rotation through `oxide-code.log.lock`. On Unix, the directory and all three files are private to your user.
- **Bare REPL, headless runs, and `--list`**: stderr.

## Reference

### Config Keys

Each key goes in the table its prefix names, so `client.compaction.auto_enabled` is `auto_enabled` under `[client.compaction]`. A project file sets only the keys [Project Config](#project-config) lists.

| Key                                        | Type    | Default                         | Description                                                                                                 |
| ------------------------------------------ | ------- | ------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `client.attribution`                       | boolean | `true`                          | Send the [attribution block](authentication.md#llm-gateways), user config only                              |
| `client.auth`                              | table   | -                               | [Typed credential](authentication.md), user config only                                                     |
| `client.base_url`                          | string  | `"https://api.anthropic.com"`   | [API base URL](#base_url-endpoint), user config only                                                        |
| `client.compaction.auto_enabled`           | boolean | `true`                          | Enable [automatic context compaction](#clientcompaction-context-compression)                                |
| `client.compaction.auto_threshold_percent` | integer | model-derived                   | Share of the context window that [triggers compaction](#clientcompaction-context-compression), `1` to `100` |
| `client.compaction.auto_threshold_tokens`  | integer | model-derived                   | Token count that [triggers compaction](#clientcompaction-context-compression)                               |
| `client.effort`                            | string  | per-model                       | [Effort level](#effort-reasoning-effort), `"low"` to `"max"`                                                |
| `client.extra_ca_certs`                    | string  | -                               | [PEM bundle](#extra_ca_certs-corporate-trust-anchors) appended to the trust store, user config only         |
| `client.headers`                           | table   | empty                           | [Extra request headers](authentication.md#llm-gateways), user config only                                   |
| `client.max_tokens`                        | integer | per-model                       | [Max tokens per response](#max_tokens-response-ceiling)                                                     |
| `client.max_tool_rounds`                   | integer | unset (unbounded)               | [Per-turn safety cap](#max_tool_rounds-agent-loop-safety-cap) on tool rounds                                |
| `client.model`                             | string  | `"claude-opus-5-5"`             | [Model](#model-supported-models) to use                                                                     |
| `client.prompt_cache_ttl`                  | string  | `"1h"`                          | [Prompt-cache TTL](#prompt_cache_ttl-cache-duration), `"5m"` or `"1h"`                                      |
| `client.structured_outputs`                | boolean | `true`                          | Allow [title JSON schemas](authentication.md#llm-gateways), user config only                                |
| `permission.allow`                         | table   | empty                           | [Calls that run without asking](permissions.md#rule-tables), user config only                               |
| `permission.ask`                           | table   | empty                           | [Calls that ask first](permissions.md#rule-tables), even when an allow pattern matches                      |
| `permission.defaults`                      | table   | all on                          | [Built-in rules](permissions.md#built-in-rules) to turn off by name, user config only                       |
| `permission.deny`                          | table   | empty                           | [Calls that never run](permissions.md#rule-tables), in every mode                                           |
| `permission.extra_roots`                   | array   | `[]`                            | [More directories](permissions.md#workspace-roots) whose edits run without asking, user config only         |
| `permission.mode`                          | string  | `"manual"`                      | [Permission mode](permissions.md#modes), `"manual"`, `"plan"`, or `"yolo"`, user config only                |
| `tui.show_thinking`                        | boolean | `false`                         | [Show extended thinking](#tui-terminal-ui)                                                                  |
| `tui.show_welcome`                         | boolean | `true`                          | [Paint the welcome splash](#tui-terminal-ui) on an empty chat                                               |
| `tui.status_line`                          | array   | see [`[tui]`](#tui-terminal-ui) | Ordered status-line segments                                                                                |
| `tui.theme.base`                           | string  | `"mocha"`                       | [Built-in theme](theming.md#built-in-themes) name, or a TOML theme file path in user config                 |
| `tui.theme.overrides`                      | table   | empty                           | [Per-slot patches](theming.md#overrides) applied on top of the base                                         |

### Environment Variables

A variable that maps to a config key overrides that key in every file.

| Variable                                 | Config key                                 | Description                                                                                              |
| ---------------------------------------- | ------------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| `ANTHROPIC_API_KEY`                      | `client.auth`                              | [Anthropic API key](authentication.md)                                                                   |
| `ANTHROPIC_AUTH_TOKEN`                   | `client.auth`                              | [Gateway bearer token](authentication.md#llm-gateways)                                                   |
| `ANTHROPIC_BASE_URL`                     | `client.base_url`                          | API base URL                                                                                             |
| `ANTHROPIC_CUSTOM_HEADERS`               | `client.headers`                           | [Extra request headers](authentication.md#llm-gateways), one `Name: value` per line                      |
| `CLAUDE_CODE_ATTRIBUTION_HEADER`         | `client.attribution`                       | Send the [attribution block](authentication.md#llm-gateways), default `true`                             |
| `CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS` | `client.structured_outputs`                | Disable [title JSON schemas](authentication.md#llm-gateways), default `false`, inverse of the config key |
| `CLAUDE_CODE_OAUTH_TOKEN`                | -                                          | [Long-lived OAuth token](authentication.md)                                                              |
| `CLAUDE_CONFIG_DIR`                      | -                                          | [Claude Code's config dir](authentication.md), default `~/.claude`                                       |
| `NODE_EXTRA_CA_CERTS`                    | -                                          | [Fallback PEM trust bundle](authentication.md#llm-gateways)                                              |
| `NO_COLOR`                               | -                                          | Any non-empty value turns off color in the TUI and in [logs](#logs) written to stderr                    |
| `OX_COMPACTION_AUTO_ENABLED`             | `client.compaction.auto_enabled`           | Enable auto-compaction                                                                                   |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`   | `client.compaction.auto_threshold_percent` | Percent compaction trigger                                                                               |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`    | `client.compaction.auto_threshold_tokens`  | Absolute compaction trigger                                                                              |
| `OX_EFFORT`                              | `client.effort`                            | Effort level                                                                                             |
| `OX_EXTRA_CA_CERTS`                      | `client.extra_ca_certs`                    | Path to a PEM trust bundle                                                                               |
| `OX_MAX_TOKENS`                          | `client.max_tokens`                        | Max tokens per response                                                                                  |
| `OX_MAX_TOOL_ROUNDS`                     | `client.max_tool_rounds`                   | Per-turn tool-round cap                                                                                  |
| `OX_MODEL`                               | `client.model`                             | Model to use                                                                                             |
| `OX_PERMISSION_MODE`                     | `permission.mode`                          | Tool permission mode                                                                                     |
| `OX_PROMPT_CACHE_TTL`                    | `client.prompt_cache_ttl`                  | Prompt-cache TTL                                                                                         |
| `OX_SESSION_ID`                          | -                                          | Set by `ox` for each `bash` command to the [session's ID](permissions.md#commands-the-assistant-runs)    |
| `OX_SHOW_THINKING`                       | `tui.show_thinking`                        | Show extended thinking                                                                                   |
| `OX_SHOW_WELCOME`                        | `tui.show_welcome`                         | Paint the welcome splash                                                                                 |
| `OX_STATUS_LINE`                         | `tui.status_line`                          | Comma-separated status-line segments                                                                     |
| `RUST_LOG`                               | -                                          | [Log filter](#logs), default `warn`                                                                      |
| `XDG_CONFIG_HOME`                        | -                                          | Base of the user config directory, default `~/.config`                                                   |
| `XDG_DATA_HOME`                          | -                                          | Base of the sessions, the device ID, and the oxide-code login, default `~/.local/share`                  |
| `XDG_STATE_HOME`                         | -                                          | Base of the log directory, default `~/.local/state`                                                      |

`ox` treats an empty value of each variable in the table as unset. Boolean variables accept `1`, `true`, `yes`, or `on` to enable and `0`, `false`, `no`, or `off` to disable, in any letter case. Numeric variables take a whole number: at least `1` for `OX_MAX_TOKENS` and `OX_MAX_TOOL_ROUNDS`, at least `0` for `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`, and `1` to `100` for `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`. Any other value makes `ox` exit at startup with an error naming the variable and the values it accepts. The matching config keys accept the same ranges, and an out-of-range value there names its file and line.

`ox` also follows these variables, which its libraries read:

- **`HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY`** send requests through a proxy, and `NO_PROXY` lists comma-separated hosts that skip it. The lower-case names work too, and `ALL_PROXY` applies where the variable for the request's scheme is unset.
- **`SSL_CERT_FILE` and `SSL_CERT_DIR`** replace the system trust store on Linux with a PEM bundle or directories of certificates.
