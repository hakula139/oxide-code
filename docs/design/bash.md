# Shell Execution

The `bash` tool runs each command as `bash -c <command>` in the session's working directory, with empty stdin and with stdout and stderr sharing one pipe back into the result, which holds their output in the order their writes reach the pipe. This page describes the process that runs the command, the environment it sees, and the [background shells](#background-shells) that outlive their call, while [Output Truncation](truncation.md#spill-files) explains how the output is capped and spilled.

## Process Tree

Each command runs in a `ShellTree` containing the shell and all its descendants, so a timeout, cancelled call, or stopped background shell kills the entire tree.

- **Unix.** The shell leads a new session and process group (`setsid`). A timeout or cancel sends `SIGKILL` to the whole group. On kernels other than Linux, such as XNU, the kill repeats for up to 1 s until no member it can signal is alive, because a child forked during `killpg` can miss the signal there.
- **Windows.** The shell runs in a Job Object, whose termination ends every process in it. Git for Windows' `bash.exe` runs the command, and the call fails if Git Bash is missing. An idle Git Bash that ox keeps running sets up the [MSYS runtime](#msys-runtime) before any shell starts, so killing a shell leaves the runtime intact while that bash runs.

Because the session has no controlling terminal, a command that prompts on `/dev/tty`, such as `sudo`, `ssh` with password authentication, or `git push` over HTTPS without a credential helper, fails immediately with an error the model can read. In ox's own session, the prompt would print over the TUI or REPL, and the read would stop the process with `SIGTTIN` until a timeout or `bash_stop` killed it.

### MSYS Runtime

Every MSYS process, such as Git Bash's `bash.exe` and the tools under its `usr\bin`, maps a per-user shared memory region as it starts, and the region lasts while any MSYS process maps it. The first process to map it builds the runtime's mount table under a lock that it releases when it finishes. If it dies first, as a shell killed within milliseconds of its start does, the lock stays taken. Every MSYS process started after that, including the user's own Git Bash windows and the `sh` that git runs aliases and credential helpers through, waits 15 s for the lock and then aborts with `add_item (...) failed, errno 1`, until no MSYS process maps the region. [git-for-windows/git#6368](https://github.com/git-for-windows/git/issues/6368) tracks the runtime bug, and [msys2/msys2-runtime#333](https://github.com/msys2/msys2-runtime/pull/333) proposes a fix.

A process that starts once the region is set up skips the lock, so a shell killed at any point after that leaves the runtime intact. ox therefore keeps one idle bash running `echo; read`, without the [credentials](#credentials), since other MSYS processes can read its environment. Before each shell starts, ox checks that this bash is still running and otherwise starts a new one, then waits up to 15 s, as long as MSYS waits for the lock, for the line it prints once its runtime is up. The wait is asynchronous and comes before a background shell takes its slot, so a cancel ends it and other tools never wait behind it. A bash still starting when the wait ends keeps running for the next shell to wait on, while a shell started after its holder failed runs unprotected. The holding bash reads a stdin that ox never writes to, so it exits when ox exits and the pipe closes. The region serves every integrity level of the user, so the holder of an elevated ox also sets it up for the user's non-elevated Git Bash.

## Environment

The command inherits ox's environment minus the [credentials](#credentials) ox authenticates with, `CDPATH`, and `OLDPWD`, and it adds these variables:

| Variable                         | Value            | Purpose                                                                                          |
| -------------------------------- | ---------------- | ------------------------------------------------------------------------------------------------ |
| `OX_SESSION_ID`                  | The session's ID | Lets a script tell which session started it                                                      |
| `EDITOR`, `GIT_EDITOR`, `VISUAL` | `true`           | Makes a command that opens an editor keep its default text, so nothing waits on an editor window |

With the editor set to `true`, `git rebase --continue` and `git cherry-pick --continue` keep the prepared message, while `git commit` without `-m` stops with `Aborting commit due to empty commit message`. `GIT_EDITOR` takes precedence over git's `core.editor`, which takes precedence over `VISUAL` and `EDITOR`, so all three are needed for git and for tools that fall back to the generic pair, such as `gh` and `crontab -e`.

An exported `CDPATH` makes `cd sub` try each of its entries before the working directory, while the [permission gate](permissions.md#command-targets) resolves `sub` against the working directory alone. Dropping it keeps `cd` where the gate placed it, so an allowed `cd sub && make` cannot run `make` in another `sub`. An inherited `OLDPWD` names a directory the gate never sees for `~-`, so dropping it leaves `~-` as written until the command's own first `cd`, `pushd`, or `popd`.

### Credentials

Every command runs without `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, and `ANTHROPIC_CUSTOM_HEADERS`, which carries gateway keys. `secret_env_vars` derives the list from the order in which credentials are resolved, so ox withholds each new credential variable as soon as it reads it. ox reads these variables from its own environment, which this does not change.

A [known-safe command](permissions.md#command-targets) never expands a variable, but any command the user approves or an allow rule admits can, such as a build script run by an allowed `cargo test`, and its output reaches the session store and the model's context. Withholding these variables keeps ox's own credentials off every such path. A script that needs its own Anthropic key, such as a test suite that calls the API, has to read it from another variable or a file. Other secrets in ox's environment, such as cloud or GitHub tokens, still reach every command.

## Background Shells

The `bash` tool can start a command in a background shell that keeps running after the call returns. Use `bash_output` to read its output and `bash_stop` to end it. Background shells suit dev servers, watchers, and long builds that the model checks while it continues working. They end with the session.

- **Starting.** Calling `bash` with `run_in_background: true` starts the command and returns immediately with an ID such as `shell-1`. IDs count up while `ox` runs, so they are not reused within a run. A session resumed in a new `ox` process starts again at `shell-1`. The call's `timeout` does not apply. It uses the `exec` permission class, like a foreground call, and its approval prompt is titled `Bash (background)`. The tool description tells the model that it is not notified when the command finishes and to set the flag instead of adding a trailing `&`.
- **Reading.** `bash_output` returns the output printed since the previous read, or `(no new output)`, followed by `(still running)`, `(exit code N)`, or `(killed by signal N)`. Each read has the same output limit as a foreground call: it retains the head and tail of the output and saves output it cuts in full to a [spill file](truncation.md#spill-files) numbered by the read. An optional `timeout` in milliseconds, capped at 600000, waits for the command to finish before returning. The wait ends when the command exits or the timeout passes, so for a server it lasts the full timeout. An unknown ID returns an error listing the open shells and their states.
- **Stopping.** `bash_stop` ends the shell and its process tree, removes it from the list, and returns any unread output followed by `(stopped)`. If the shell had already finished, the result ends with `(stopped after it exited with code N)` or `(stopped after signal N killed it)`.
- **Limit.** A shell remains open, whether running or finished, until `bash_stop` stops it or the session ends. This keeps a finished shell's last output readable. A session can hold at most 16 open shells. If that limit is reached, `bash` refuses to start another and returns an error listing the open shells.
- **Permission.** `bash_output` and `bash_stop` belong to the `read` class and run without approval in every mode. They act only on shells started by an allowed `bash` call. A class-wide `read` deny blocks them, but `bash` can still start background shells. Those shells run unread until the session ends. Neither tool joins a parallel batch: reading empties the unread output, while stopping ends the shell another call may be reading.

### Shell Lifecycle

Each shell runs under a supervisor task that owns its `ShellTree`, the [process tree](#process-tree) that also holds a foreground call. The supervisor copies the pipe that stdout and stderr share into the shell's capture as output arrives, then publishes the exit state when the shell exits.

A stop kills the whole tree while the shell is running. It also kills the tree after the shell exits if a descendant still holds its pipe, as with a server started with a trailing `&`. Once the shell has exited and its pipe has closed, the supervisor releases the tree, as it does for a foreground call. A descendant that redirected its output elsewhere then keeps running. `bash_stop` returns after the supervisor has killed or released the tree and drained the remaining output.

The engine ends every open shell and waits for each tree to be gone when it rolls to a new session on `/clear` or `/archive`, resumes another session, or shuts down. A failed resume leaves the shells running because the session stays active. Pressing Esc to interrupt a turn leaves background shells running because only the turn's own work stops.

### Compaction

Shells stay open through `/compact` and auto-compaction. Because the `bash` calls that started them are gone, a `<system-reminder>` block in the synthetic summary message lists each open shell's ID, state, and command's first line. It follows the task list's block and precedes the reminder to resume the task that a mid-turn compaction adds. The reminder is part of the persisted summary, so it remains visible when the session is resumed, even after the shells have ended. `bash_output` then reports that the ID is not open.

### Frontends

Whenever a shell starts, exits, or stops, the engine emits `Event::BackgroundShellsUpdated` with every open shell. Each entry includes the shell's ID, command, and state. Ending shells with the session emits no event, so frontends clear their copy on `SessionRolled` and `SessionResumed`, just as they do the task list.

- **TUI.** `/status` lists each shell under a Background Shells heading, along with its state and command's first line.
- **REPL.** `--no-tui` ignores the event, and the model receives output through `bash_output` results.
- **Headless.** `stream-json` output includes the `background_shells_updated` event, as described in the [headless guide](../guide/headless.md#events).

## Design Decisions

1. **Detach from the terminal with a new session.** A child in a new process group still shares ox's controlling terminal, so a `/dev/tty` prompt appears on the user's screen with no way for the model to answer it. Codex detaches its shell tool the same way (`detach_from_tty` in `codex-rs/utils/pty/src/process_group.rs`), while Claude Code and pi spawn bash with Node's `detached: true`, which also calls `setsid`. A session leader also leads its process group, so the group kill still reaches every descendant.
2. **Set the editors and leave pagers alone.** Pagers such as `less` and git's pager start only when stdout is a terminal. Here stdout is always a pipe, so setting a pager variable would have no effect. An editor starts regardless of stdout, and a GUI editor opens on the user's screen. Claude Code sets only `GIT_EDITOR=true`, leaving tools that read `VISUAL` or `EDITOR` uncovered. Codex sets `PAGER` and `GIT_PAGER` to `cat` because its unified exec runs commands in a pseudo-terminal. oh-my-pi sets those variables, the editors, and about 40 other non-interactive defaults, such as `CI=true` and `TERM=dumb`. Those defaults change how builds and tests behave, so ox sets only the variables that keep a command from waiting for input that cannot arrive.
3. **Leave credential prompts to the missing terminal.** Without a controlling terminal, git's and ssh's terminal prompts fail on their own, so `GIT_TERMINAL_PROMPT=0` would add nothing. A GUI credential helper, such as Git Credential Manager, still opens because the user can answer its prompt.
4. **Withhold ox's own credentials by default.** ox needs its credentials only in its own process, and its permission gate cannot prevent an allowed command from reading the environment. Claude Code strips the same variables, along with cloud and CI tokens, only when `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` is set. Its GitHub Action sets this flag for workflows that untrusted users can trigger. Codex withholds only the tokens it creates for itself, oh-my-pi strips provider keys from its Python kernel but not from bash, and pi passes everything through. Making this depend on a flag would leave the interactive case exposed because prompt injection can reach the model just as easily. ox leaves other secrets alone because it cannot tell which ones a build or deploy command needs.
5. **Hold the MSYS runtime from ox.** A timeout, cancelled call, `bash_stop`, `/clear`, resume, or exit can kill a shell that is still starting, so the broken runtime reaches users as well as the test suite. Retrying a shell that aborted cannot recover, since each retry waits out the same lock and keeps the broken region mapped. Waiting until each shell has started before it may be killed would need a signal that its runtime is up, and would delay every cancel. Claude Code, in the frozen 2.1.87 source, and pi end a Git Bash tree with `taskkill /T /F` and leave the runtime unguarded, and Claude Code users report this crash in [anthropics/claude-code#30165](https://github.com/anthropics/claude-code/issues/30165). Codex runs PowerShell on Windows, and oh-my-pi interprets commands in its embedded brush shell, though the Git for Windows tools it puts on `PATH` still run as MSYS processes. The holding bash costs one idle process, and its startup, which takes tens of milliseconds, delays the first shell and the first after a holder exits.
6. **Share one pipe between stdout and stderr.** The output holds the command's writes in the order they reach the pipe, so a test runner's panic on stderr appears beside the result line printed before it. Claude Code passes one file descriptor as both streams, and pi feeds both of its pipes into one `onData` callback in arrival order, so neither labels a line's stream either. The merge has two costs. A program that buffers stdout when it is not a terminal flushes it late, so a Python `print` followed by an uncaught exception shows the traceback first, where separate captures happened to show the true order. A short stderr also loses the share of the budget it had, so an error printed amid more than 128 KiB of stdout can fall in the omitted middle, where only the spill file holds it. ox leaves `PYTHONUNBUFFERED` unset although oh-my-pi sets it, since defaults that change how builds and tests behave stay unset (**Set the editors and leave pagers alone**). Labeling each chunk would need two pipes again and still could not recover the order of writes buffered inside the command.
7. **Read incrementally from memory.** Each `bash_output` read returns what has arrived since the previous read, so the model sees new lines without rereading old ones. Claude Code writes background output to a file and shows the model its tail. A model that reads before about 128 KiB piles up sees the whole stream, and a read that cuts its window keeps only that window on disk.
8. **Report an exit on the next read.** Claude Code adds a `<task-notification>` message when a background task finishes. Here, a shell never starts a turn or adds a message by itself, and the tool text says so because a model used to Claude Code may wait for a notice. The model learns that the shell exited on its next `bash_output` read, whose `timeout` wait can cover a build.
9. **Keep finished shells open until stopped.** Closing a shell on exit would lose output the model has not yet read. The cap bounds what the session holds because each shell keeps at most one head-and-tail capture. Codex sets a soft cap of 64 processes, and oh-my-pi caps running jobs at 15.
10. **End shells with the session.** A dev server that outlived `ox` would keep its port open with nothing left to stop it. Codex also terminates its processes at teardown. The engine waits for each tree to end, so even a signal exit, which does not wait for spawned tasks, still kills them.
11. **Release a finished tree whose pipe is closed.** Once the shell has exited and nothing holds its pipe, oxide-code can no longer tell when the process group empties. Killing the group later could hit an unrelated group that reused its ID. Releasing it matches a foreground call, where a redirected server also keeps running.
12. **Skip approval for reads and stops.** Each `bash_output` or `bash_stop` call acts on a shell already started by an allowed `bash` call, so asking again would repeat that approval.

## Sources

- `crates/ox-core/src/config/credentials.rs`: `secret_env_vars`, derived from `EXPLICIT` and `CUSTOM_HEADERS_VAR`, and `withhold_secrets`, which removes them from a child's environment.
- `crates/ox-core/src/engine.rs`: `Engine::shells`, which `Engine::new` shares with the three tools, attached to the event channel in `spawn` and ended in `finalize`.
- `crates/ox-core/src/engine/compaction.rs`: `Engine::replace_with_summary`, which lists the open shells in the summary message.
- `crates/ox-core/src/engine/post_compact.rs`: `shell_reminder`, the block listing open shells after the summary.
- `crates/ox-core/src/engine/sessions.rs`: `adopt_session`, which ends the old session's shells.
- `crates/ox-core/src/protocol.rs`: `Event::BackgroundShellsUpdated`.
- `crates/ox-core/src/tool.rs`: `ToolRegistry::built_in`, which hands one `BackgroundShells` to the three tools.
- `crates/ox-core/src/tool/bash.rs`: `ShellTree`, `SESSION_ID_VAR`, `EDITOR_VARS`, `DIRECTORY_VARS`, `run_in_background`, `PipeCapture`, `render_output`, `bash`, which finds Git Bash on Windows, and `Bash::ready`, which waits for the MSYS runtime before a shell starts.
- `crates/ox-core/src/tool/bash/background.rs`: `BackgroundShells`, `supervise`, `BashOutputTool`, `BashStopTool`, and `BackgroundShells::start`, which spawns a background shell through the same `ShellTree`.
- `crates/ox-core/src/tool/bash/msys.rs`: `MsysRuntime`, which holds the MSYS runtime.
- `crates/ox-tui/src/app/events.rs`: the list shown by `/status`, replaced on each `BackgroundShellsUpdated` and cleared on `SessionRolled` and `SessionResumed`.
- `crates/ox-tui/src/slash/status.rs`: the Background Shells section.
