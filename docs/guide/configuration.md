# Configuration

oxide-code merges configuration in order of increasing priority:

1. **Built-in defaults.**
2. **User config file**: `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **User drop-ins**: `*.toml` files in `~/.config/ox/config.d/` (or `$XDG_CONFIG_HOME/ox/config.d/`), in file-name order.
4. **Project config file**: The nearest `.ox/config.toml` from the working directory up to the project root (the nearest directory at or above the working directory containing a `.git` directory or file, or the working directory outside a repository).
5. **[Environment variables](#environment-variables)**: Override every file. CLI flags such as `--mode` override them in turn.

Run `/config` to see each resolved value and its layer: `default`, `user`, `config.d/<name>`, `project`, an environment variable such as `$OX_MODEL`, `--mode`, or `session` (changed during the session).

## Layers

### Merging

Later layers override earlier ones key by key and merge tables recursively, including `[tui.theme.overrides]`, with these exceptions:

- **`client.auth` and `client.headers`** replace lower layers whole. An empty header map clears lower headers.
- **`permission.extra_roots` and the class lists under `permission.deny`, `permission.ask`, and `permission.allow`** concatenate across layers.
- **Other arrays**, such as `tui.status_line`, replace lower layers whole.
- **`auto_threshold_tokens` and `auto_threshold_percent`** act as one setting, so setting either replaces both.

### Drop-Ins

`config.d/` holds drop-ins that share the `config.toml` schema and count as user config. Dotfile managers and team scripts can write separate files there without editing `config.toml`, and you can put local overrides there when the [Home Manager module](install.md#home-manager) manages `config.toml`.

- **Order**: Load after `config.toml` in byte-sorted file-name order, so numeric prefixes such as `10-team.toml` control precedence.
- **Skipped entries**: Hidden files, subdirectories, and non-`.toml` files are skipped. Symlinks load their target.
- **Errors**: Any invalid drop-in (including a broken symlink, malformed permission pattern, or invalid extra root) stops startup and names the file.

### Project Config

Because a project file comes from the checkout, it cannot redirect credentials, change the endpoint or TLS trust, or widen approvals. It may set only these keys (any other fails the config load):

- **`[client]`**: `model`, `effort`, `max_tokens`, `max_tool_rounds`, `prompt_cache_ttl`, and [`[client.compaction]`](#clientcompaction-context-compression).
- **`[permission]`**: `deny` and `ask`.
- **`[tui]`**: Every key, except `tui.theme.base` must name a built-in theme (a file path stops TUI startup).

TOML errors in a project file report only line, column, and key so values are never quoted. The file must resolve inside the project root. Any symlinked file or `.ox` directory pointing outside the root fails the load (naming only the link), as does an unreadable candidate, non-regular file, or broken symlink. Symlinks inside the repository work unless the project root is your home directory or one of its ancestors (such as a dotfiles repository at `~`), where any project file reached through a symlink fails.

## Checking the Config

`ox config check` validates the config files and environment variables for the working directory as startup does and lists each loaded file. Warnings go to stderr, and any error exits with status 1. It skips credential files, stored logins, CA bundles, and instruction files, so it never touches the keychain or network and can run in CI or a Nix build. Other paths, such as theme files and extra roots, resolve against the current filesystem.

## Config File

All fields are optional. [Config Keys](#config-keys) lists every key, type, and default.

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

oxide-code supports `claude-fable-5-1`, `claude-opus-5-5`, `claude-sonnet-5-5`, and `claude-haiku-5-5`, each with a 1M-token context window and adaptive thinking. Fable 5.1 is not available on every account or gateway.

Unrecognized IDs (such as gateway aliases) are sent verbatim without `effort`, `thinking`, or context management: `max_tokens` defaults to `16000`, cost estimates are omitted, and auto-compaction stays off unless `auto_threshold_tokens` is set. Startup warns on unknown IDs and suggests a supported model when a `claude-*` ID names a model family or carries a `[1m]` suffix.

#### `effort`: Reasoning Effort

`effort` sets `output_config.effort` to `"low"`, `"medium"`, `"high"`, `"xhigh"`, or `"max"`, accepted by every supported model. When unset, each model uses its API default:

| Model      | Default  |
| ---------- | -------- |
| Fable 5.1  | `high`   |
| Opus 5.5   | `medium` |
| Sonnet 5.5 | `high`   |
| Haiku 5.5  | `medium` |

A `/model` swap carries the current effort over. Anthropic's [effort documentation](https://platform.claude.com/docs/en/build-with-claude/effort) recommends these levels for Haiku 5.5 and for agentic Sonnet 5.5 work:

- `max` / `xhigh`: Only after measuring a quality gain, since turns run longer and cost more.
- `high`: Knowledge work, harder or longer agent tasks, and strict instruction following.
- `medium`: Starting point for well-specified agentic coding and multistep tool use.
- `low`: Chat and short, latency-sensitive tasks (long agent tasks are more likely to stop early or skip verification).

Opus 5.5 has no fixed starting level (run an effort sweep on your own evaluations). Fable 5.1's recommended start is its `high` default.

#### `max_tokens`: Response Ceiling

When unset, `max_tokens` defaults to `64000` on supported models (half their 128K output ceiling, since `effort` controls thinking and this cap only bounds runaway output) and `16000` on other IDs. The default follows `/model` swaps.

#### `max_tool_rounds`: Agent-Loop Safety Cap

Unset by default (unbounded), since normal agent sessions routinely run hundreds of rounds. `max_tool_rounds = N` ends a turn after `N` rounds with a runaway-loop error to guard against stuck retry loops.

#### `base_url`: Endpoint

Set `base_url` in user config or `ANTHROPIC_BASE_URL`. The URL must use HTTPS (except localhost for a local proxy) and omit query strings, fragments, and credentials. Requests go to `/v1/messages` under it, so leave `/v1` off. Path prefixes are supported, and trailing slashes make no difference. See [LLM Gateways](authentication.md#llm-gateways) for gateway authentication.

#### `extra_ca_certs`: Corporate Trust Anchors

oxide-code verifies TLS against the OS trust store, so system-installed corporate CAs work automatically. To add a private root CA, point `extra_ca_certs` at a PEM bundle:

```toml
[client]
base_url = "https://gw.llm.corp.example/anthropic"
extra_ca_certs = "~/.config/ox/corp-cachain.pem"
```

The path must be absolute or start with `~/`. Precedence is `OX_EXTRA_CA_CERTS`, then `extra_ca_certs`, then [`NODE_EXTRA_CA_CERTS`](authentication.md#llm-gateways). Both variables accept relative paths resolved from the working directory, matching Node. On macOS, server certificates valid longer than Apple's 825-day limit are accepted when they chain to a CA in this bundle (even one installed system-wide), except Apple's built-in roots.

#### `prompt_cache_ttl`: Cache Duration

Default `"1h"` costs more per cache write than the server's `"5m"` default and pays off in sessions with pauses over 5 minutes.

### `[client.compaction]`: Context Compression

Auto-compaction is on by default for supported models (`auto_enabled = false` disables it). By default it triggers with room for the next response plus a safety buffer, and `auto_threshold_tokens` or `auto_threshold_percent` (`1` to `100`) compacts earlier.

The two thresholds are mutually exclusive. Both resolve to a token count clamped between `50000` and the model's safe trigger (which adjusts across `/model` swaps). For unrecognized model IDs, default and percent triggers stay off, while an explicit `auto_threshold_tokens` still applies (with a floor of `50000`). See [Compaction](slash-commands.md#compaction) for trigger timing and failure handling.

### `[interaction]`: Question and Approval Deadlines

Questions and approvals wait indefinitely by default. Set a positive integer `timeout_seconds` in user config or a drop-in to cap their total wait:

```toml
[interaction.questions]
timeout_seconds = 120
on_timeout = "recommended"

[interaction.approvals]
timeout_seconds = 60
on_timeout = "deny"
```

- **Questions**: Default `on_timeout = "recommended"` picks the recommended option or skips if none exists. `on_timeout = "skip"` always skips. Timeout answers are marked automatic in the tool result and do not count as user instructions.
- **Approvals**: Default `on_timeout = "deny"` denies the call. `on_timeout = "allow"` approves only that single call without granting later calls or overriding deny rules.

The timer starts when the prompt appears and does not reset while typing, and TUI prompts show the configured duration and action. Headless runs ignore these deadlines because they abort questions and deny unapproved calls immediately ([Headless Mode](headless.md)). Remove `timeout_seconds` to disable a deadline.

### `[tui]`: Terminal UI

- **`show_thinking = true`** sets `thinking.display = "summarized"` so the API streams reasoning live, then collapses it to a `Thought` header (**Ctrl+O** or click expands it). With the default `false`, reasoning stays hidden while pre-tool progress notes still display (`thinking.display = "updates"`). Haiku 5.5 writes no progress notes and uses `thinking.display = "omitted"`.
- **`show_welcome = false`** blanks the chat until the first prompt (useful when piping or screen-recording).
- **`status_line`** lists at least one segment from the table below in display order (default shown in the [example](#config-file)).

Segments with no data (such as `git-branch` on detached HEAD or usage limits under an API key) are omitted. The three pull request segments run `gh pr view` (requiring `gh` on `PATH`) and open the PR, checks, or changed files on click, **Ctrl+click**, or **Cmd+click**. `pull-request-review` appears only when branch protection requires a review.

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
| `permission-mode`     | `auto`, `manual`, `plan`, or `yolo`                           | On switch       |
| `run-state`           | `Ready` / spinner + label / **Ctrl+C** exit hint              | On state change |
| `queued-prompts`      | `Queued: 2` while prompts wait for the running turn           | On change       |
| `task-progress`       | `Tasks: 2/5` completed while the session keeps a task list    | On change       |
| `thread-title`        | Session title, AI-generated or set by `/rename`               | On change       |
| `session-id`          | `ID: 3f2a9c1e`, the prefix `ox -r` resumes                    | On change       |
| `theme`               | `Theme: mocha`, or a theme file's name                        | On `/theme`     |
| `current-time`        | `HH:MM` in the local timezone                                 | Per minute      |

Claude subscription sign-ins report usage-limit windows on each response and drop a window after reset until the next response reports it. [Theming](theming.md#status-line-segments) lists their color thresholds. Narrow rows drop lower-priority segments first and truncate the highest-priority one if needed (see [Status Line](../design/status-line.md#narrow-rows)).

`OX_STATUS_LINE` accepts the same segment names, comma-separated with optional whitespace (such as `OX_STATUS_LINE="model, run-state"`). Values with no segments (such as whitespace or lone commas), empty entries between commas, or unknown names fail startup.

### `[tui.theme]`: Terminal Theme

`tui.theme.base` selects a built-in theme or a theme file, and `[tui.theme.overrides]` patches color slots. See [Theming](theming.md).

### `[permission]`: Tool Approval

`[permission]` configures the mode, deny / ask / allow rules, extra workspace roots, and classifier model and instructions. See [Permissions](permissions.md).

## Logs

`ox` logs warnings and errors by default (`RUST_LOG=info` or `debug` adds detail). Unparsable directives are skipped with a warning on stderr.

- **TUI**: `$XDG_STATE_HOME/ox/log/oxide-code.log` (default `~/.local/state/ox/log/oxide-code.log`), shared across concurrent sessions and private to your user on Unix. Rotates to `oxide-code.log.1` (replacing the prior backup) before exceeding 10 MiB.
- **Headless runs and `--list`**: stderr.

## Reference

### Config Keys

Keys live under the table their prefix names (for example, `client.compaction.auto_enabled` is `auto_enabled` under `[client.compaction]`). Project files allow only the keys in [Project Config](#project-config).

| Key                                        | Type    | Default                         | Description                                                                                                             |
| ------------------------------------------ | ------- | ------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `client.attribution`                       | boolean | `true`                          | Send the [attribution block](authentication.md#llm-gateways), user config only                                          |
| `client.auth`                              | table   | -                               | [Typed credential](authentication.md), user config only                                                                 |
| `client.base_url`                          | string  | `"https://api.anthropic.com"`   | [API base URL](#base_url-endpoint), user config only                                                                    |
| `client.compaction.auto_enabled`           | boolean | `true`                          | Enable [automatic context compaction](#clientcompaction-context-compression)                                            |
| `client.compaction.auto_threshold_percent` | integer | model-derived                   | Share of the context window that [triggers compaction](#clientcompaction-context-compression), `1` to `100`             |
| `client.compaction.auto_threshold_tokens`  | integer | model-derived                   | Token count that [triggers compaction](#clientcompaction-context-compression)                                           |
| `client.effort`                            | string  | per-model                       | [Effort level](#effort-reasoning-effort), `"low"` to `"max"`                                                            |
| `client.extra_ca_certs`                    | string  | -                               | [PEM bundle](#extra_ca_certs-corporate-trust-anchors) appended to the trust store, user config only                     |
| `client.headers`                           | table   | empty                           | [Extra request headers](authentication.md#llm-gateways), user config only                                               |
| `client.max_tokens`                        | integer | per-model                       | [Max tokens per response](#max_tokens-response-ceiling)                                                                 |
| `client.max_tool_rounds`                   | integer | unset (unbounded)               | [Per-turn safety cap](#max_tool_rounds-agent-loop-safety-cap) on tool rounds                                            |
| `client.model`                             | string  | `"claude-opus-5-5"`             | [Model](#model-supported-models) to use                                                                                 |
| `client.prompt_cache_ttl`                  | string  | `"1h"`                          | [Prompt-cache TTL](#prompt_cache_ttl-cache-duration), `"5m"` or `"1h"`                                                  |
| `client.structured_outputs`                | boolean | `true`                          | Allow [JSON schemas](authentication.md#llm-gateways) for titles and permission classification, user config only         |
| `interaction.approvals.on_timeout`         | string  | `"deny"`                        | [Approval deadline action](#interaction-question-and-approval-deadlines), `"deny"` or `"allow"`, user config only       |
| `interaction.approvals.timeout_seconds`    | integer | unset (unbounded)               | Total approval wait in positive seconds, user config only                                                               |
| `interaction.questions.on_timeout`         | string  | `"recommended"`                 | [Question deadline action](#interaction-question-and-approval-deadlines), `"recommended"` or `"skip"`, user config only |
| `interaction.questions.timeout_seconds`    | integer | unset (unbounded)               | Total question wait in positive seconds, user config only                                                               |
| `permission.allow`                         | table   | empty                           | [Calls that run without asking](permissions.md#rule-tables), user config only                                           |
| `permission.ask`                           | table   | empty                           | [Calls that ask first](permissions.md#rule-tables), even when an allow pattern matches                                  |
| `permission.auto.instructions`             | array   | `[]`                            | Natural-language approval instructions, user config only                                                                |
| `permission.auto.model`                    | string  | `"claude-haiku-5-5"`            | Approval classifier model, user config only                                                                             |
| `permission.deny`                          | table   | empty                           | [Calls that never run](permissions.md#rule-tables), in every mode                                                       |
| `permission.extra_roots`                   | array   | `[]`                            | [Additional rule anchors and classifier context](permissions.md#workspace-roots), user config only                      |
| `permission.mode`                          | string  | `"auto"`                        | [Permission mode](permissions.md#modes), `"auto"`, `"manual"`, `"plan"`, or `"yolo"`, user config only                  |
| `tui.show_thinking`                        | boolean | `false`                         | [Show extended thinking](#tui-terminal-ui)                                                                              |
| `tui.show_welcome`                         | boolean | `true`                          | [Paint the welcome splash](#tui-terminal-ui) on an empty chat                                                           |
| `tui.status_line`                          | array   | see [`[tui]`](#tui-terminal-ui) | Ordered status-line segments                                                                                            |
| `tui.theme.base`                           | string  | `"mocha"`                       | [Built-in theme](theming.md#built-in-themes) name, or a TOML theme file path in user config                             |
| `tui.theme.overrides`                      | table   | empty                           | [Per-slot patches](theming.md#overrides) applied on top of the base                                                     |

### Environment Variables

A variable that maps to a config key overrides that key in every file.

| Variable                                 | Config key                                 | Description                                                                                                                                 |
| ---------------------------------------- | ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `ANTHROPIC_API_KEY`                      | `client.auth`                              | [Anthropic API key](authentication.md)                                                                                                      |
| `ANTHROPIC_AUTH_TOKEN`                   | `client.auth`                              | [Gateway bearer token](authentication.md#llm-gateways)                                                                                      |
| `ANTHROPIC_BASE_URL`                     | `client.base_url`                          | API base URL                                                                                                                                |
| `ANTHROPIC_CUSTOM_HEADERS`               | `client.headers`                           | [Extra request headers](authentication.md#llm-gateways), one `Name: value` per line                                                         |
| `CLAUDE_CODE_ATTRIBUTION_HEADER`         | `client.attribution`                       | Send the [attribution block](authentication.md#llm-gateways), default `true`                                                                |
| `CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS` | `client.structured_outputs`                | Disable [JSON schemas](authentication.md#llm-gateways) for titles and permission classification, default `false`, inverse of the config key |
| `CLAUDE_CODE_OAUTH_TOKEN`                | -                                          | [Long-lived OAuth token](authentication.md)                                                                                                 |
| `CLAUDE_CONFIG_DIR`                      | -                                          | [Claude Code's config dir](authentication.md), default `~/.claude`                                                                          |
| `NODE_EXTRA_CA_CERTS`                    | -                                          | [Fallback PEM trust bundle](authentication.md#llm-gateways)                                                                                 |
| `NO_COLOR`                               | -                                          | Any non-empty value turns off color in the TUI and in [logs](#logs) written to stderr                                                       |
| `OX_COMPACTION_AUTO_ENABLED`             | `client.compaction.auto_enabled`           | Enable auto-compaction                                                                                                                      |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`   | `client.compaction.auto_threshold_percent` | Percent compaction trigger                                                                                                                  |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`    | `client.compaction.auto_threshold_tokens`  | Absolute compaction trigger                                                                                                                 |
| `OX_EFFORT`                              | `client.effort`                            | Effort level                                                                                                                                |
| `OX_EXTRA_CA_CERTS`                      | `client.extra_ca_certs`                    | Path to a PEM trust bundle                                                                                                                  |
| `OX_MAX_TOKENS`                          | `client.max_tokens`                        | Max tokens per response                                                                                                                     |
| `OX_MAX_TOOL_ROUNDS`                     | `client.max_tool_rounds`                   | Per-turn tool-round cap                                                                                                                     |
| `OX_MODEL`                               | `client.model`                             | Model to use                                                                                                                                |
| `OX_PERMISSION_MODE`                     | `permission.mode`                          | Tool permission mode                                                                                                                        |
| `OX_PROMPT_CACHE_TTL`                    | `client.prompt_cache_ttl`                  | Prompt-cache TTL                                                                                                                            |
| `OX_SESSION_ID`                          | -                                          | Set by `ox` for each `bash` command to the [session's ID](permissions.md#commands-the-assistant-runs)                                       |
| `OX_SHOW_THINKING`                       | `tui.show_thinking`                        | Show extended thinking                                                                                                                      |
| `OX_SHOW_WELCOME`                        | `tui.show_welcome`                         | Paint the welcome splash                                                                                                                    |
| `OX_STATUS_LINE`                         | `tui.status_line`                          | Comma-separated status-line segments                                                                                                        |
| `RUST_LOG`                               | -                                          | [Log filter](#logs), default `warn`                                                                                                         |
| `XDG_CONFIG_HOME`                        | -                                          | Base of the user config directory, default `~/.config`                                                                                      |
| `XDG_DATA_HOME`                          | -                                          | Base of the sessions, the device ID, and the oxide-code login, default `~/.local/share`                                                     |
| `XDG_STATE_HOME`                         | -                                          | Base of the log directory, default `~/.local/state`                                                                                         |

Empty values count as unset. Booleans accept `1`, `true`, `yes`, `on` and `0`, `false`, `no`, `off` (case-insensitive). Integers require ≥ 1 for `OX_MAX_TOKENS` and `OX_MAX_TOOL_ROUNDS`, ≥ 0 for `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`, and `1` to `100` for `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`. Invalid values fail startup naming the variable and accepted values. Matching config keys enforce the same ranges and report file and line.

Library-level variables also honored:

- **`HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY`** (lowercase accepted) route requests through a proxy (`ALL_PROXY` applies when a scheme-specific variable is unset). `NO_PROXY` lists comma-separated bypass hosts.
- **`SSL_CERT_FILE` and `SSL_CERT_DIR`** replace the system trust store on Linux with a PEM bundle or certificate directories.
