# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults.**
2. **User config file**: `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **Project config file**: the nearest `ox.toml` found by walking up from the current directory. Project config cannot set `client.api_key`, `client.base_url`, `client.extra_ca_certs`, `permission.mode`, or `permission.allow`.
4. **Environment variables**: always win.

## Config file

All fields are optional. Only specify the values you want to override.

```toml
# ~/.config/ox/config.toml (user-wide)
# or ox.toml in your project root (per-project, except the user-only keys listed above)

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
  "run-state",
  "thread-title",
]
```

### `[client]`: API connection

| Key                | Type    | Default                     | Description                                               |
| ------------------ | ------- | --------------------------- | --------------------------------------------------------- |
| `api_key`          | string  | -                           | Anthropic API key (user config only)                      |
| `base_url`         | string  | `https://api.anthropic.com` | API base URL (user config only)                           |
| `extra_ca_certs`   | string  | -                           | PEM bundle appended to the trust store (user config only) |
| `model`            | string  | `claude-opus-5-5`           | Model to use                                              |
| `effort`           | string  | per-model (see below)       | Intelligence-vs-latency tier                              |
| `max_tokens`       | integer | per-model (see below)       | Max tokens per response                                   |
| `max_tool_rounds`  | integer | unset (unbounded)           | Per-turn safety cap on tool rounds                        |
| `prompt_cache_ttl` | string  | `"1h"`                      | Prompt-cache TTL (`"5m"` or `"1h"`)                       |

#### `model`: supported models

oxide-code supports `claude-opus-5-5`, `claude-sonnet-5-5`, and `claude-haiku-5-5`, each with a 1M-token context window and adaptive thinking. Any other id, such as a gateway alias, is sent verbatim without `effort`, `thinking`, or context management. Its `max_tokens` defaults to `16000`, it gets no cost estimate, and its automatic compaction trigger stays off unless you set `auto_threshold_tokens`. oxide-code warns about such an id at startup, and for a retired `claude-*` id or a `[1m]` suffix it suggests the matching 5.5 model.

#### `effort`: intelligence tier

`effort` maps 1:1 to the `output_config.effort` body field. Accepted values: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Every supported model accepts all five.

Per-model defaults when `effort` is unset, matching the API's own defaults:

| Model      | Default  |
| ---------- | -------- |
| Opus 5.5   | `medium` |
| Sonnet 5.5 | `high`   |
| Haiku 5.5  | `medium` |

A `/model` swap carries the current effort over to the new model.

Tier guide for the 5.5 models (from Anthropic's [effort documentation](https://platform.claude.com/docs/en/build-with-claude/effort)):

- `max` / `xhigh`: Only where you have measured a quality gain, since turns run much longer and cost more.
- `high`: Knowledge work, long agent tasks, and strict instruction following.
- `medium`: The starting point for most work, including agentic coding and multistep tool use.
- `low`: Chat and short, latency-sensitive tasks. At this level, long agent tasks are more likely to stop early or skip verifying a change.

#### `max_tokens`: response ceiling

When unset, `max_tokens` is `64000` on the supported models, half their 128K output ceiling. `effort` controls how much the model thinks, so this cap only bounds runaway output. Other ids get `16000`, which fits the lower output ceilings of older or smaller models, and the default follows a `/model` swap. Set `max_tokens` (via TOML or `ANTHROPIC_MAX_TOKENS`) to change it.

#### `max_tool_rounds`: agent-loop safety cap

When unset, the agent loop has no per-turn round cap. Setting `max_tool_rounds = N` (or `OX_MAX_TOOL_ROUNDS=N`) bails the turn after `N` rounds with a runaway-loop error. The cap is a guard against tools stuck in a retry loop, since normal agent sessions routinely run hundreds of rounds.

#### `base_url`: endpoint

Use `base_url` only in `~/.config/ox/config.toml` or `ANTHROPIC_BASE_URL`. Project `ox.toml` cannot set it, because project files are loaded from the checkout and should not be able to redirect credentials. The URL must use HTTPS unless it points at localhost for a local proxy.

#### `extra_ca_certs`: corporate trust anchors

oxide-code verifies TLS certificates using the operating system's trust store, so a corporate CA installed system-wide works without extra configuration. For gateways and other private-CA endpoints whose root CA isn't in that store, set `extra_ca_certs` to a PEM bundle to add those roots to oxide-code's trust store:

```toml
[client]
base_url = "https://gw.llm.corp.example/anthropic"
extra_ca_certs = "~/.config/ox/corp-cachain.pem"
```

Use an absolute or `~`-rooted path. Relative paths resolve from the launch cwd. The field is user-config only because a checked-in trust-anchor path could widen TLS trust for the process. Equivalent env var: `OX_EXTRA_CA_CERTS`.

#### `prompt_cache_ttl`: cache duration

Accepted values: `"5m"` (matches the server default as of 2026-03-06) and `"1h"` (higher write premium, bigger hit-rate win on long sessions). oxide-code defaults to `"1h"` because Anthropic's silent 2026-03 TTL drop cut typical prompt-caching savings from 80 %+ to 40-55 %. See [Agentic Request Body Fields](../research/api/anthropic.md#agentic-request-body-fields) for the wire shape and cost analysis.

### `[client.compaction]`: context compression

Auto-compaction is enabled by default for the supported models. The default trigger leaves room for the next response and a safety buffer. Set one threshold override when you want compaction to happen earlier:

| Key                      | Type    | Default       | Description                                               |
| ------------------------ | ------- | ------------- | --------------------------------------------------------- |
| `auto_enabled`           | boolean | `true`        | Enable automatic context compaction                       |
| `auto_threshold_tokens`  | integer | model-derived | Absolute trigger, snapped into `50000`-safe-trigger range |
| `auto_threshold_percent` | integer | model-derived | Percent of context, snapped into the same range           |

`auto_threshold_tokens` and `auto_threshold_percent` are mutually exclusive. Percent must be between `1` and `100`. Either form resolves to a token count clamped into the usable range: values below `50000` tokens snap up to that floor, and values above the model-derived safe trigger snap down. The model-derived ceiling shifts with `/model`, so a global threshold sized for a larger context window keeps working after a swap.

For other model ids, the default and percent-based automatic triggers stay off. An explicit token threshold still works, snapped to the `50000` floor when set lower.

### `[tui]`: Terminal UI

| Key             | Type    | Default   | Description                               |
| --------------- | ------- | --------- | ----------------------------------------- |
| `show_thinking` | boolean | `false`   | Show extended thinking                    |
| `show_welcome`  | boolean | `true`    | Paint the welcome splash on an empty chat |
| `status_line`   | array   | see below | Ordered status-line segments              |

`show_thinking = true` opts the request into `thinking.display = "summarized"` so the API streams reasoning text. Otherwise reasoning stays hidden, and on Opus 5.5 and Sonnet 5.5 the chat still shows the short progress notes the model writes before each tool call.

`show_welcome = false` blanks the chat region until you submit a prompt, which is useful when piping or screen-recording.

`status_line` accepts these segment names: `current-dir`, `git-branch`, `pull-request`, `model`, `model-with-effort`, `context-used`, `session-cost`, `run-state`, `thread-title`, `current-time`. The list must contain at least one segment. Segments with no data, such as a missing git branch, an unopened pull request, or usage before the first completed turn, are omitted. The `pull-request` segment shells out to `gh pr view` so it requires the GitHub CLI on PATH. In supporting terminals, Ctrl-click or Cmd-click the segment to open the PR URL. The separator takes its style from the active theme's `separator` slot and has no setting of its own.

| Segment             | Renders                                      | Refresh         |
| ------------------- | -------------------------------------------- | --------------- |
| `current-dir`       | Tildified working directory                  | At startup      |
| `git-branch`        | Current branch (omitted on detached HEAD)    | Every 5 s       |
| `pull-request`      | Open PR as clickable `#86` when OSC 8 works  | Every 60 s      |
| `model`             | Compact model label (e.g., `Opus 5.5`)       | On `/model`     |
| `model-with-effort` | Model label plus the effort tier in parens   | On `/model`     |
| `context-used`      | `Ctx: 10% (100k/1M)` after the first turn    | Per turn        |
| `session-cost`      | `Sess: $0.4321` running USD estimate         | Per turn        |
| `run-state`         | `Ready` / spinner + label / Ctrl+C exit hint | On state change |
| `thread-title`      | AI-generated session title                   | After turn 1    |
| `current-time`      | `HH:MM` in the local timezone                | Per minute      |

When the row is too narrow to fit every configured segment, low-utility ones drop in this order before the remaining content gets truncated: `thread-title` → `current-time` → `pull-request` → `git-branch` → `current-dir` → `session-cost` → `context-used` → `model` → `model-with-effort` → `run-state`. Run state, the model labels, and the bar's last surviving segment are always preserved.

`OX_STATUS_LINE` accepts the same segment names, comma-separated, with optional whitespace (`OX_STATUS_LINE="model, run-state"`). A value with no segments (`","`), an empty entry between commas (`"model,,run-state"`), or an unknown segment name fails startup with a parse error.

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

### `[permission]`: Tool approval

Every tool call passes a permission check before running. In the default `auto` mode, reads and edits inside the working directory run without asking, and everything else opens an approval prompt. Press `y` to approve or `n` / Esc to deny. Enter does nothing, and the prompt ignores keys until you pause typing for 300 ms, so keystrokes or a paste meant for the input box cannot approve a call. Headless runs (`-p`, `--no-tui`) deny anything that would prompt.

| Key     | Type   | Default  | Description                                                    |
| ------- | ------ | -------- | -------------------------------------------------------------- |
| `mode`  | string | `"auto"` | `"auto"`, `"plan"` (read-only), or `"yolo"` (user config only) |
| `allow` | array  | `[]`     | Rules that run without asking (user config only)               |
| `deny`  | array  | `[]`     | Rules that never run, in every mode                            |

```toml
[permission]
allow = ["bash(cargo test:*)", "bash(git status)", "edit(~/notes/**)"]
deny = ["read(**/.env)", "bash(git push:*)"]
```

Rules follow the format `class(specifier)`. The class is `read` (read, grep, and glob tools), `edit` (edit and write), or `bash`, and a bare class name or `class(*)` matches every call in that class.

- **`bash`** specifiers match commands exactly (`bash(git status)`), by prefix (`bash(cargo test:*)`), or by wildcard (`bash(git *)`). Allow rules never match a command containing `;`, `|`, `&`, a newline, `>`, `<`, `$(`, or a backtick, while deny rules match the full command or any part between `;`, `|`, `&`, or newlines.
- **`read` and `edit`** specifiers are path globs where `*` and `?` match within one path segment and `**` spans segments. A relative glob matches paths inside the working directory, so `edit(*.json)` covers `package.json` but not `src/a.json`. A glob starting with `/` or `~/` matches the absolute path after symlinks resolve.
- **Deny and allow** match paths differently. An allow rule matches only the resolved path, with exact case and spelling. A deny rule also matches the path as the call writes it and, when the rule's literal leading directories run through a symlink at startup, the location they resolve to. It treats names that differ only in case, Unicode normalization, invisible characters, or trailing dots as the same name, so `read(**/.env)` also blocks `.ENV`.

In `auto` mode, edits to these paths need approval even when an allow rule matches: `.git`, `.githooks`, `.husky`, `.vscode`, `.idea`, `.devcontainer`, and `.codex` directories at any depth, shell startup files such as `.bashrc` and `.zshrc`, `.envrc`, `.gitconfig`, `.gitmodules`, `.cargo/config.toml`, `.claude/settings.json`, `.claude/settings.local.json`, `.claude/hooks`, `rust-toolchain.toml`, `ox.toml`, and oxide-code's own config, data, and state directories. Their names compare the way deny rules compare names, in the path as written and as resolved, and a symlinked protected name in the launch directory protects its target as well. A `deny` rule still denies such an edit, and the built-in deny list blocks edits under the launch directory's own `.git`.

Launching from `/` or your home directory turns off the auto-allow for edits inside it, so those edits need an allow rule or an approval. On macOS and Linux, so does an edit to a file with more than one hard link.

`plan` mode denies all edit and `bash` calls. `yolo` mode never asks and skips the built-in deny list (`rm -rf`, `curl | sh`, and similar), but it still honors your `deny` rules. A project `ox.toml` can only add `deny` rules, and setting `mode` or `allow` there fails the config load, so a repository can restrict what runs but never widen it. The environment variable `OX_PERMISSION_MODE` overrides `mode`.

## Authentication

oxide-code checks three credential sources in order:

1. `ANTHROPIC_API_KEY` environment variable.
2. `api_key` under `[client]` in the user config file.
3. Claude Code OAuth credentials, if [Claude Code](https://code.claude.com/docs) is installed and signed in:
   - **macOS**: the `"Claude Code-credentials"` Keychain entry (preferred), falling back to `~/.claude/.credentials.json`.
   - **Linux**: `~/.claude/.credentials.json`.

   Expired tokens are refreshed automatically. No configuration needed.

The environment variable and OAuth keep credentials out of config files. An `api_key` in `~/.config/ox/config.toml` works but sits on disk in plaintext, and a project `ox.toml` cannot set it.

## Environment variables

Environment variables override all config file values.

| Variable                               | Config key                                 | Default                     | Description                  |
| -------------------------------------- | ------------------------------------------ | --------------------------- | ---------------------------- |
| `ANTHROPIC_API_KEY`                    | `client.api_key`                           | -                           | Anthropic API key            |
| `ANTHROPIC_BASE_URL`                   | `client.base_url`                          | `https://api.anthropic.com` | API base URL                 |
| `ANTHROPIC_MODEL`                      | `client.model`                             | `claude-opus-5-5`           | Model to use                 |
| `ANTHROPIC_EFFORT`                     | `client.effort`                            | per-model                   | Intelligence-vs-latency tier |
| `ANTHROPIC_MAX_TOKENS`                 | `client.max_tokens`                        | per-model                   | Max tokens per response      |
| `OX_EXTRA_CA_CERTS`                    | `client.extra_ca_certs`                    | -                           | Path to a PEM trust bundle   |
| `OX_MAX_TOOL_ROUNDS`                   | `client.max_tool_rounds`                   | unset (unbounded)           | Per-turn tool-round cap      |
| `OX_PROMPT_CACHE_TTL`                  | `client.prompt_cache_ttl`                  | `1h`                        | Prompt-cache TTL             |
| `OX_COMPACTION_AUTO_ENABLED`           | `client.compaction.auto_enabled`           | `true`                      | Enable auto-compaction       |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`  | `client.compaction.auto_threshold_tokens`  | model-derived               | Absolute compaction trigger  |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT` | `client.compaction.auto_threshold_percent` | model-derived               | Percent compaction trigger   |
| `OX_PERMISSION_MODE`                   | `permission.mode`                          | `auto`                      | Tool permission mode         |
| `OX_SHOW_THINKING`                     | `tui.show_thinking`                        | `false`                     | Show extended thinking       |
| `OX_SHOW_WELCOME`                      | `tui.show_welcome`                         | `true`                      | Paint the welcome splash     |
| `OX_STATUS_LINE`                       | `tui.status_line`                          | see `[tui]`                 | Comma-separated segments     |

Every variable treats an empty value as unset. Boolean variables accept `1`, `true`, `yes`, or `on` to enable and `0`, `false`, `no`, or `off` to disable, in any letter case. Any other value makes `ox` exit at startup with an error naming the variable.
