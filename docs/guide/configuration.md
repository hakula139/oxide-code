# Configuration

oxide-code loads configuration from multiple sources, merged in order of increasing priority:

1. **Built-in defaults.**
2. **User config file**: `~/.config/ox/config.toml` (or `$XDG_CONFIG_HOME/ox/config.toml`).
3. **User drop-ins**: `*.toml` files in `~/.config/ox/config.d/` (or `$XDG_CONFIG_HOME/ox/config.d/`), in file-name order.
4. **Project config file**: `.ox/config.toml` in the nearest directory at or above the current one that has one.
5. **Environment variables**: always win.

Run `/config` to see each resolved value next to the layer that set it: `default`, `user`, `config.d/<name>`, `project` (or `ox.toml` for a legacy project file), an environment variable such as `$ANTHROPIC_MODEL`, or `session` after a `/model` or `/effort` change.

## Layers

### Merging

Each file sets only the keys it names, and a later layer wins key by key, so a drop-in that sets `client.effort` keeps the `client.model` from `config.toml`. Tables merge recursively, including `[tui.theme.overrides]`.

- **`permission.allow` and `permission.deny`** concatenate, so the rules from every layer apply.
- **Other arrays**, such as `tui.status_line`, come whole from the last layer that sets them.
- **`auto_threshold_tokens` and `auto_threshold_percent`** act as one setting, so a layer that sets either replaces both.

### Drop-ins

`config.d/` lets a dotfile manager, a Nix module, or a team script own one file without editing your `config.toml`. Drop-ins load after `config.toml`, sorted by file name byte by byte, so a numeric prefix such as `10-team.toml` or `50-nix.toml` sets their order. Hidden files, subdirectories, and files without a `.toml` extension are skipped, and a symlink loads its target.

Drop-ins share the `config.toml` schema and count as user config. An invalid drop-in, including a symlink whose target is missing, stops startup with an error that names the file. A malformed permission rule names the file that holds it in the same way.

### Project config

A project file comes from the checkout, so it cannot set `client.api_key`, `client.base_url`, `client.extra_ca_certs`, `permission.mode`, or `permission.allow`. Setting one fails the config load. This keeps a repository from redirecting your credentials or widening what runs without asking.

A legacy `ox.toml` still loads when it sits in the nearest directory at or above the current one that has either file, below `.ox/config.toml` when that directory has both, and startup warns with the path to move it to. In the TUI the warning appears in the chat, and in `-p` and `--no-tui` runs it goes to stderr.

## Config file

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

When unset, `max_tokens` is `64000` on the supported models, half their 128K output ceiling. `effort` controls how much the model thinks, so this cap only bounds runaway output. Other ids get `16000`, which fits the lower output ceilings of older or smaller models, and the default follows a `/model` swap. Set `max_tokens` (via TOML or `OX_MAX_TOKENS`) to change it.

#### `max_tool_rounds`: agent-loop safety cap

When unset, the agent loop has no per-turn round cap. Setting `max_tool_rounds = N` (or `OX_MAX_TOOL_ROUNDS=N`) bails the turn after `N` rounds with a runaway-loop error. The cap is a guard against tools stuck in a retry loop, since normal agent sessions routinely run hundreds of rounds.

#### `base_url`: endpoint

Set `base_url` in user config or `ANTHROPIC_BASE_URL`. A project config cannot set it, because project files are loaded from the checkout and should not be able to redirect credentials. The URL must use HTTPS unless it points at localhost for a local proxy.

#### `extra_ca_certs`: corporate trust anchors

oxide-code verifies TLS certificates using the operating system's trust store, so a corporate CA installed system-wide works without extra configuration. For gateways and other private-CA endpoints whose root CA isn't in that store, set `extra_ca_certs` to a PEM bundle to add those roots to oxide-code's trust store:

```toml
[client]
base_url = "https://gw.llm.corp.example/anthropic"
extra_ca_certs = "~/.config/ox/corp-cachain.pem"
```

Use an absolute or `~`-rooted path. Relative paths resolve from the launch cwd. The field is user-config only because a checked-in trust-anchor path could widen TLS trust for the process. Equivalent env var: `OX_EXTRA_CA_CERTS`, which wins over this field. `NODE_EXTRA_CA_CERTS` applies only when neither is set (see [LLM gateways](#llm-gateways)).

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

If automatic compaction fails, oxide-code does not send the prompt, and the TUI puts it back in the input. Sending it again retries the compaction. After 3 failures in a row, automatic compaction pauses, so the next prompt goes out without it, and `/status` shows it as paused. A successful `/compact`, a `/model` or `/effort` change, `/clear`, `/archive`, or `/resume` turns it back on.

### `[tui]`: Terminal UI

| Key             | Type    | Default   | Description                               |
| --------------- | ------- | --------- | ----------------------------------------- |
| `show_thinking` | boolean | `false`   | Show extended thinking                    |
| `show_welcome`  | boolean | `true`    | Paint the welcome splash on an empty chat |
| `status_line`   | array   | see below | Ordered status-line segments              |

`show_thinking = true` opts the request into `thinking.display = "summarized"` so the API streams reasoning text. Otherwise reasoning stays hidden, and on Opus 5.5 and Sonnet 5.5 the chat still shows the short progress notes the model writes before each tool call.

`show_welcome = false` blanks the chat region until you submit a prompt, which is useful when piping or screen-recording.

`status_line` accepts these segment names: `current-dir`, `git-branch`, `pull-request`, `pull-request-checks`, `pull-request-review`, `model`, `model-with-effort`, `context-used`, `session-cost`, `five-hour-limit`, `weekly-limit`, `run-state`, `queued-prompts`, `task-progress`, `thread-title`, `session-id`, `theme`, `current-time`. The list must contain at least one segment. Segments with no data, such as a missing git branch, an unopened pull request, context usage before the first completed turn, or usage limits under an API key, are omitted. The three pull request segments shell out to `gh pr view` so they require the GitHub CLI on PATH. In supporting terminals, Ctrl-click or Cmd-click one of them to open the PR, its checks, or its changed files. `pull-request-review` shows only on repositories whose branch protection requires a review, since GitHub reports no review decision otherwise. The separator takes its style from the active theme's `separator` slot and has no setting of its own.

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
| `run-state`           | `Ready` / spinner + label / Ctrl+C exit hint                  | On state change |
| `queued-prompts`      | `Queued: 2` while prompts wait for the running turn           | On change       |
| `task-progress`       | Nothing, since oxide-code keeps no task list                  | Never           |
| `thread-title`        | AI-generated session title                                    | After turn 1    |
| `session-id`          | `ID: 3f2a9c1e`, the prefix `ox -c` resumes                    | On change       |
| `theme`               | `Theme: mocha`, or a theme file's name                        | On `/theme`     |
| `current-time`        | `HH:MM` in the local timezone                                 | Per minute      |

The usage limits come from Claude subscription sign-ins, which report how much of each window is spent with every response. A window disappears once it resets, until the next response reports it again.

When the row is too narrow to fit every configured segment, low-utility ones drop in this order before the remaining content gets truncated: `theme` → `session-id` → `thread-title` → `current-time` → `pull-request-review` → `pull-request-checks` → `pull-request` → `git-branch` → `current-dir` → `queued-prompts` → `session-cost` → `weekly-limit` → `five-hour-limit` → `context-used` → `model` → `model-with-effort` → `run-state`. Run state, the model labels, and the bar's last surviving segment are always preserved.

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

Every tool call passes a permission check before running. In the default `auto` mode, reads anywhere and edits inside the working directory run without asking, and other edits and `bash` calls open an approval prompt. Press `y` to approve, `a` to approve and allow matching calls for the rest of the session, `n` to deny, `f` to deny and tell the model why, or Esc to deny and end the turn. With `f`, type the feedback and press Enter, and the model receives it as the call's result. Enter does nothing at the choice, and the prompt ignores keys until you pause typing for 300 ms, so keystrokes or a paste meant for the input box cannot answer it.

The `--no-tui` REPL asks on stderr with the same choices and reads the answer from the next input line, using `x` to end the turn. It asks only when stdin is a terminal. A REPL reading piped input and a headless run (`-p`) cannot ask, so they deny the call and tell the model which rule to add to the `allow` list, such as `"bash(cargo test)"`.

The `a` key opens the allow rule it will add, in the syntax below, so you can widen or narrow it before pressing Enter. The rule must still cover the call being approved. The prompt offers `a` only when such a rule would let the call run, so a protected path, a chained command such as `cargo test | tail`, or a command containing `*` or an invisible character offers none. The rule lives in memory until you exit, `/clear`, `/archive`, or `/resume` another session, `/status` lists it, and deny rules and protected paths still take precedence over it.

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

- **`bash`** specifiers match commands exactly (`bash(git status)`), by prefix (`bash(cargo test:*)`), or by wildcard (`bash(git *)`). Each specifier matches one simple command after wrappers such as `env`, `nice`, and `timeout` and redirections are set aside, so `bash(cargo test:*)` covers `nice cargo test 2>&1`. A leading `VAR=value` assignment is set aside too when it only changes output, as `RUST_LOG`, `LANG`, and `NO_COLOR` do. Any other assignment stays part of the command, so `CARGO_TARGET_DIR=/tmp cargo test` needs a rule that names it. A command chained with `;`, `&&`, `||`, or `|` is allowed only when every part is, and writing a file with `>` also needs that file to be inside the working directory or allowed by an `edit` rule. A deny rule matches when any part matches, including a quoted or full-path command name such as `/bin/rm`. A command that hides what it runs, such as `$(...)`, `eval`, or `sh -c`, is never allowed by a rule, so it asks unless a deny rule matches. Read-only commands such as `ls`, `cat`, `grep`, `find` without `-exec` or `-delete`, and `sed` without `-i` run without asking.
- **`read` and `edit`** specifiers are path globs where `*` and `?` match within one path segment and `**` spans segments. A relative glob matches paths inside the working directory, so `edit(*.json)` covers `package.json` but not `src/a.json`. A glob starting with `/` or `~/` matches the absolute path after symlinks resolve.
- **Deny and allow** match paths differently. An allow rule matches only the resolved path, with exact case and spelling. A deny rule also matches the path as the call writes it and, when the rule's literal leading directories run through a symlink at startup, the location they resolve to. It treats names that differ only in case, Unicode normalization, invisible characters, or trailing dots as the same name, so `read(**/.env)` also blocks `.ENV`. A `read` deny rule also hides the files it matches from a `grep` or `glob` over their directory, and the output counts the hidden files without naming them.

In `auto` mode, edits to these paths need approval even when an allow rule matches:

- **Directories at any depth.** `.git`, `.githooks`, `.husky`, `.vscode`, `.idea`, `.devcontainer`, `.codex`, `.ox`, and `.claude/hooks`.
- **File names anywhere.** Shell startup files such as `.bashrc` and `.zshrc`, `.envrc`, `.gitconfig`, `.gitmodules`, a legacy `ox.toml`, `rust-toolchain`, and `rust-toolchain.toml`.
- **Parent / file pairs.** `.cargo/config`, `.cargo/config.toml`, `.claude/settings.json`, and `.claude/settings.local.json`.
- **Application directories.** oxide-code's own config, data, and state directories.

Their names compare the way deny rules compare names, in the path as written and as resolved, and a symlinked protected name in the launch directory protects its target as well. A `deny` rule still denies such an edit, and the built-in deny list blocks edits under the launch directory's own `.git`.

Launching from `/` or your home directory turns off the auto-allow for edits inside it, so those edits need an allow rule or an approval. So does an edit to a file with more than one hard link.

`plan` mode denies all edit and `bash` calls. `yolo` mode never asks and skips the built-in deny list (`rm -rf`, `curl | sh`, and similar), but it still honors your `deny` rules. A project config can only add `deny` rules, and setting `mode` or `allow` there fails the config load, so a repository can restrict what runs but never widen it. The environment variable `OX_PERMISSION_MODE` overrides `mode`.

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
- **The Claude Code login** comes from the `"Claude Code-credentials"` Keychain entry on macOS, falling back to `~/.claude/.credentials.json`, which is the only store on Linux.
- **Both logins** refresh automatically near expiry and are sent only to the Anthropic API. With a custom `base_url` and no other credential, startup fails and names `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`, so a personal subscription token never reaches a third-party host.
- **Without a credential**, `ox` starts signed out and shows how to sign in, and prompts fail until `/login` succeeds. `ox -p` and `ox --no-tui` exit at startup with the list of credential options.
- **`/status` and `/config`** show the active source, such as `OAuth (CLAUDE_CODE_OAUTH_TOKEN)`, without its value.

The environment variables and the stored logins keep credentials out of config files. An `api_key` in `~/.config/ox/config.toml` works but sits on disk in plaintext, and a project config cannot set it.

## LLM gateways

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
- **`CLAUDE_CODE_ATTRIBUTION_HEADER=0`** drops the `x-anthropic-billing-header` attribution block from the system prompt. By default, agent turns and `/compact` summaries carry it under every credential, because strict gateways reject requests that lack it. Session titles carry it only under OAuth. As in Claude Code, the block on a custom base URL omits the per-request `cch` hash, so it stays the same from request to request.
- **`CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS=1`** drops `output_config.format` from session-title requests, for gateways whose upstream rejects it. Titles then rely on the prompt alone to return JSON.

oxide-code is stricter than Claude Code in three ways. It never sends a stored login to a custom base URL, it sends one credential even when several are set, and it rejects malformed `ANTHROPIC_CUSTOM_HEADERS` lines where Claude Code skips them.

## Environment variables

Environment variables override all config file values.

| Variable                                 | Config key                                 | Default                     | Description                  |
| ---------------------------------------- | ------------------------------------------ | --------------------------- | ---------------------------- |
| `ANTHROPIC_AUTH_TOKEN`                   | -                                          | -                           | Gateway bearer token         |
| `ANTHROPIC_API_KEY`                      | `client.api_key`                           | -                           | Anthropic API key            |
| `CLAUDE_CODE_OAUTH_TOKEN`                | -                                          | -                           | Long-lived OAuth token       |
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
| `OX_PERMISSION_MODE`                     | `permission.mode`                          | `auto`                      | Tool permission mode         |
| `OX_SHOW_THINKING`                       | `tui.show_thinking`                        | `false`                     | Show extended thinking       |
| `OX_SHOW_WELCOME`                        | `tui.show_welcome`                         | `true`                      | Paint the welcome splash     |
| `OX_STATUS_LINE`                         | `tui.status_line`                          | see `[tui]`                 | Comma-separated segments     |

`ANTHROPIC_EFFORT` and `ANTHROPIC_MAX_TOKENS` still work as aliases for `OX_EFFORT` and `OX_MAX_TOKENS`, which win when both are set.

Every variable treats an empty value as unset. Boolean variables accept `1`, `true`, `yes`, or `on` to enable and `0`, `false`, `no`, or `off` to disable, in any letter case. Numeric variables take a whole number: at least `1` for `OX_MAX_TOKENS` and `OX_MAX_TOOL_ROUNDS`, at least `0` for `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`, and `1` to `100` for `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`. Any other value makes `ox` exit at startup with an error naming the variable and the values it accepts. The matching config keys accept the same ranges, and an out-of-range value there names its file and line.
