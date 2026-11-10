# Permissions

Every tool call passes a permission check before running. Deny rules block matching calls, ask rules require approval, and allow rules or session grants permit them. The selected mode decides every other call. Read operations confined to session state, such as checking a shell task's output, run after the deny and ask checks without classification or approval.

[SECURITY.md](../../SECURITY.md#trust-model) describes the limits of these guardrails and when an external sandbox is needed.

## Modes

| Mode             | Calls no rule matches                                                                                  |
| ---------------- | ------------------------------------------------------------------------------------------------------ |
| `auto` (default) | A separate model allows the call or asks for approval, and the call asks when that model cannot answer |
| `manual`         | Ask, including reads and edits inside the workspace                                                    |
| `plan`           | Deny edit, exec, and net calls, and allow read-only calls                                              |
| `yolo`           | Allow                                                                                                  |

In every mode, your deny and ask rules still apply. The `auto` classifier cannot override them, and it works as follows:

- **Input**: It sees your user-authored prompts from the session, the complete validated tool input, the resolved native path where applicable, and the working directory and workspace roots, but no assistant explanations or tool output.
- **Compaction**: It keeps the active turn's prompts across compaction. After resuming a compacted session, it uses the visible user text and your new follow-up, and asks when that context is insufficient.
- **Requests**: It uses the configured endpoint and credentials with its own model, and its cost joins session usage without changing the main model's context estimate. It does not read files, run tools, or cache decisions.

Set `mode` under `[permission]` in user config. `OX_PERMISSION_MODE` overrides it, and `ox --mode <mode>` overrides both for one run.

During a session, `/mode` opens the picker and `/mode <mode>` switches directly. **Tab** and **Shift+Tab** cycle forward and backward through `auto` → `manual` → `plan` → `yolo`, wrapping at either end, and the footer names the next mode. A change applies before the next gated action, including after an in-flight classification, while commands already running continue. Restarting restores the configured mode.

## Approval Prompt

When a call asks, the TUI opens an approval prompt that shows the whole diff or command along with the matching rule or the classifier's reason.

| Key                                                              | Effect                                                                           |
| ---------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| **Y**                                                            | Approve the call                                                                 |
| **A**                                                            | Approve, and add a session rule that allows matching calls                       |
| **N**                                                            | Deny the call                                                                    |
| **F**                                                            | Deny, then type feedback and press **Enter** for the model to read as the result |
| **Esc**                                                          | Deny and end the turn                                                            |
| **Up** / **Down**, **PageUp** / **PageDown**, **Home** / **End** | Scroll a body longer than 16 rows, with a `rows a-b of n` line for the position  |

**Enter** alone does nothing, and the prompt ignores keys until you pause typing for 300 ms, so keystrokes or a paste meant for the input box cannot answer it. A denied call tells the model which rule or mode denied it.

A headless run (`-p`) cannot ask, so it denies the call and tells the model the config change that would let it through, such as adding `"cargo test"` to the `exec` list under `[permission.allow]`, or otherwise the rule that made it ask.

**A** opens the pattern of the session rule it will add, in the syntax below, so you can widen or narrow it before pressing **Enter**. The pattern must still cover the call being approved. In the **A** and **F** editors, **Esc** goes back to the choice. The prompt offers **A** only when such a pattern would let the call run, so these calls offer none:

- A call an `ask` pattern matches
- A chained command, such as `cargo test | tail`
- A command with a word that expands or globs, as in `ls *.rs`, or with an invisible character

Session rules live in memory until you exit, `/clear`, `/archive`, or `/resume` another session, and `/status` lists them. Deny and ask patterns still take precedence over them. To keep a session rule, add its pattern to the matching class list under `[permission.allow]` in your user config.

## Settings

Set `mode`, `extra_roots`, the `deny`, `ask`, and `allow` rule tables, and the classifier's `auto` table under `[permission]` in user config or a drop-in. A project's `.ox/config.toml` accepts only `deny` and `ask`. [Config Keys](configuration.md#config-keys) lists their types and defaults, and `/config` lists what each layer set.

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

The trailing `*` covers bare `cargo test` and any arguments after it, while a [chained command](#chained-commands-and-unsupported-syntax) needs a pattern for each part.

### Deny a Secret File

```toml
[permission.deny]
read = ["/**/.env"]
```

`/**/.env` matches the name anywhere on disk, and [Path Patterns](#path-patterns) covers the other forms.

### Ask Before a Push

```toml
[permission.ask]
exec = ["git push *"]
```

The push asks even when a broader allow pattern covers it, since [ask wins over allow](#rule-tables).

## Rules

Allow patterns match narrowly, as written, while deny and ask patterns match broadly, across every spelling of a call. So `allow.exec = ["git status"]` covers `git status` but not `git status -s`, while `deny.exec = ["rm"]` blocks every `rm` command.

### Rule Tables

Each of `deny`, `ask`, and `allow` holds a pattern list per class. Inside `[permission]`, the dotted form `ask.exec = ["git push *"]` works too.

| Class  | Tools                                                       |
| ------ | ----------------------------------------------------------- |
| `read` | `read`, `grep`, `glob`, `tasks`, `bash_output`, `bash_stop` |
| `edit` | `edit`, `write`                                             |
| `exec` | `bash`                                                      |
| `net`  | None yet                                                    |
| `mcp`  | None yet                                                    |

Deny wins over ask, and ask wins over allow and session rules, so `ask.exec = ["docker *"]` asks even when `allow.exec = ["docker ps"]` covers the call. The pattern `*` matches every call in its class. A path pattern never matches a `tasks`, `bash_output`, or `bash_stop` call, so only `deny.read = ["*"]` blocks them. Under that deny, `bash` can still start background shells, which then run unread until the session ends. An `allow.read` rule permits matching reads before the mode fallback.

### `exec` Patterns

An `exec` pattern is one command, matched word by word after quotes are removed and `$'...'` escapes are decoded. A `*` standing alone as a word is the only wildcard, so a glob inside a word, as in `mkfs*`, fails to load, and you write a pattern for each command it should cover, such as `mkfs.ext4 *`. A pattern also fails to load when it is empty, holds more than one command or a redirection, or has a word that expands, such as `$HOME`. These load failures apply to deny and ask patterns too.

#### Allow Patterns

In an allow pattern, a lone `*` matches exactly one word before the end of the pattern, or any number of words at the end, so `git * --force` covers `git push --force` but misses `git push origin --force`. Quote a word to match it as written, as in `'*'`.

#### Programs and Assignments

Patterns match the actual program and its arguments. `env`, `sudo`, `bash -c`, `eval`, and `xargs` stay in the words being matched, so `cargo *` covers `cargo test`, while `env cargo test` needs `env cargo *`. A literal script passed to `bash -c` is one argument, so `bash *` permits it without inspecting the script.

Leading assignments stay in the match, so `A=1 cargo test` needs a pattern such as `A=1 cargo *`. An unquoted variable or tilde expansion is dynamic and cannot appear in a pattern. Quote it to match the characters literally.

#### Chained Commands and Unsupported Syntax

A command chained with `;`, `&&`, `||`, or `|` is allowed by patterns only when every simple command is covered. Different rules can cover different parts, so `allow.exec = ["cargo test *", "git status"]` permits `cargo test --all && git status`.

Dynamic words, redirections, and unsupported compound syntax leave the call unmatched by command-specific allow patterns, so it reaches the mode fallback. The whole-class rule `allow.exec = ["*"]` permits any Bash call after deny and ask checks, including calls with unsupported syntax.

#### What Deny and Ask Patterns Catch

A deny or ask pattern matches any extracted simple command, including a quoted, full-path, or differently cased program name, so `deny.exec = ["git push *"]` matches `/bin/git push`. Words after the first need only appear in order, with intervening arguments, so it also matches `git -c a=b push` and `git stash push`. A dynamic word can match any pattern word. Leading assignments are retained, and deny and ask rules also check the actual program after them.

Rules examine the literal program and arguments. A pattern for `rm` does not inspect a string passed to `bash -c`, a command run by `sudo`, or a child program launched by a script.

#### Native Paths and Bash

`read` and `edit` path rules apply to native file tools and search walks, while Bash uses `exec` rules and the selected mode. The gate does not infer which files a shell command, redirection, interpreter, wrapper, or child process reads or writes, so `deny.read = [".env"]` blocks the native read tool but not `cat .env`. Configure an exec rule or classifier instructions for shell actions, and use an external sandbox to enforce filesystem isolation.

### Path Patterns

`read` and `edit` patterns are globs in the style of `.gitignore`, except that a leading `/` means the file system root:

| Pattern                                                           | Matches                                                                              |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| No `/` other than a trailing one, as in `.env`                    | That name at any depth below the working directory and each extra root               |
| A leading `./` or an inner `/`, as in `./secrets` or `src/*.json` | Paths starting at those directories                                                  |
| A leading `/` or `~/`                                             | An absolute path                                                                     |
| `/**/` followed by a name, as in `/**/.env`                       | That name under any file-system root, including every drive and UNC share on Windows |

`*` and `?` match within one path segment, `[abc]` matches one listed character, `{a,b}` matches either alternative, and `**` spans segments. A trailing `/` is ignored, and a pattern also covers everything below a path it matches, so `secrets` covers `secrets/key`.

A pattern with a `.` or `..` segment fails to load, so name a path outside these directories from `/` or `~/`. A leading `!` fails to load too, since rules have no negation. On Windows, `\` separates segments like `/`, and an absolute pattern starts with a drive, as in `C:/Users/me/.env`, while `/**/` works across roots and other patterns that start with `/` alone fail to load.

Allow patterns match a path only as it resolves, with exact case and spelling. Deny and ask patterns also match:

- The path as the call writes it, and equivalent filesystem directory aliases.
- The location a symlink in the pattern's literal leading directories resolves to, at startup or when the call is checked.
- Names that differ only in case, Unicode normalization, invisible characters, or trailing dots, so `deny.read = [".env"]` also blocks `.ENV`.

A `read` deny or ask pattern also hides the files it matches from a `grep` or `glob` over their directory, and the output counts the hidden files without naming them.

### `net` and `mcp` Patterns

A `net` pattern is a host glob such as `*.example.com`, matched without regard to case, and fails to load if it holds a scheme, port, path, or whitespace. An `mcp` pattern is a `server/tool` glob such as `github/get_*`, where `*` stays within the server or tool name, and fails to load unless one `/` separates two non-empty names. No built-in tool uses `net` or `mcp` yet, so these lists load but match nothing. A `bash` command that reaches the network, such as `curl`, is an `exec` call, so deny it with an `exec` pattern such as `curl *`.

## Workspace Roots

Each `extra_roots` entry is an absolute or `~/` path, such as `"~/src/shared-lib"`, and user config and drop-ins add to one list. Symlinks resolve at startup, and a relative entry fails the config load. The launch directory comes first, followed by distinct resolved roots in config order. The roots anchor relative path rules and provide classifier context, without granting edit permission.

## Commands the Assistant Runs

- **No terminal**: A `bash` call runs in a shell with no controlling terminal, so a command that prompts on `/dev/tty` fails at once. That covers `sudo`, `ssh` with password authentication, and `git push` over HTTPS without a credential helper.
- **Editors**: `EDITOR`, `VISUAL`, and `GIT_EDITOR` are set to `true`, so an editor step keeps its default text. `git rebase --continue` keeps the prepared message, while `git commit` without `-m` stops with `Aborting commit due to empty commit message`.
- **`CDPATH`** is unset, so directory changes do not depend on the caller's search path.
- **`OX_SESSION_ID`** holds the session's ID, so a script can tell which session started it.
- **Credentials**: [Commands and Credentials](authentication.md#commands-and-credentials) covers the variables a command cannot see.
