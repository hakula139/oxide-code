# Shell Execution

The `bash` tool runs each command as `bash -c <command>` in the session's working directory, with stdin closed and stdout and stderr piped back into the result. This page describes the process that runs the command and the environment it sees. [Background Shells](background-shells.md) covers commands that outlive their call, while [Output Truncation](truncation.md#spill-files) explains how the pipes are capped and spilled.

## Process Tree

Each command runs in a `ShellTree` containing the shell and all its descendants, so a timeout, cancelled call, or stopped background shell kills the entire tree.

- **Unix.** The shell leads a new session and process group (`setsid`). A timeout or cancel sends `SIGKILL` to the whole group. On kernels other than Linux, such as XNU, the kill repeats for up to 1 s until no member it can signal is alive, because a child forked during `killpg` can miss the signal there.
- **Windows.** The shell runs in a Job Object, whose termination ends every process in it. Git for Windows' `bash.exe` runs the command, and the call fails if Git Bash is missing.

Because the session has no controlling terminal, a command that prompts on `/dev/tty`, such as `sudo`, `ssh` with password authentication, or `git push` over HTTPS without a credential helper, fails immediately with an error the model can read. In ox's own session, the prompt would print over the TUI or REPL, and the read would stop the process with `SIGTTIN` until a timeout or `bash_stop` killed it.

## Environment

The command inherits ox's environment and adds these variables:

| Variable                         | Value            | Purpose                                                                                          |
| -------------------------------- | ---------------- | ------------------------------------------------------------------------------------------------ |
| `OX_SESSION_ID`                  | The session's ID | Lets a script tell which session started it                                                      |
| `EDITOR`, `GIT_EDITOR`, `VISUAL` | `true`           | Makes a command that opens an editor keep its default text, so nothing waits on an editor window |

With the editor set to `true`, `git rebase --continue` and `git cherry-pick --continue` keep the prepared message, while `git commit` without `-m` stops with `Aborting commit due to empty commit message`. `GIT_EDITOR` takes precedence over git's `core.editor`, which takes precedence over `VISUAL` and `EDITOR`, so all three are needed for git and for tools that fall back to the generic pair, such as `gh` and `crontab -e`.

## Design Decisions

1. **Detach from the terminal with a new session.** A child in a new process group still shares ox's controlling terminal, so a `/dev/tty` prompt appears on the user's screen with no way for the model to answer it. Codex detaches its shell tool the same way (`detach_from_tty` in `codex-rs/utils/pty/src/process_group.rs`), while Claude Code and pi spawn bash with Node's `detached: true`, which also calls `setsid`. A session leader also leads its process group, so the group kill still reaches every descendant.
2. **Set the editors and leave pagers alone.** Pagers such as `less` and git's pager start only when stdout is a terminal. Here stdout is always a pipe, so setting a pager variable would have no effect. An editor starts regardless of stdout, and a GUI editor opens on the user's screen. Claude Code sets only `GIT_EDITOR=true`, leaving tools that read `VISUAL` or `EDITOR` uncovered. Codex sets `PAGER` and `GIT_PAGER` to `cat` because its unified exec runs commands in a pseudo-terminal. oh-my-pi sets those variables, the editors, and about 40 other non-interactive defaults, such as `CI=true` and `TERM=dumb`. Those defaults change how builds and tests behave, so ox sets only the variables that keep a command from waiting for input that cannot arrive.
3. **Leave credential prompts to the missing terminal.** Without a controlling terminal, git's and ssh's terminal prompts fail on their own, so `GIT_TERMINAL_PROMPT=0` would add nothing. A GUI credential helper, such as Git Credential Manager, still opens because the user can answer its prompt.

## Sources

- `crates/ox-core/src/tool/bash.rs`: `ShellTree`, `SESSION_ID_VAR`, `EDITOR_VARS`, and `bash`, which finds Git Bash on Windows.
- `crates/ox-core/src/tool/bash/background.rs`: `BackgroundShells::start`, which spawns a background shell through the same `ShellTree`.
