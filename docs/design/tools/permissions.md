# Tool Permissions and Approval

Every tool call passes through a permission gate. Rules place calls in one of three tiers: deny, ask, or allow. The mode decides the calls the rules leave open, and `plan` also denies every `edit`, `exec`, and `net` call. Planned extensions, including a classifier and an OS sandbox, are tracked in [#130](https://github.com/hakula139/oxide-code/issues/130).

Companion docs: [research/tools/permissions.md](../../research/tools/permissions.md), [slash/modals.md](../slash/modals.md), [session/file-tracking.md](../session/file-tracking.md).

## Rule Tables

Rules live in three tables under `[permission]`, one for each tier. Each table holds a pattern list for each permission class:

```toml
[permission.deny]
read = ["~/.aws/**", ".env"]
exec = ["git push --force *"]

[permission.ask]
exec = ["git push *"]
edit = ["Cargo.lock"]

[permission.allow]
exec = ["cargo test *", "cargo build *"]
edit = ["~/notes/**"]
```

The classes are `read` (the `read`, `grep`, `glob`, `tasks`, `bash_output`, and `bash_stop` tools), `edit` (`edit` and `write`), `exec` (`bash`), `net`, and `mcp`. Each tool declares its class in its `ToolSpec`. No built-in tool belongs to the `net` or `mcp` class yet, so those lists parse and validate but match no calls. A `bash` command that accesses the network is still an `exec` call. MCP tools join the `mcp` class with [#178](https://github.com/hakula139/oxide-code/issues/178).

A `Rule` consists of a class and a pattern. Its tier is determined by the list that contains it. Every pattern is compiled when the config loads, so a malformed pattern fails startup and names its file. The pattern `*` matches every call in its class, including calls that name no path, such as `tasks`.

## Modes

`permission.mode` picks one of three modes. `OX_PERMISSION_MODE` overrides it and follows `effort`'s behavior for empty values and typos: an empty value is absent, and a typo fails. `--mode` overrides both for one run, and `/config` credits it to the flag.

- **`manual`**, the default, sends every call the rules leave open to the user, as described in the [approval round-trip](#approval-round-trip). If there is nobody to ask, the run denies them.
- **`plan`** is read-only. It denies every `edit`, `exec`, and `net` call, including a known-safe `bash` read, and reads behave as in `manual`.
- **`yolo`** runs every call the rules leave open, except a `bash` command that fails to parse, which asks. Deny and ask rules still hold, shipped defaults included, so `yolo` still asks before editing `.git/hooks` until the `git` default is turned off.

Deny and ask rules apply in every mode. In `plan`, only ask rules for `read` and `mcp` calls still ask, since `plan` denies the other classes first. An `auto` mode, tracked in [#131](https://github.com/hakula139/oxide-code/issues/131), will decide open calls with a classifier. Calls that match the ask tier will still go to the user, because an ask rule records that the user wants to see the call.

The modes correspond to these settings in Claude Code and Codex:

| oxide-code | Claude Code                                     | Codex                                                              |
| ---------- | ----------------------------------------------- | ------------------------------------------------------------------ |
| `manual`   | `acceptEdits`, which also runs workspace edits  | `approval_policy = "untrusted"` with the `workspace-write` sandbox |
| `plan`     | `plan`                                          | the `read-only` sandbox, which also runs commands that only read   |
| `yolo`     | `bypassPermissions`, which also keeps ask rules | `--yolo`, which keeps no rules                                     |

### Switching Modes

`Op::SetMode` switches the engine's policy. The TUI sends it from the `/mode` picker, `/mode <mode>`, or Shift+Tab, while the REPL sends it from `/mode <mode>`. Headless runs send none because `--mode` reaches the engine through `Config` before it starts. The engine confirms each switch with `Event::ModeChanged`, which the TUI records in its session snapshot, the status line shows, and the REPL prints. While idle, the engine applies the switch at once.

During a turn, a compaction, or a sign-in, the op arrives at `await_unless_aborted` or `await_approval`, which keep it in the operation's `Inbox` beside the queued prompts and confirm it on receipt. The gate takes a waiting switch before it weighs each call, so a switch applies from the turn's next gated call, including one in the same round. The engine applies any switch still waiting once the operation ends. Calls already gated keep their decisions, so switching to `plan` does not stop a running command.

Shift+Tab cycles `manual` → `plan` → `manual`. The cycle adds `yolo` after `plan` only when the session started in `yolo`, from config, `OX_PERMISSION_MODE`, or `--mode`. When invoked from a mode outside the cycle, such as `yolo` selected with `/mode`, it returns to `manual`. The TUI advances from the last mode it requested, so pressing again before the engine confirms the first request moves on to the next mode. The key is fixed until configurable key bindings land with [#278](https://github.com/hakula139/oxide-code/issues/278).

## Decision Pipeline

The gate evaluates each call in a fixed order and stops at the first match.

1. **Deny tier** → deny. A deny rule for the call's class matches, or a `bash` command writes a file matched by a deny `edit` rule or redirects input from a file matched by a deny `read` rule.
2. **`plan`** → deny an `edit`, `exec`, or `net` call.
3. **Ask tier** → ask. An ask rule for the call's class matches, a `bash` command fails to parse, an edit-class call targets a protected path, or a `bash` command writes a file matched by an ask `edit` rule or marked as protected. A `bash` command also asks if it redirects input from a file matched by an ask `read` rule, if a deny or ask `read` rule matches a file that a reader in it may read, if a reader takes a word that expands a variable while any such rule exists, or, while the `rm` default is on, if it runs `rm` with both its recursive and force options.
4. **Read-class call** → allow.
5. **Edit-class call inside the workspace** → allow.
6. **Allow tier** → allow. An allow rule or session grant matches the call.
7. **Mode** → allow in `yolo`, and ask in `manual` and `plan`.

Each step settles the call with a `Verdict`, which pairs the `Decision` with the `Basis` that reached it: a configured rule and its tier, the built-in group behind a shipped pattern or a protected path, a session grant, the read or workspace default, the mode, a `bash` command that failed to parse, or for an allowed `bash` command what let each part through. The gate logs every verdict at debug level. A denial names its basis in the result the model reads, such as the `deny.exec` pattern `curl *`, and the approval prompt names the rule that holds the call. A call the mode asks about has no rule to name, so the prompt shows none.

The tiers apply in this order: deny rules outrank all ask and allow rules, and ask rules outrank all allow rules, session grants, and the workspace allow. Steps 4 and 5 come after the ask tier. Thus, `ask.read = ["~/.aws/**"]` makes reading those files ask, and `ask.edit = ["Cargo.lock"]` makes editing the project's lock file ask.

A `bash` command is allowed at step 6 only if it is not opaque, each of its simple commands is allowed by an `exec` rule or is a known-safe read, and every file it writes is inside the workspace or allowed by an `edit` rule. Opaque commands can still be denied. For example, `deny.exec = ["rm -rf *"]` stops `rm -rf x; echo $(date)`, but no rule, including `*`, allows it. Only `yolo` runs an opaque command without asking, unless the command failed to parse, which asks as [decision 15](#design-decisions) explains. The prompt then says that ox could not parse it.

## Path Targets

A path tool's target is resolved before any rules run. The tool's `Tool::prepare` method does this in the call's `ToolCtx`, which carries the session's `Workspace`: its cwd and roots. `prepare` expands a leading `~` or `~/` in the path argument to the home directory, as config patterns do. `Access::for_path` then joins the path to the cwd and walks the path one component at a time, as the kernel does. It follows every symlink it encounters, including dangling links, and processes `..` from the current position in the walk. This prevents a `missing/..` detour from hiding a symlink encountered later. Once the first missing component is reached, nothing further exists, so the remaining path is applied as `create_dir_all` builds it. If the walk meets more than 40 symlink hops, a directory it cannot search, or a file that more components follow, the target stays outside the workspace under its joined spelling with no rooted forms. The tool follows native filesystem behavior. A resolved path that is not valid UTF-8 also stays outside the workspace, because rules match it and the pinned input names it only by its lossy spelling.

`prepare` pins the tool's `file_path` field to the resolved path. The gate, preview, and run all use that prepared call, which gives the approval title the actual destination and ensures the tool opens the path the gate checked. `grep` and `glob` retain the `path` as written in the call, with only `~` expanded, because read deny rules match it, then walk from the root resolved by `prepare`. `read`, `edit`, and `write` check file size and type both by path and on the opened file whenever they read a file, so swapping a file between checks cannot bypass them. `read`, `edit`, and `write` also refuse a symlink swapped into the final path component after the check. On Unix, opens use `O_NOFOLLOW` and `O_NONBLOCK`, preventing a swapped-in FIFO from hanging the open. On Windows, opens use `FILE_FLAG_OPEN_REPARSE_POINT` and fail if the handle is a symlink or junction. Other reparse points, such as a deduplicated file, are opened again through their file system filter, and the tool keeps the handle only if it reaches the same file. `write` creates a new file with `create_new`, which also refuses a dangling link, and truncates an existing file only after the open succeeds. On every platform, a symlink swapped into an intermediate directory between the gate and the open is not covered.

Each resolved target belongs to one zone:

- **Protected**: Edit-class calls ask at step 3 if the target's resolved path or call-spelled path contains a protected name from an enabled [shipped default](#shipped-defaults). They also ask when the target is in a location where such a name directly inside the cwd or a workspace root resolves. For example, `.vscode -> tools/vscode` protects `tools/vscode` too. Names are compared after dropping default-ignorable code points, applying Unicode compatibility case folding, and trimming trailing dots and spaces. On a case-insensitive file system such as APFS, `.VSCODE` and `.vſcode` open as `.vscode`. HFS+ ignores the zero-width `\u{200c}` in `.g\u{200c}it`, and Windows opens `.git.` as `.git`. On Windows, the existing part of the resolved path is spelled as the file system opens it, with each 8.3 short name written in full and NTFS stream suffixes dropped. Thus, `GIT~1` and `.git::$INDEX_ALLOCATION` count as `.git`.
- **Workspace**: Edit-class calls are allowed at step 5 when the target sits inside a workspace root. The roots are the cwd and each `permission.extra_roots` entry, resolved the same way as a target. The path gate and edit tools read the same set. If the cwd is `/`, `$HOME`, or an ancestor of `$HOME`, the workspace has no roots, including extra roots, so launching there leaves every edit to the allow rules. The check also compares file identity, so aliases kept by resolution, such as macOS's case-insensitive `/users/<name>` or the `/System/Volumes/Data` firmlink, count as the directory they name. An existing regular file with more than one hard link is never a workspace target, because another link can sit outside every root.
- **Outside**: Every other target needs an allow rule, approval, or `yolo`.

`Access::Path` records the protected names a target contains by default group. The policy can therefore apply the groups enabled in the config without resolving the path again.

## Command Targets

A `bash` call's target is its command as parsed by tree-sitter-bash. `Access::for_command` splits the command into simple commands across `;`, `&&`, `||`, `&`, pipes, line breaks, subshells, and compound statements. Each simple command drops the wrappers that lead it, with their options: `builtin`, `command`, `coproc`, `env`, `exec`, `nice`, `nohup`, `stdbuf`, `time`, and `timeout`, which change only how the command runs, and `busybox`, `doas`, `pkexec`, `sudo`, and `xargs`, which change who runs it or which arguments it gets. A `coproc` command still asks, as the opaque list below explains, so seeing through it serves deny rules alone. For example, `nice -n 5 cargo test 2>&1` runs the simple command `cargo test`, and `sudo -u root rm -rf x` runs `rm -rf x`. Each wrapper's options are parsed by its own grammar, with short options in clusters and values attached or in the next word, as in `sudo -nu root` and `nice -n5`. `xargs` adds a word for the arguments it reads from its input, which expands and so matches any word of a deny or ask rule, or, under `-I`, `-i`, or `--replace`, marks each word holding the placeholder as expanding. Under `-J`, as in BSD `xargs`, only a word that is exactly the placeholder expands, and the input word is added when no word is. Each `-exec`, `-execdir`, `-ok`, or `-okdir` action of `find` adds its command, up to its `;` or a `+` after `{}`, as a further simple command, in which each word holding `{}` expands. A redirection can carry words bash passes to a command: those after a heredoc operator, as in `cat <<EOF notes.txt`, and those after a redirection's target, as in `git >/dev/null push`. tree-sitter-bash parses them as part of the redirection, so the analysis hands them to the simple command bash gives them: the last one of a pipeline or list, the one a `!` negates, or a command of their own after assignments or redirections that stand alone. Where tree-sitter-bash splits one bash word into parts with nothing or only backslash escapes between them, as in `=="x"` or `"x"\ "y"`, the parts form one word. An argument it reads as an anonymous token, such as a bare `==`, still counts as a word, and so does a lone `-` it drops before a redirection, as in `python3 - <<EOF`. Command words are unquoted, a `$'...'` string decodes its escapes as bash does, and a leading unquoted `~` or `~/` expands to the home directory. Each output redirection to a file (`>`, `>>`, `>|`, `>&`, `&>`, `&>>`) becomes an edit-class path target, and each input redirection (`<`) a read-class one, both resolved and zoned like a path tool's target. A redirection to `/dev/null`, `/dev/stdout`, `/dev/stderr`, or `/dev/tty` adds no target.

Each simple command retains two word lists. Its program words start at the program that runs, after all assignments and wrappers. Its named words also start at the program unless a leading assignment or an `env` argument sets a variable outside a short list of variables that affect only output, such as `RUST_LOG`, `LANG`, `TZ`, or `NO_COLOR`. Other variables can change which program runs or what it loads, as `PATH`, `LD_PRELOAD`, and `GIT_PAGER` can. In those cases, the named words start at the assignment and include all following words, including wrappers. Thus, `GIT_PAGER=sh git log` needs a rule that names the assignment and is never a known-safe read. A wrapper spelled with a directory, as in `./nice` or `/usr/bin/env`, may be any program, so the named words also start at it, as they do at a wrapper that changes who runs the command or its arguments. Thus, an allow rule for `rm *` covers `nice rm x` but not `sudo rm x`, which needs a rule that names `sudo`. Deny and ask rules still see the program that runs.

A command is opaque if the parse cannot show everything it runs or writes:

- It fails to parse, or tree-sitter-bash ends one of its words where bash does not, as at an escaped blank between words or a `$` before a blank, or reads a reserved word as a command name, as it does for `coproc`, which it has no grammar for, and for a compound command after the `time` keyword.
- It has words after a redirection of a compound statement, which bash rejects, or contains a command or process substitution, an arithmetic expansion, or a `[[` or `((` test, which can evaluate a substitution inside an array subscript.
- Its command name is not a literal word or holds a glob, as in `$CMD -rf x` or `/bin/s? -c ls`.
- It runs a shell such as `sh`, `bash`, or `zsh`, or a builtin that runs code (`eval`, `source`, `.`, `trap`, `let`), compared by folded name, so `BASH` counts too. Thus, both `sh -c '...'` and `curl ... | sh` are opaque.
- A wrapper uses an option the parser does not know or one that expands, such as `env -S` or `nice $LEVEL`. Deny and ask rules and the `rm` rule then treat each later word as where the program may start, so `sudo --unknown value rm -rf x` still counts as `rm -rf x`.
- A wrapper runs the command through a shell (`sudo -i`, `sudo -s`, `doas -s`), or `xargs` runs a wrapper alone, as in `xargs sudo`, whose command then comes from the input.
- A `find` argument expands, since it may become an action that runs a command.
- It runs `shopt`, which can turn on `dotglob`, `extglob`, or `globstar` and change what a glob matches.
- A redirection target is not a literal path or contains a glob.
- It assigns a variable the shell reads, such as `PATH`, `HOME`, `IFS`, or `BASH_ENV`, either with a plain assignment or in a `for` loop. It is also opaque if it declares or exports a variable outside the short list above, because later commands can see that variable.
- A `cd`, `pushd`, `popd`, `env -C`, `sudo -D`, `pkexec`, or `find -execdir` runs alongside a redirection or a reader of a relative path, which may then name another file.

The literal script in `sh -c` and the joined arguments to `eval` are parsed too, so deny and ask rules still see the simple commands inside them.

`su` and `runuser` start a shell, so they count as shells, and their `-c` or `--command` script is parsed the same way. `runuser -u` runs its command without a shell, so that command is parsed as a further simple command.

These wrappers, `find` actions, and literal scripts are the only places the analysis looks for a command inside another. Any other program that runs a command from its arguments, its input, or a file, such as `watch`, `fd -x`, `make`, `python3 -c`, or a `git` alias, is the program a rule sees, and variables set by `read` or `printf -v` stay unknown. In `manual`, such a command asks unless an allow rule names the runner. In `yolo`, it runs unless a deny or ask rule names the runner, since no rule sees the command inside it.

Some read-only commands run without an allow rule. `echo`, `false`, `pwd`, `true`, `cat`, `head`, `ls`, `tail`, `wc`, `find`, `grep`, and `sed` are safe when every argument is a literal word without globs, the command runs under its bare name, and nothing in the whole command expands a variable. That last condition covers a `case` subject, an assignment, a `for` list, a here-string, and an unquoted heredoc too, since any of them can bring a variable's value into what a safe command prints. The commands that can do more have these guards:

- `find` must not use an action that runs a command, deletes, or writes a file (`-exec`, `-execdir`, `-ok`, `-okdir`, `-delete`, `-fls`, `-fprint`, `-fprint0`, `-fprintf`).
- `grep` must not use a recursive option or `-d`.
- `sed` must not use `-i`, `-I`, or a script file. Its scripts may only print, delete, substitute, or move text between buffers.

GNU tools accept long options by any valid prefix, so `sed --in` is refused as `--in-place`.

`cat`, `find`, `grep`, `head`, `ls`, `sed`, `tail`, and `wc` are the readers, whether or not a call is safe, and match by their folded bare name, so `/bin/cat` and `CAT` count. Every argument of a reader that may name a file counts as a read of that path, including each word after the options and values attached to options. A globbing argument also counts as a read of each file it matches when the gate runs, expanded by bash's rules: `*`, `?`, and `[...]` skip names that start with `.` unless the pattern's name starts with `.` too, and a brace's alternatives are all tried, since bash writes them out before globbing. If a deny or ask `read` rule matches one of these reads, the command asks at step 3, even when an `exec` rule allows it, because its arguments may cover more files than it reads. An argument that expands a variable may name any file, as may the input words `xargs` or the paths `find` passes to a reader, so it asks whenever a deny or ask `read` rule exists, as the shipped `ssh` and `credentials` rules do. A glob that matches more than 1,024 files or a file whose name is not UTF-8 counts the same way. The `read` rules reach other programs only through input redirections, since a program can open any file once it runs.

## Patterns

[Permissions](../../guide/permissions.md#rules) gives the user-facing grammar with examples, and this section covers how matching works.

Each class has its own pattern grammar. Allow rules widen what can run without asking, so they match conservatively: a call matches only as it resolves, with names exactly as written in the pattern. Deny and ask rules hold calls back, so they match aggressively across every spelling a call can have. `MatchDiscipline` brings these two approaches together, and the ask tier uses the deny approach.

### Exec Patterns

An allow pattern matches a simple command's named words, including the assignments and wrappers that remain in it. A literal pattern word matches only a command word with that exact value, provided it neither expands nor globs. A `*` before the last word matches one such word, since an expanding or globbing word can become several. A trailing `*` matches all remaining words. Every simple command must match an allow pattern or be a known-safe read for the command to be allowed, so `cargo test *` never covers `cargo test && rm -rf /`.

A deny or ask pattern matches a simple command in any of three spellings: its named words, its program words, or its program words with the program's directory dropped. Its first word matches the command's first word, and each later literal word matches one of the command's later words, in order, with any words between them, so `git push *` holds `git -c a=b push origin` and a `*` adds nothing. Words compare unquoted, with both folded as protected names are. A command word that expands or globs matches any pattern word, since it may become that word when the command runs, so `git push *` also holds `git ${X:-push} origin`. Thus, `rm -rf *` stops `"rm" -rf x`, `/bin/rm -rf x`, `RM -RF x`, `FOO=1 nice rm -rf x`, and `cd src && rm -rf x`.

### Path Patterns

A `read` or `edit` pattern without a leading `/` or `~/` is anchored at each anchor directory, the cwd and each workspace root.

An allow pattern matches the resolved path. If anchored, it matches the path beneath each anchor directory as that directory resolves. Deny and ask patterns also match the path as spelled in the call, after lexical normalization, beneath each anchor directory in both its resolved and spelled forms. This prevents `.env -> ../shared/.env` from routing a read around `deny.read = [".env"]`. They also compare names as protected paths do, so `.env` covers `.ENV` and `.env.`. When the policy loads, a deny or ask pattern whose literal prefix, the components before its first glob character, passes through a symlink also matches the prefix's resolved location. For anchored patterns, this is checked from each anchor directory. For example, with `secrets -> ../vault`, `secrets/**` also covers `../vault/key`.

## Shipped Defaults

oxide-code ships named defaults, each a group of rules in one tier, which the [permissions guide](../../guide/permissions.md#built-in-rules) lists. A user config turns a group off with `false` under `[permission.defaults]`.

`git`, `direnv`, `editors`, `cargo`, `shell`, `agents`, and `ox` define the protected names of the [Protected zone](#path-targets). `git`, `ssh`, and `cloud` also protect locations in the home directory: the user's git config and credential stores, all of `~/.ssh`, where `authorized_keys`, `authorized_keys2`, `rc`, and `environment` decide what a login runs, and the config directories of the AWS, Azure, Google Cloud, Docker, and Kubernetes CLIs, whose credential helpers and exec plugins run commands. Each covers files that can run or configure code outside the task at hand, so edits to them ask even when an allow rule or the workspace would otherwise permit the edit. The rest of `.claude`, including `worktrees` and `plans`, remains editable. Like any file, a [spill file](truncation.md#spill-files) holding oversized tool output opens for `read` and `grep` at step 4. Editing one asks through `ox` only when the spill lies in the data directory, which excludes spills beside a session file resumed from elsewhere. `devices`, `ssh`, and `credentials` add patterns that match like user rules in the same tier. On Unix, `credentials` also denies `/proc/*/environ` and `/proc/*/task/*/environ`, which on Linux hold each process's environment, including any credential `ox` reads from its own, and which `/proc/self` and `/proc/thread-self` paths resolve to. The rest of `/proc`, such as `cpuinfo` or a process's `status`, stays readable, since diagnosing a build or a hung process needs it and it holds no environment. `rm` checks the arguments before `--` of each program a simple command may run, including one a wrapper or `find` action runs, for both a recursive option, `r` or `R` in a short option cluster or a prefix of `--recursive`, and a force option, `f` in a cluster or a prefix of `--force`, so `rm -rfv`, `rm -r --force`, and `rm -f -R` all count. A word that expands, the program name included, counts as either, and GNU `rm` reads options after its operands too, so `rm / -rf` counts. No default joins the allow tier, so an allow rule never overrides one, and `true` in a later layer turns a default back on. A `read` deny default also withholds its files from `grep` and `glob` walks.

## Approval Round-Trip

When the gate asks, the decision travels through the `ops` channel the turn loop polls. `run_tool_round` gates calls one at a time, so at most one approval can be outstanding and no ID fan-out is needed. Consecutive calls whose `ToolSpec` marks them parallel-safe, namely the read-only `read`, `grep`, and `glob` tools, run together once each call passes the gate. This way, an approval never waits on a running call. If a batch is interrupted, results from finished calls are kept and each unfinished call's `ToolCtx` token is cancelled, stopping blocking `grep` and `glob` walks. The TUI places each result under its call. The REPL repeats a call's header above its result when another call's result was printed in between, so batches read the same in live and resumed sessions.

For a call that asks, the dispatcher emits `Event::ApprovalRequested { id, preview, basis, grant }` with the ID, the basis that made the call ask, the session grant described below, and a small `Clone` preview. The preview is an edit diff from `edit::synthesize_chunk`, a diff against the file's current content for `write`, or the command string under the tool name for `bash`, titled `Bash (background)` for a background call. The gate intercepts the call before it runs, just as the parse-error short-circuit returns a synthetic `ToolOutput`. It waits for a decision in `await_approval`, which hands each op to `Inbox::absorb` as `await_unless_aborted` does. `absorb` maps `Op::Interrupt` to `Cancelled` and `Op::Shutdown` to `Shutdown`, and keeps `Op::Queue` and `Op::SetMode` in the turn's `Inbox`. The wait matches `Op::Resolve { id, decision }` and ignores decisions whose IDs do not match the outstanding call, and dropping the wait future is cancel-safe.

Every frontend responds with one of the `Decision` variants:

- **`Approve`** runs the call.
- **`AllowForSession`** runs the call and adds a session grant, described below.
- **`Deny`** returns a synthetic error `ToolOutput` stating that the user denied the call. The model receives the refusal as a tool result and can choose another approach.
- **`DenyWithFeedback`** returns the user's text in that error result, so the model learns why the call was denied and what to do next.
- **`Abort`** ends the turn as `Op::Interrupt` does. The call's result records that it never ran.

In the TUI, an `ApprovalModal` joins the `ModalStack`. The blocked agent must receive a decision whenever the modal is dismissed. However, `ModalStack::handle_key` intercepts Esc and Ctrl+C before delegating, while `clear` drops modals outright. Both paths produce `ModalAction::Cancelled`. The stack therefore has a cancel hook, `Modal::on_cancel`, that returns an optional `ModalAction`. On a universal cancel or session-swap `clear`, it resolves a pending approval to `Decision::Abort`, matching Esc outside the modal.

`Y` approves, `N` denies, and Esc aborts. `F` opens a one-line feedback editor. Enter denies with the typed text, an empty answer submits nothing, and Esc returns to the choice. Enter does nothing while the choice is shown. The App drops key releases, so a held key reaches the modal as repeated presses. Once the guard below expires, a repeat counts as a fresh press. As in every modal, a key held with Ctrl or Alt does nothing outside the editors once the guard expires. Apart from the stack-level Esc / Ctrl+C cancel, it ignores every key until 300 ms have passed without one. Each key during that window restarts the timer, preventing a keystroke typed for the prompt underneath or a pasted burst from approving the call. Opening an editor restarts the timer, so an Enter typed right after `A` or `F` cannot submit it. `Y` also waits until a render has shown the first body row, so a modal clipped by a short terminal cannot approve a call the user has not seen.

`A` allows the call and every later call covered by a session grant:

- **Grant:** When the gate asks, `Policy::session_grant` derives the narrowest rule that names the call with `Rule::for_call`. For `bash`, it uses the command's single simple command as its named words, quoting each word when required by the pattern grammar. Thus, `RUST_LOG=debug cargo test 2>&1` offers `cargo test`. For an edit, the grant uses the escaped path, prefixed with `./` inside the cwd or written as an absolute path outside it. The grant travels on `Event::ApprovalRequested` and is offered only if adding its rule would allow the call. Protected paths, ask-rule matches, opaque commands, and commands with multiple simple commands offer no grant. Calls with words that expand or glob, and calls containing characters the editor cannot display as themselves, offer no grant either. This ensures the user always sees the exact rule they are granting.
- **Editor:** `A` opens a one-line editor prefilled with the rule's pattern. The user can widen or narrow it, for example to `cargo test *`. Enter checks the edit with `SessionGrant::accept`, which parses the pattern in the call's class and requires it to still cover the call. If it does not, the editor shows the reason inline. Esc returns to the choices through `Modal::captures_esc`, while Ctrl+C still aborts.
- **Engine check:** The accepted rule answers `Decision::AllowForSession`. Since a frontend's check is advisory, the gate keeps its own grant for the pending call, admits the answered rule against it with `SessionGrant::admit`, and denies the call with an error if the rule fails.
- **Precedence and lifetime:** An admitted rule joins the engine's policy, where step 6 treats it like a configured allow rule. Deny rules, ask rules, and protected paths still take precedence. The list lives only in the engine's memory and is emptied by `/clear`, `/archive`, and a successful `/resume`. `Event::SessionRulesUpdated` carries it to the frontend, and `/status` lists it under Session Rules by class.

The preview must show everything it approves. Every row wraps. If the body exceeds 16 rows, it scrolls with Up / Down, PageUp / PageDown, and Home / End, and shows a `rows a-b of n` position line. Control and zero-width characters render as `\u{..}` escapes. In a command, every whitespace character after the indentation renders as `·` unless it is a single space. Thus, padding that pushes a chained command off-screen appears as a dotted run.

## Configuration

```toml
[permission]
mode = "manual"
extra_roots = ["~/src/shared"]

[permission.allow]
exec = ["cargo test *"]

[permission.defaults]
rm = false
```

The `[permission]` table parses into a `PermissionTable` on `FileConfig`. `deny_unknown_fields` applies at every level, so an unknown class or default name fails to load and lists the accepted names. Every `permission.<tier>.<class>` list and `permission.extra_roots` entry is concatenated across config layers, while a `permission.defaults` key set in a higher layer replaces the lower one. `Config::load` resolves the policy after the compaction block, including each deny and ask pattern's literal prefix. It also resolves the extra roots with `Workspace::resolve`, failing to load if a root is relative, does not resolve, or is `/`, `$HOME`, or an ancestor of `$HOME`. A root that does not resolve runs through a symlink loop, a directory the walk cannot search, or a file that more components follow, and the error names that component.

A checked-in `.ox/config.toml` is untrusted, so `reject_project_keys` checks it against allowlists: it may set only the `[client]`, `[permission]`, and `[tui]` tables, only request settings in `[client]`, and only `permission.deny` and `permission.ask` in `[permission]`. Any other `[permission]` key there, such as `mode`, `allow`, `extra_roots`, or `defaults`, is rejected with a message pointing to user config. The merge appends project rules to the user's rules, allowing a repo to restrict itself or require approval without widening what the user allowed or turning off a default.

## Stdio Frontends

The `--no-tui` REPL and `-p` headless runs share `render_op`. It passes each `Event::ApprovalRequested` to an `Approver` and sends the decision back as `Op::Resolve`. The request itself never reaches the event sink, so `stream-json` output contains only the call's result.

- **REPL:** When stdin is a terminal, the prompter writes the preview and the rule that holds the call to stderr, escaping hidden characters, and reads the answer from the next stdin line: `y`, `a` followed by the pattern to allow, `n`, `f` followed by the feedback line, or `x` to abort. An unknown answer, empty feedback, or a pattern the grant does not admit returns to the choices. End of input aborts, and Ctrl+C interrupts the turn, which drops the question. Piped input answers the way it does in a headless run, so a scripted prompt line is never read as an answer.
- **Headless:** When nobody is available to ask, rules and zones decide every call, so a headless run assumes an already-trusted invocation. `Unattended` denies calls that would ask and provides feedback naming the pattern to add and the class list under `[permission.allow]` that admits it, quoted as a TOML string from the call's grant. A call that an ask rule or a built-in rule holds is denied, and the feedback names that rule. For a built-in ask rule, it also names the `[permission.defaults]` key that turns it off. A reader command that would open a file guarded by the `ssh` or `credentials` deny group also prompts for approval. Its feedback names the group without suggesting that the user disable it. Other calls with no grant, such as those running more than one simple command or containing a word that expands, are denied with feedback saying that no allow pattern matches this call. A broader pattern may still cover such a call. A command that failed to parse is denied with feedback asking for simpler shell syntax.

The stdio sink escapes control characters other than tabs and line breaks in model and tool text, preventing streamed text or a tool label from redrawing the terminal to disguise the REPL's preview.

## Permission Classes

`Tool::spec` has no default, so every tool declares its class in its `ToolSpec` and a new tool cannot inherit a permissive class. Read-class tools are allowed at step 4, but they still resolve a target, such as the file for `read` or the search root for `grep` and `glob`, so a path-scoped deny or ask rule can match first. Rules target the class, so `deny.read = [".env"]` also stops a `grep` rooted at a `.env` file. Since `grep` and `glob` gate on the search root, they also check every file found during a walk against the `read` deny and ask rules, withholding matches and ending their output with a count of withheld files but no names. A path the walk could not read faces the same check, so the note listing unreadable paths never names a withheld one. A walk cannot ask about each file. A file is checked where it resolves and as spelled in the call beneath the search root, just as the gate checks a read of it. Thus, `.env` is also withheld under a root that is a symlink out of the workspace. Walks do not follow symlinks below the canonical root, so the check needs no I/O.

`edit` and `write` share a class but have different blast radii. `edit` requires an existing file that has been read, so it cannot create files. `write` can create new files and parent directories without a prior Read, while overwriting an existing file still passes through the tracker gate. Both declare `file_path` as their path field, so the resolution, zoning, and pinning described above apply to them identically.

## Design Decisions

1. **Tiers hold in every mode.** Deny and ask rules apply whatever the mode, and the mode mostly decides what the rules leave open. A `yolo` that skipped rules would make a deny rule's meaning depend on a setting elsewhere, so `yolo` only changes the fallback from ask to run, as Claude Code's bypass mode keeps deny rules, ask rules, and protected paths. `plan` denies `edit`, `exec`, and `net` calls ahead of the ask tier because approving one would break the read-only session.

2. **Ask outranks allow.** An ask rule records a call the user wants to see, such as `git push`, even when a broad allow covers it. Placing the ask tier above every allow, including session grants, the workspace allow, and a future classifier, preserves that request as the allows grow.

3. **Protected paths ask.** Editing `.git/hooks`, `.vscode/tasks.json`, or `.envrc` inside the workspace can escalate to code execution outside the task. These paths therefore ask even under `allow.edit = ["*"]`. Asking keeps legitimate edits possible, while a deny would require the user to edit config. A user who never wants to be asked about one group can turn it off.

4. **Words over strings.** Matching `exec` patterns against parsed words gives a rule the same meaning regardless of how the command quotes, spaces, or wraps its words. It also lets an allow pattern reject a word that could expand into several. A lone `*` is the only wildcard, so rules do not depend on how a glob inside a word interacts with shell quoting.

5. **Gitignore-style paths.** Patterns work like `.gitignore` entries, so `.env` and `target` need no `**/` prefix and cover what is beneath them. Anchoring at every anchor directory applies a project rule to each workspace root, while `/` and `~/` keep exact absolute rules available.

6. **Resolve once, act on the result.** Resolving in `prepare` and pinning the input makes the tool open the path the gate checked. A no-follow open makes a symlink swapped into the final component cause the open to fail. A swap in an intermediate directory needs `openat`-style traversal or an OS sandbox ([#133](https://github.com/hakula139/oxide-code/issues/133)).

7. **Project files tighten only.** Honoring a project's `allow`, `mode`, `extra_roots`, or `defaults` would let a teammate's repo broaden what the local user permitted, creating the same privilege-escalation vector as project-set credentials. Appending `deny` and `ask` rules lets a repo restrict itself without that risk.

8. **Deny rules are the enforced boundary for path tools.** The deny list matches both spellings of the path as the file system sees it when the gate checks, each pattern's literal prefix as resolved when the policy loaded, and folded names. It does not cover a symlink swapped into an intermediate directory after the check or files touched by a `bash` command, so an untrusted model still needs an external OS sandbox.

9. **Decision on the op channel.** Routing approvals through `ops` and the `ModalStack` reuses the turn loop's cancel / quit / queue semantics, so approvals need no second channel or control-flow path.

10. **Every decision names its basis.** Carrying the basis with the decision lets the denial, the approval prompt, the headless feedback, and the debug log all name the same rule, so the model and the user can see which rule fired, and the user knows which pattern to change.

11. **Mode switches wait for the next gated call.** A switch mid-turn changes how the remaining calls are weighed without revisiting calls that already passed the gate, which would mean cancelling a running command or a pending approval. Confirming on receipt keeps the status line honest about the mode the next call meets.

12. **Shift+Tab reaches `yolo` only from a `yolo` start.** A stray key press then cannot leave every call unprompted in a session that started with prompts. Claude Code likewise leaves `bypassPermissions` out of its Shift+Tab cycle unless the session started with it allowed, while `/mode yolo` stays available as a deliberate, typed choice.

13. **Approvals stay in process.** A persisted grant has no disk ground truth to revalidate on resume, unlike a `FileSnapshot` rehash, so restoring it would regress trust. In keeping with the slash-command persistence stance ([decision 6](../slash/commands.md#design-decisions)), session commands remain reversible, and cross-session writes wait for an explicit confirmed action.

14. **Rules follow command runners, and allow rules see through only the harmless ones.** `yolo` runs whatever no rule holds, so a deny rule that `sudo rm`, `xargs rm`, or `find -exec rm` could step around would hold only in `manual`. Deny and ask rules therefore see the command each followed wrapper or `find` action runs, and after an option the parser does not know, every word that may start it. Allow rules see through only the wrappers that change how a command runs, since `sudo` and `xargs` change who runs it and with which arguments, so allowing `rm *` must not allow either. Claude Code strips only `nice`, `nohup`, `stdbuf`, `time`, and `timeout` for its rules and matches `xargs` only without options, relying on asking for the rest, while Codex looks through `sudo` and `env` only for its forced-`rm` check.

15. **A command that fails to parse asks, even in `yolo`.** tree-sitter-bash's error recovery can fold one command's words into another's or into a redirection, so the words rules see may not be the words bash runs, and a deny rule could miss a command `yolo` would run. The grammar can also end a word where bash does not without a parse error, as at an escaped blank between words, a line continuation that bash removes to join two tokens, or a `$` it reads together with the blank after it. Such a command asks too: outside quotes, the analysis accepts only blanks and line continuations between tokens, backslash escapes alone only between the parts of one command word, line continuations alone only after a control operator, and no blank inside a `$` expansion. So does a command whose name, or whose first word after the `time` keyword and its options, is a reserved word, which happens only where the grammar has no rule for the construct. Asking keeps deny rules the boundary in `yolo` at the cost of an occasional prompt for valid syntax the grammar does not support, which rewriting the command avoids. A deny rule that matches what the parser did see still denies first. Claude Code also asks when its parser fails, but its bypass mode overrides that ask, and Codex only withholds known-safe status from such a command, so a mode that never asks still runs it.

## Sources

- `crates/ox-core/src/config.rs`: `Config::load`, `resolve_permission`, `OX_PERMISSION_MODE`.
- `crates/ox-core/src/config/load.rs`: `reject_project_keys` and the project allowlists.
- `crates/ox-core/src/config/merge.rs`: `APPENDED_TABLES` and `APPENDED` lists, `merge`.
- `crates/ox-core/src/config/schema.rs`: `PermissionTable`, `TierTable`, the `defaults` map.
- `crates/ox-core/src/engine.rs`: the live policy, `Engine::set_mode`, and `Engine::adopt_mode`.
- `crates/ox-core/src/engine/abort.rs`: `await_unless_aborted`, which races an allowed call against cancel and quit, and the `Inbox` whose `absorb` keeps queued prompts and mode switches.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round` (parallel-safe batches), `Dispatcher::gate` (prepare, then decide), `check_permission` (take a waiting mode switch, then log the verdict), `allow_for_session` (admit against the call's grant), `await_approval`, `denied_output`.
- `crates/ox-core/src/engine/sessions.rs`: `adopt_session`, which resets the session grants when the session changes.
- `crates/ox-core/src/permission.rs`: `Mode`, `PermissionClass`, `Workspace`, `Access::for_path`, `Access::for_command`, `Verdict`, `Basis`, `Policy::verdict`, `Policy::session_grant`, `SessionGrant`, `ReadFilter`.
- `crates/ox-core/src/permission/defaults.rs`: the shipped defaults, `ShippedTier`, protected names and home locations, and `[permission.defaults]` names.
- `crates/ox-core/src/permission/rule.rs`: `Rule`, the exec, path, net, and MCP patterns, `MatchDiscipline`, and `Rule::for_call`.
- `crates/ox-core/src/permission/shell.rs`: `CommandTarget::parse`, wrapper grammars and stripping, `find` actions, named and program words, opaque forms, redirection reads and writes, known-safe commands, reader glob expansion, and the forced recursive `rm` check.
- `crates/ox-core/src/protocol.rs`: `Event::ApprovalRequested`, `Event::SessionRulesUpdated`, `Event::ModeChanged`, `Op::Resolve`, `Op::SetMode`, `Decision`.
- `crates/ox-core/src/tool.rs`: `Tool` trait (`spec`, `path_field`, `prepare`, `approval_preview`, `run`), `ToolSpec`, `ToolCtx`, `Prepared`.
- `crates/ox-core/src/tool/bash.rs`: `BashTool::prepare` (the command target) and `BashTool::approval_preview` (the `Bash (background)` title).
- `crates/ox-core/src/tool/edit.rs`: read-before-edit gate, `synthesize_chunk` preview.
- `crates/ox-core/src/tool/file.rs`: `read_validated`, `write_no_follow`, `open_no_follow`.
- `crates/ox-core/src/tool/glob.rs`: read filter on each match.
- `crates/ox-core/src/tool/grep.rs`: read filter in `collect_files`.
- `crates/ox-core/src/tool/search.rs`: `SearchRoot`, `push_withheld_note`.
- `crates/ox-core/src/tool/view.rs`: `ApprovalPreview`.
- `crates/ox-core/src/tool/write.rs`: new-file creation, symlink refusal, current-content preview.
- `crates/ox-core/src/util/text.rs`: `is_hidden`, shared by both frontends, plus the stdio sink's `escape_controls` and the REPL preview's `escape_hidden`.
- `crates/ox-tui/src/app/dispatch.rs`: `App::cycle_mode`, the Shift+Tab cycle.
- `crates/ox-tui/src/components/chat.rs`: `ChatView::push_tool_result`, which places a result under its call.
- `crates/ox-tui/src/modal.rs`: `ModalStack`, the cancel hook, `captures_esc`.
- `crates/ox-tui/src/modal/approval.rs`: `ApprovalModal` wrapping, scrolling, character reveal, and the rule and feedback editors.
- `crates/ox-tui/src/slash/mode.rs`: `ModeCmd` and the `/mode` picker.
- `crates/ox-tui/src/slash/status.rs`: the Session Rules listing.
- `crates/ox/src/driver/repl.rs`: `Prompter`, the stdin approval prompt.
- `crates/ox/src/driver/stdio.rs`: `render_op`, the `Approver` trait, `Unattended`, the escaped `StdioSink` output and its repeated call headers.
- `crates/ox/src/main.rs`: the `--mode` flag.
