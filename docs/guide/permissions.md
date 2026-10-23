# Permissions

Every tool call passes a permission check before running. In the default `manual` mode, reads, read-only commands such as `ls`, and edits inside the working directory or an [extra root](#workspace-roots) run without asking, and every other call asks. Your [rules](#rules) adjust that call by call: a deny rule blocks a call in every mode, an ask rule makes a call ask even when it would otherwise run, and an allow rule lets a call run without asking. The [built-in rules](#built-in-rules) already deny reads of `~/.ssh` and credential files and ask before edits to protected config.

## Modes

- **`manual`**, the default, asks before tool calls that no permission rule allows or denies.
- **`plan`** is read-only, so it denies every edit, `exec`, and `net` call, including read-only commands such as `ls`. Reads behave as in `manual`, and the `tasks`, `bash_output`, and `bash_stop` tools count as reads, so they still run.
- **`yolo`** runs tool calls without asking unless a deny rule or one of your ask rules holds them. A deny rule, built-in or yours, denies the call, and an ask rule you configured still asks. The [built-in rules](#built-in-rules) that ask don't apply, so `yolo` edits `.git/hooks` and runs `rm -rf build` without asking. A `bash` command that uses shell syntax `ox` cannot analyze, such as arithmetic or a statement that sets `PATH`, runs unless a deny pattern or one of your ask patterns matches what `ox` can read of it. In `manual`, such a command asks, and rewriting it as plain commands avoids the prompt. A headless run denies the calls that would ask.

In every mode, an `exec` deny or ask pattern is a guardrail against mistakes and cannot hold back a model that works around it, since a pattern that names a program cannot match a copy of it under another name, as in `cp /bin/rm ./x && ./x -rf ~`, and no pattern sees what a program runs or writes once it starts, such as `python3 -c`, `make`, a build script, or a git alias. This matters most in `yolo`, which runs whatever such a pattern misses. A `read` or `edit` deny or ask pattern does hold for the file tools in every mode where it applies. To run untrusted work unattended, run `ox` inside a sandbox or container.

Set the mode with `mode` under `[permission]` in your user config. The environment variable `OX_PERMISSION_MODE` overrides `mode`, and `ox --mode <mode>` overrides both for one run.

During a session, `/mode` opens a picker of the three modes, and `/mode <mode>` switches directly. **Shift+Tab** switches between `manual` and `plan`, and its cycle includes `yolo` only when the session started in `yolo`, so a stray key press cannot run every call unprompted. A switch during a turn takes effect with the turn's next tool call. A command already running in the foreground or as a background shell continues after the switch to `plan`. The default status line includes the `permission-mode` [segment](configuration.md#tui-terminal-ui), which displays `plan` or `yolo`. To display the mode in a custom `status_line`, include `permission-mode`. Restarting restores the configured mode.

## Approval Prompt

When a call asks, the TUI opens an approval prompt. The prompt names the pattern or built-in rule that made the call ask, and a denied call tells the model which rule or mode denied it. Press **Y** to approve, **A** to approve and add a session rule that allows matching calls, **N** to deny, **F** to deny and tell the model why, or **Esc** to deny and end the turn. With **F**, type the feedback and press **Enter**, and the model receives it as the call's result. **Enter** alone does nothing in the prompt, and the prompt ignores keys until you pause typing for 300 ms, so keystrokes or a paste meant for the input box cannot answer it.

The `--no-tui` REPL asks on stderr with the same choices and reads the answer from the next input line, using `x` to end the turn. It asks only when stdin is a terminal. A REPL reading piped input and a headless run (`-p`) cannot ask, so they deny the call and tell the model the config change that would let it through, such as adding `"cargo test"` to the `exec` list under `[permission.allow]`, or otherwise the rule that made it ask.

The **A** key opens the pattern of the session rule it will add, in the syntax below, so you can widen or narrow it before pressing **Enter**. The pattern must still cover the call being approved. In the **A** and **F** editors, **Esc** goes back to the choice. The prompt offers **A** only when such a pattern would let the call run, so these calls offer none:

- An edit to a [protected path](#built-in-rules)
- A call an `ask` pattern matches
- A chained command, such as `cargo test | tail`
- A command with a word that expands or globs, as in `ls *.rs`, or with an invisible character

Session rules live in memory until you exit, `/clear`, `/archive`, or `/resume` another session, and `/status` lists them. Deny patterns, ask patterns, and protected paths still take precedence over them. To keep a session rule, add its pattern to the matching class list under `[permission.allow]` in your user config. `/config` lists the rules, extra roots, and turned-off defaults the config files set, each with the layer that added it.

## Settings

Set these keys under `[permission]` in `~/.config/ox/config.toml` or a drop-in. A project's `.ox/config.toml` accepts only `deny` and `ask`, as [Project Config](configuration.md#project-config) explains.

| Key           | Type   | Default    | Description                                                                                          |
| ------------- | ------ | ---------- | ---------------------------------------------------------------------------------------------------- |
| `mode`        | string | `"manual"` | `"manual"`, `"plan"`, or `"yolo"`, [user config only](configuration.md#project-config)               |
| `extra_roots` | array  | `[]`       | More directories whose edits run without asking, [user config only](configuration.md#project-config) |
| `deny`        | table  | empty      | Calls that never run, in every mode                                                                  |
| `ask`         | table  | empty      | Calls that ask first, even when an allow pattern matches                                             |
| `allow`       | table  | empty      | Calls that run without asking, [user config only](configuration.md#project-config)                   |
| `defaults`    | table  | all on     | Built-in rules to turn off by name, [user config only](configuration.md#project-config)              |

```toml
# ~/.config/ox/config.toml
[permission]
mode = "manual"
extra_roots = ["~/src/shared-lib"]

[permission.deny]
read = ["~/.aws/**", "/**/.env"]

[permission.ask]
exec = ["git push *"]

[permission.allow]
exec = ["cargo test *", "git status"]
edit = ["~/notes/**"]
```

## Rules

Allow patterns match narrowly, as written, while deny and ask patterns match broadly, across every spelling of a call. So `allow.exec = ["git status"]` covers `git status` but not `git status -s`, while `deny.exec = ["rm"]` blocks every `rm` command.

### Rule Tables

Each of `deny`, `ask`, and `allow` holds a pattern list per class: `read` (read, grep, glob, tasks, bash_output, and bash_stop tools), `edit` (edit and write), `exec` (bash), `net`, and `mcp`. `bash_output` and `bash_stop` belong to `read` because they act only on shells that an allowed `bash` call started. Inside `[permission]`, the dotted form `ask.exec = ["git push *"]` works too.

Deny wins over ask, and ask wins over allow, the working-directory allow, and session rules, so `ask.exec = ["git push *"]` asks even when an allow pattern covers the push. The pattern `*` matches every call in its class. A path pattern never matches a `tasks`, `bash_output`, or `bash_stop` call, so only `deny.read = ["*"]` blocks them. Under that deny, `bash` can still start background shells, which then run unread until the session ends. `allow.read` has no effect, since reads already run without asking.

### `exec` Patterns

An `exec` pattern is one command, matched word by word after quotes are removed and `$'...'` escapes are decoded. A `*` standing alone as a word is the only wildcard. In an allow pattern, it matches exactly one word before the end of the pattern, or any number of words at the end, so `cargo test *` covers `cargo test` and `cargo test --all`, and `git * --force` covers `git push --force` but misses `git push origin --force`. Quote a word to match it as written, as in `'*'`. A glob inside a word, as in `mkfs*`, fails to load, so write a pattern for each command it should cover, such as `mkfs.ext4 *`. A pattern also fails to load when it is empty, holds more than one command or a redirection, or has a word that expands, such as `$HOME`.

A pattern matches one simple command after wrappers such as `env`, `nice`, and `timeout` and redirections are set aside, so `cargo test *` covers `nice cargo test 2>&1`. A wrapper that changes who runs the command or which arguments it gets, which `sudo`, `doas`, `pkexec`, `busybox`, and `xargs` do, stays part of the command for allow patterns, so `rm *` does not allow `sudo rm x`. A wrapper spelled with a directory, such as `./nice`, may be any program, so it stays part of the command as well. A leading `VAR=value` assignment is set aside too when it only changes output, as `RUST_LOG`, `LANG`, and `NO_COLOR` do. Any other assignment stays part of the command, so `CARGO_TARGET_DIR=/tmp cargo test` needs a pattern that names it.

A command chained with `;`, `&&`, `||`, or `|` is allowed only when every part is, and writing a file with `>` also needs that file to be inside the working directory or an extra root, or allowed by an `edit` pattern. A deny or ask pattern matches when any part matches, including a quoted, full-path, or differently cased command name such as `/bin/rm` or `RM`, and the command a wrapper runs, so `deny.exec = ["rm *"]` denies `sudo rm x`, `xargs rm`, and `find . -exec rm {} +` in every mode. Its words after the first need only appear in order, with any words between them, so `ask.exec = ["git push *"]` also asks for `git -c a=b push`. A word that expands or globs, such as `$X`, `${X:-push}`, or `pu*`, matches any word of a deny or ask pattern, since it may become that word when the command runs. Patterns also see the commands inside a `$(...)` substitution and a literal `sh -c` or `eval` script, so `sh -c 'npm test'` needs patterns for both `npm test` and `sh -c *`. A command that uses shell syntax ox cannot analyze, such as arithmetic, a function definition, or a statement that sets `PATH` or another uppercase variable, is never allowed by a pattern, so it asks in `manual`, naming the construct, unless a deny pattern matches. So does a command that changes directory and then names a relative path as a program, a file to read, or a redirection target, such as `cd sub && ./run.sh`, and one that runs anything after a `cd` that may leave the working directory and extra roots, such as `cd /tmp/x && make` or `cd $DIR && make`, since the path may then name another file, and the program may read another directory's files. Deny and ask patterns see inside the wrappers above, `find -exec` and its variants, and a literal `sh -c` or `eval` script, but not inside other programs that run a command, such as `watch`, `fd -x`, `make`, or `python3 -c`. In `yolo`, such a command runs unless a pattern names the runner itself. Read-only commands such as `ls`, `cat`, `grep`, `[ -f x ]`, `find` without `-exec` or `-delete`, and `sed` without `-i` run without asking when the command runs under its bare name, every argument is a literal word without a glob, and nothing in the whole command expands a variable, so `echo $TOKEN` and `cat <<< $TOKEN` ask. A recursive `grep` asks, and so does `cat src/*.rs`, which an allow pattern such as `cat *` covers.

`read` patterns reach a `bash` command in two places. A file read through `<` faces them like a read tool's file, so `deny.read = ["/**/.env"]` denies `sort < .env`. A file reader (`cat`, `find`, `grep`, `head`, `ls`, `sed`, `tail`, or `wc`) asks when a deny or ask `read` pattern matches one of its file arguments or a file one of its globs matches when the call is checked, so `cat ~/.ss*/id_rsa` asks even under `allow.exec = ["cat *"]`. An argument that expands a variable, as in `cat $F`, may name any file, as may the input words of `xargs cat`, the paths of `find . -exec cat {} +`, and a glob `ox` cannot list, so outside `yolo` it asks whenever a deny or ask `read` pattern exists, and the built-in `ssh` rule is one. `yolo` holds a reader only for the files a deny pattern or one of your ask patterns matches by name, where a leading `~`, `$HOME`, or `${HOME}` names the home directory, and denies it when a deny pattern matches. Any other program can open whatever files it likes once it runs, so an `exec` allow pattern such as `python3 *` can run its command on files the `read` patterns hold. Keep such programs out of your allow patterns where `read` patterns guard files, since an `exec` deny pattern for them is only a guardrail.

### Path Patterns

`read` and `edit` patterns are globs in the style of `.gitignore`, except that a leading `/` means the file system root. A pattern with no `/` other than a trailing one, such as `.env`, matches that name at any depth below the working directory and each extra root. A pattern that starts with `./` or has a `/` anywhere except at the end, such as `./secrets` or `src/*.json`, starts at those directories. A pattern that starts with `/` or `~/` is an absolute path, so `/**/.env` matches a `.env` anywhere on disk. `*` and `?` match within one path segment, `[abc]` matches one listed character, `{a,b}` matches either alternative, `**` spans segments, a trailing `/` is ignored, and a pattern also covers everything below a path it matches, so `secrets` covers `secrets/key`. A pattern with a `.` or `..` segment fails to load, so name a path outside these directories from `/` or `~/`. A leading `!` fails to load too, since rules have no negation. On Windows, `\` separates segments like `/`, and an absolute pattern starts with a drive, as in `C:/Users/me/.env`, so one that starts with `/` alone fails to load. A leading `!` fails to load as well, since patterns have no negation.

Allow patterns match a path only as it resolves, with exact case and spelling. Deny and ask patterns also match the path as the call writes it and, when the pattern's literal leading directories run through a symlink at startup, the location they resolve to. They treat names that differ only in case, Unicode normalization, invisible characters, or trailing dots as the same name, so `deny.read = [".env"]` also blocks `.ENV`. A `read` deny or ask pattern also hides the files it matches from a `grep` or `glob` over their directory, and the output counts the hidden files without naming them.

### `net` and `mcp` Patterns

A `net` pattern is a host glob such as `*.example.com`, matched without regard to case, and fails to load if it holds a scheme, port, path, or whitespace. An `mcp` pattern is a `server/tool` glob such as `github/get_*`, where `*` stays within the server or tool name, and fails to load unless one `/` separates two non-empty names. No built-in tool uses `net` or `mcp` yet, so these lists load but match nothing. A `bash` command that reaches the network, such as `curl`, is an `exec` call, so deny it with an `exec` pattern such as `curl *`.

## Built-In Rules

oxide-code ships the rules below. The ones that ask about edits protect their paths: such an edit asks even when an allow pattern or the working-directory allow covers it. `yolo` skips every built-in rule that asks and keeps the reads that `ssh` and `credentials` deny. Protected file names match the way deny patterns do, ignoring case, Unicode normalization, invisible characters, and trailing dots, and a symlinked protected name in the launch directory or an extra root protects its target as well.

| Name          | Rule                                                                                                                                                                                                                                                            |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `git`         | Edits to `.git`, `.githooks`, and `.husky` at any depth, to `.gitconfig` and `.gitmodules` anywhere, and to the XDG git directory, such as `~/.config/git`, and `~/.git-credentials` ask                                                                        |
| `direnv`      | Edits to `.envrc` anywhere ask                                                                                                                                                                                                                                  |
| `editors`     | Edits to `.vscode`, `.idea`, and `.devcontainer` at any depth ask                                                                                                                                                                                               |
| `cargo`       | Edits to `.cargo/config`, `.cargo/config.toml`, `rust-toolchain`, and `rust-toolchain.toml` ask                                                                                                                                                                 |
| `shell`       | Edits to shell startup and logout files such as `.bashrc`, `.zshrc`, and `.bash_logout`, and to fish's `config.fish`, `conf.d`, and `functions`, ask                                                                                                            |
| `agents`      | Edits to `.codex` at any depth, `.claude/settings.json`, `.claude/settings.local.json`, `.claude/hooks`, the `settings.json` and `hooks` in `$CLAUDE_CONFIG_DIR`, `.claude.json`, and `.mcp.json` ask                                                           |
| `ox`          | Edits to `.ox` at any depth and oxide-code's own config, data, and state directories ask                                                                                                                                                                        |
| `devices`     | Edits under `/dev` ask on Unix, as does a `bash` redirection to any device there except `/dev/null`, `/dev/stdout`, `/dev/stderr`, and `/dev/tty`                                                                                                               |
| `rm`          | An `rm` with both its recursive and force options asks in any spelling, such as `rm -rf x`, `rm -r --force x`, `rm x -Rf`, and `sudo rm -rf x`                                                                                                                  |
| `ssh`         | Reads of `~/.ssh` and everything in it are denied, and edits to anything in it ask                                                                                                                                                                              |
| `cloud`       | Edits to `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.docker`, and `~/.kube` ask                                                                                                                                                                               |
| `credentials` | Reads of oxide-code's `credentials.json`, Claude Code's `.credentials.json` in `~/.claude` or `$CLAUDE_CONFIG_DIR`, and git's `~/.git-credentials` and `~/.config/git/credentials` are denied, as are reads of any process's environment under `/proc` on Linux |

Built-in rules sit in the deny and ask tiers, so an allow pattern cannot override one. To turn one off, set its name to `false` under `[permission.defaults]` in user config. A later drop-in can set it back to `true`.

```toml
[permission.defaults]
cargo = false
```

## Workspace Roots

Each `extra_roots` entry is an absolute or `~/` path, such as `"~/src/shared-lib"`, and the user config and drop-ins add to one list. Symlinks in it resolve at startup, and a relative entry, `/`, your home directory, or a parent of it fails the config load. Protected paths inside an extra root still need approval outside `yolo`.

Launching from `/`, your home directory, or a parent of it turns off the auto-allow for every edit, `extra_roots` included, so those edits need an allow pattern or an approval. So does an edit to a file with more than one hard link.
