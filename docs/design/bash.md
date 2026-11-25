# Shell Execution

`bash` runs each command via `bash -c <command>` in the session's working directory, with empty stdin and a single pipe shared by stdout and stderr so writes appear in arrival order. This page covers process management, the environment, and [background shells](#background-shells), while [Output Truncation](truncation.md#spill-files) covers output capping and spills.

## Process Tree

Each command runs inside a `ShellTree` owning the shell and all descendants, so a timeout, cancellation, or `bash_stop` kills the entire tree:

- **Unix.** The shell leads a new session and process group (`setsid`), and cancellation sends `SIGKILL` to the group. On non-Linux kernels the kill repeats for up to 1 s until no signalable member remains, catching children forked mid-`killpg`.
- **Windows.** Commands run under Git for Windows' `bash.exe` (failing if absent) inside a Job Object whose termination ends every process in it. An idle Git Bash holder initializes the [MSYS runtime](#msys-runtime) first so killing a shell leaves the runtime intact.

Detaching from the controlling terminal makes `/dev/tty` prompts (such as `sudo` or password `ssh`) fail immediately with a model-readable error, avoiding TUI corruption or `SIGTTIN` hangs until timeout.

### MSYS Runtime

Every MSYS process (`bash.exe` and `usr\bin` tools) maps a per-user shared memory region that persists while any MSYS process maps it. The first mapper initializes the mount table under a lock. If it is killed milliseconds into startup, the lock stays held and every subsequent MSYS process (including user Git Bash windows and git's `sh`) blocks 15 s and aborts with `add_item (...) failed, errno 1` until all MSYS processes exit ([git-for-windows/git#6368](https://github.com/git-for-windows/git/issues/6368), [msys2/msys2-runtime#333](https://github.com/msys2/msys2-runtime/pull/333)).

Once initialized, later MSYS processes skip that lock, so `ox` keeps one idle bash running `echo; read` without [credentials](#credentials) (since other MSYS processes can read its environment):

- Before spawning a shell, `ox` ensures the holder is running and asynchronously waits up to 15 s for its ready line, before taking a background slot so cancellation stops the wait and other tools never block. A still-starting holder stays alive for later shells, while a shell started after the holder failed runs unprotected.
- The holder exits automatically when `ox` exits and closes its stdin pipe.
- The shared region spans integrity levels, so an elevated `ox` also initializes it for non-elevated Git Bash.

## Environment

Commands inherit `ox`'s environment minus [credentials](#credentials), `CDPATH`, and `OLDPWD` (leaving `~-` unexpanded until a command changes directories), and set `OX_SESSION_ID` plus `EDITOR`, `GIT_EDITOR`, and `VISUAL` to `true` (covering git's precedence and generic tools). [Commands the Assistant Runs](../guide/permissions.md#commands-the-assistant-runs) documents the user-facing behavior.

### Credentials

`secret_env_vars` derives the withheld variables listed in [Commands and Credentials](../guide/authentication.md#commands-and-credentials) directly from the credential resolution order, so newly added credential variables are withheld automatically while remaining in `ox`'s own environment. Other environment secrets, such as cloud or GitHub tokens, still pass through.

## Background Shells

With `run_in_background: true`, `bash` starts a shell that outlives the tool call for servers, watchers, and long builds. `bash_output` reads unread output, `bash_stop` terminates it, and session teardown ends every open shell.

- **Starting.** Returns immediately with a monotonically increasing ID (`shell-1`, resetting in a new process). `timeout` does not apply. Uses the `exec` class with approval title `Bash (background)`. The tool description tells the model no completion notice arrives and to set the flag in place of a trailing `&`.
- **Reading.** `bash_output` returns output since the previous read or `(no new output)`, followed by `(still running)`, `(exit code N)`, or `(killed by signal N)`. Each read applies the foreground head-and-tail cap and spills overflow to a per-read [spill file](truncation.md#spill-files). An optional `timeout` in ms (capped at 600,000 ms) waits for exit or expiry. Unknown IDs return an error listing open shells and states.
- **Stopping.** `bash_stop` kills the tree, removes the shell, and returns unread output followed by `(stopped)`, `(stopped after it exited with code N)`, or `(stopped after signal N killed it)`.
- **Limit.** Finished shells stay open until stopped or session end so final output remains readable, up to 16 open shells per session (past which `bash` refuses and lists open shells).
- **Permission.** `bash_output` and `bash_stop` belong to `read` and need no approval in any mode ([Rules](../guide/permissions.md#rules) covers class-wide `read` denies). Neither joins a parallel batch because reads drain state and stops affect concurrent reads.

### Shell Lifecycle

A supervisor task owns each background `ShellTree`, drains its shared pipe into the capture, and publishes exit status. Stopping kills the tree while running, or after exit if a descendant still holds the pipe open (such as a trailing `&` server). Once the shell exits and its pipe closes, the supervisor releases the tree as foreground calls do, letting fully redirected descendants survive. `bash_stop` returns after the kill or release and final output drain.

The engine terminates and awaits all open shells on `/clear`, `/archive`, successful `/resume`, and shutdown. Failed resumes and Esc interrupts leave background shells running.

### Compaction

Open shells survive compaction. Because their starting `bash` calls are summarized away, a `<system-reminder>` block in the synthetic summary message lists each open shell's ID, state, and first command line (after the task-list reminder and before any mid-turn resume reminder). Because the reminder persists in the summary, resumed sessions still show it after shells have ended, and `bash_output` then reports the ID as closed.

### Frontends

Every start, exit, or stop emits `Event::BackgroundShellsUpdated` with all open shells' IDs, commands, and states. Session teardown emits no event, so frontends clear their copy on `SessionRolled` and `SessionResumed`. The TUI lists open shells in `/status`, and `stream-json` emits `background_shells_updated` ([headless guide](../guide/headless.md#events)).

## Design Decisions

1. **Detach from the terminal with a new session.** A new process group alone still shares the controlling terminal, letting `/dev/tty` prompts corrupt the TUI. Codex (`detach_from_tty` in `codex-rs/utils/pty/src/process_group.rs`), Claude Code, and pi (Node `detached: true`) also call `setsid`. The session leader still leads its process group for group kills.
2. **Set the editors and leave pagers alone.** Pagers check for a TTY and stay off on pipes, whereas editors launch regardless (including GUI editors). Claude Code sets only `GIT_EDITOR=true`, Codex sets pagers because it uses a PTY, and oh-my-pi sets about 40 variables (`CI=true`, `TERM=dumb`, and others) that alter build and test behavior, so `ox` sets only the editor trio.
3. **Leave credential prompts to the missing terminal.** Without a controlling terminal, git and ssh terminal prompts fail on their own without `GIT_TERMINAL_PROMPT=0`, while GUI helpers like Git Credential Manager still open for the user.
4. **Withhold ox's own credentials by default.** `ox` needs its credentials only in-process, and the permission gate cannot stop an allowed command from reading `env`. Claude Code scrubs them only under `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`, Codex withholds only self-created tokens, oh-my-pi strips keys only from its Python kernel, and pi passes everything. Leaving interactive runs unfiltered exposes credentials to prompt injection, while non-ox secrets pass through because builds and deploys may need them.
5. **Hold the MSYS runtime from ox.** Timeouts, cancels, `bash_stop`, `/clear`, resume, and exit can kill a starting shell, breaking MSYS for both users and tests. Retrying hits the same lock, and waiting for each shell to initialize before allowing cancellation would delay cancels. Claude Code and pi end Git Bash trees with `taskkill /T /F` unguarded ([anthropics/claude-code#30165](https://github.com/anthropics/claude-code/issues/30165)), Codex uses PowerShell, and oh-my-pi embeds brush (though Git for Windows tools on `PATH` remain MSYS). The holder costs one idle process and tens of milliseconds on first shell startup.
6. **Share one pipe between stdout and stderr.** Arrival order keeps panics beside preceding test lines, matching Claude Code (shared fd) and pi (single `onData` callback). The tradeoffs are late buffered stdout (such as a Python `print` appearing after a traceback) and short stderr potentially falling inside the omitted middle past 128 KiB (preserved in the spill file). `PYTHONUNBUFFERED` stays unset to avoid changing build or test behavior (**Set the editors and leave pagers alone**), and separate pipes still could not fix in-process buffering.
7. **Read incrementally from memory.** Incremental `bash_output` avoids rereading old output (Claude Code writes to a file and shows its tail). Reading before about 128 KiB accumulates sees the full stream, and oversized reads spill only that window.
8. **Report an exit on the next read.** Unlike Claude Code's `<task-notification>` messages, shells never inject turns or messages autonomously, and the tool description warns models used to Claude Code not to wait for one. `bash_output`'s `timeout` covers waiting on builds.
9. **Keep finished shells open until stopped.** Auto-closing on exit would drop unread output. The 16-shell cap bounds held captures, compared with Codex's soft cap of 64 processes and oh-my-pi's cap of 15 running jobs.
10. **End shells with the session.** Orphaned dev servers would hold ports indefinitely (Codex also kills children at teardown). Awaiting each tree ensures even signal exits clean up.
11. **Release a finished tree whose pipe is closed.** Once exited with no open pipe, `ox` cannot track process-group emptiness and killing later risks hitting a reused process group ID, matching foreground behavior for redirected servers.
12. **Skip approval for reads and stops.** Both act only on shells an allowed `bash` call already started.

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
