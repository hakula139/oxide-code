# Permissions

Every tool call passes a permission check before running: deny rules block, ask rules prompt for approval, allow rules and session grants permit, and the active mode decides everything else. Read-only calls confined to session state (such as reading a background shell's output) run after deny and ask checks without classification or approval.

See [SECURITY.md](../../SECURITY.md#trust-model) for guardrail limits and when to use an external sandbox.

## Modes

| Mode             | Calls no rule matches                                                    |
| ---------------- | ------------------------------------------------------------------------ |
| `auto` (default) | A separate model allows or asks, and the call asks when it cannot answer |
| `manual`         | Ask, including reads and edits inside the workspace                      |
| `plan`           | Deny edit, exec, and net calls, and allow read-only calls                |
| `yolo`           | Allow                                                                    |

`plan` blocks `edit`, `exec`, and `net` calls before checking ask and allow rules. Configured deny and ask rules apply in every mode and cannot be overridden by the `auto` classifier. The `auto` classifier works as follows:

- **Input**: Receives user-authored session prompts, the complete validated tool input, resolved native paths, and working directory and workspace roots. Excludes assistant text and tool output.
- **Compaction**: Keeps the active turn's prompts across compaction. After resuming a compacted session, it uses visible user text plus your new follow-up, and asks if context is insufficient.
- **Requests**: Uses the configured endpoint and credentials with its own model. Its cost counts toward session usage without affecting the main model's context estimate. It reads no files, runs no tools, and caches no verdicts.

Set `permission.mode` in user config. `OX_PERMISSION_MODE` overrides it, and `ox --mode <mode>` overrides both for one run.

In a session, `/mode` opens the picker and `/mode <mode>` switches directly. **Tab** and **Shift+Tab** cycle `auto` → `manual` → `plan` → `yolo` (wrapping at both ends), with the footer showing the next mode. Changes apply before the next gated action (including after an in-flight classification) while running commands continue. Restarting restores the configured mode.

## Approval Prompt

When a call requires approval, the TUI shows the full diff or command with the matching rule or classifier reason.

| Key         | Effect                                                                           |
| ----------- | -------------------------------------------------------------------------------- |
| **Y**       | Approve the call                                                                 |
| **A**       | Approve, and add a session rule that allows matching calls                       |
| **N**       | Deny the call                                                                    |
| **F**       | Deny, then type feedback and press **Enter** for the model to read as the result |
| **Esc**     | Deny and end the turn                                                            |
| Scroll keys | Scroll a body longer than 16 rows                                                |

Scroll keys are **Up** / **Down**, **PageUp** / **PageDown**, and **Home** / **End**, with a `rows a-b of n` indicator. **Enter** alone does nothing, and the prompt ignores input until a 300 ms typing pause so type-ahead or pastes cannot answer it. Denials tell the model which rule or mode blocked the call.

Headless runs (`-p`) cannot prompt, so they deny the call and tell the model either the config change that would allow it (such as adding `"cargo test"` to `exec` under `[permission.allow]`) or the rule that forced a prompt.

**A** opens the proposed session pattern in the syntax below so you can widen or narrow it before pressing **Enter** (it must still match the current call). **Esc** returns from the **A** and **F** editors to the prompt. **A** is omitted when no session pattern would permit the call:

- Calls matched by an `ask` pattern
- Chained commands, such as `cargo test | tail`
- Commands with expanding or globbing words (as in `ls *.rs`) or invisible characters

Session rules stay in memory until exit, `/clear`, `/archive`, or `/resume`, and appear in `/status`. Deny and ask rules still take precedence. Persist a session rule by adding it to the matching list under `[permission.allow]` in user config.

## Settings

Configure `mode`, `extra_roots`, `deny`, `ask`, `allow`, and `auto` under `[permission]` in user config or drop-ins (project `.ox/config.toml` accepts only `deny` and `ask`). See [Config Keys](configuration.md#config-keys) for types and defaults and `/config` for resolved layers.

```toml
# ~/.config/ox/config.toml
[permission]
mode = "auto"
extra_roots = ["~/src/shared-lib"]

[permission.auto]
model = "claude-haiku-5-5"
instructions = ["Ask before publishing or deploying changes."]
```

## Pattern Cookbook

### Allow a Command with Any Flags

```toml
[permission.allow]
exec = ["cargo test *"]
```

Trailing `*` matches bare `cargo test` and any arguments. [Chained commands](#chained-commands-and-unsupported-syntax) need a pattern for each part.

### Deny a Secret File

```toml
[permission.deny]
read = ["/**/.env"]
```

`/**/.env` matches `.env` anywhere on disk. See [Path Patterns](#path-patterns) for other forms.

### Ask Before a Push

```toml
[permission.ask]
exec = ["git push *"]
```

Prompts even when a broader allow rule matches, since [ask wins over allow](#rule-tables).

## Rules

Allow patterns match narrowly as written, while deny and ask patterns match broadly across alternate spellings. `allow.exec = ["git status"]` covers `git status` alone (not `git status -s`), whereas `deny.exec = ["rm"]` blocks every `rm` command.

### Rule Tables

Each of `deny`, `ask`, and `allow` holds a pattern list per class (dotted form `ask.exec = ["git push *"]` inside `[permission]` works too).

| Class  | Tools                                                              |
| ------ | ------------------------------------------------------------------ |
| `read` | `read`, `grep`, `glob`, `tasks`, `bash_output`, `bash_stop`, `ask` |
| `edit` | `edit`, `write`                                                    |
| `exec` | `bash`                                                             |
| `net`  | None yet                                                           |
| `mcp`  | None yet                                                           |

Precedence is deny → ask → allow / session rules, so `ask.exec = ["docker *"]` still asks when `allow.exec = ["docker ps"]` matches. `*` matches every call in its class. Path patterns never match `tasks`, `bash_output`, `bash_stop`, or `ask`, so only `deny.read = ["*"]` blocks them (under that deny, `bash` can still start background shells that run unread until the session ends). `allow.read` permits matching reads before mode fallback.

### `exec` Patterns

An `exec` pattern matches one command word by word after stripping quotes and decoding `$'...'` escapes. Lone `*` is the only wildcard, so in-word globs such as `mkfs*` fail to load (write separate patterns such as `mkfs.ext4 *`). Empty patterns, multi-command strings, redirections, and expanding words such as `$HOME` also fail to load across deny, ask, and allow.

#### Allow Patterns

In allow patterns, a lone `*` matches one word mid-pattern or any number of words at the end, so `git * --force` covers `git push --force` but misses `git push origin --force`. Quote a literal asterisk as `'*'`.

#### Programs and Assignments

Patterns match the literal program and arguments, keeping wrappers (`env`, `sudo`, `bash -c`, `eval`, `xargs`) in place: `cargo *` covers `cargo test`, `env cargo test` needs `env cargo *`, and `bash *` permits a literal `bash -c` script without inspecting it.

Leading assignments stay in the match (`A=1 cargo test` needs `A=1 cargo *`). Unquoted variables and tilde expansions are dynamic and disallowed in patterns. Quote them to match literally.

#### Chained Commands and Unsupported Syntax

Commands chained with `;`, `&&`, `||`, or `|` pass allow patterns only when every simple command matches (rules may combine, so `allow.exec = ["cargo test *", "git status"]` permits `cargo test --all && git status`).

Dynamic words, redirections, and unsupported compound syntax skip command-specific allow patterns and fall back to the mode. Whole-class `allow.exec = ["*"]` permits any Bash call after deny and ask checks, including unsupported syntax.

#### What Deny and Ask Patterns Catch

Deny and ask patterns match any extracted simple command, including quoted, full-path, or differently cased program names (`deny.exec = ["git push *"]` matches `/bin/git push`). Subsequent words match in order across intervening arguments (also matching `git -c a=b push` and `git stash push`), and dynamic words match any pattern word. Leading assignments are retained, and the program after them is checked too.

Rules inspect only literal command words, so a pattern for `rm` does not inspect strings passed to `bash -c`, commands under `sudo`, or child programs launched by a script.

#### Native Paths and Bash

`read` and `edit` path rules govern native file tools and search walks, while Bash uses `exec` rules and the active mode. The gate infers no filesystem effects from shell commands, redirections, interpreters, wrappers, or child processes, so `deny.read = [".env"]` blocks native `read` but not `cat .env`. Use `exec` rules or classifier instructions for shell actions, and an external sandbox for filesystem isolation.

### Path Patterns

`read` and `edit` patterns use `.gitignore`-style globs, except leading `/` means the filesystem root:

| Pattern                                                           | Matches                                                                              |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| No `/` other than a trailing one, as in `.env`                    | That name at any depth below the working directory and each extra root               |
| A leading `./` or an inner `/`, as in `./secrets` or `src/*.json` | Paths starting at those directories                                                  |
| A leading `/` or `~/`                                             | An absolute path                                                                     |
| `/**/` followed by a name, as in `/**/.env`                       | That name under any file-system root, including every drive and UNC share on Windows |

`*` and `?` match within one segment, `[abc]` matches one character, `{a,b}` matches either alternative, and `**` spans segments. Trailing `/` is ignored, and matching a path covers everything beneath it (`secrets` covers `secrets/key`).

Patterns with `.` or `..` segments or leading `!` (no negation) fail to load. Name outside paths from `/` or `~/`. On Windows, `\` acts like `/`, absolute patterns start with a drive (such as `C:/Users/me/.env`), `/**/` works across roots, and other bare `/` prefixes fail to load.

Allow patterns match only the resolved path with exact case and spelling. Deny and ask patterns also match:

- The path as written and equivalent directory aliases.
- Symlink targets in the pattern's literal leading directories, resolved at startup or check time.
- Names differing only in case, Unicode normalization, invisible characters, or trailing dots (`deny.read = [".env"]` also blocks `.ENV`).

A `read` deny or ask pattern hides matching files from `grep` and `glob`, counting hidden files without naming them.

### `net` and `mcp` Patterns

`net` patterns are case-insensitive host globs (such as `*.example.com`) and reject schemes, ports, paths, and whitespace. `mcp` patterns are `server/tool` globs (such as `github/get_*`) and fail to load unless one `/` separates two non-empty names with `*` confined to each side. No built-in tool uses `net` or `mcp` yet, so these lists load without matching. Network commands in `bash` (such as `curl`) are `exec` calls and need `exec` patterns such as `curl *`.

## Workspace Roots

`extra_roots` entries are absolute or `~/` paths (such as `"~/src/shared-lib"`), concatenated across user config and drop-ins. Symlinks resolve at startup, and relative entries fail the load. The launch directory comes first, followed by distinct resolved roots in config order. Roots anchor relative path rules and supply classifier context without granting edit permission.

## Commands the Assistant Runs

- **No terminal**: `bash` runs without a controlling terminal, so commands prompting on `/dev/tty` fail immediately (`sudo`, password `ssh`, HTTPS `git push` without a credential helper).
- **Editors**: `EDITOR`, `VISUAL`, and `GIT_EDITOR` are set to `true`, so editor steps keep default text (`git rebase --continue` succeeds, while `git commit` without `-m` stops with `Aborting commit due to empty commit message`).
- **`CDPATH`**: Unset so directory changes ignore the caller's search path.
- **`OX_SESSION_ID`**: Set to the active session's ID.
- **Credentials**: See [Commands and Credentials](authentication.md#commands-and-credentials) for hidden variables.
