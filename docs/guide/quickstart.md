# Quickstart

This guide walks through installing `ox`, signing in, and running a first session, including its tool approvals.

## Install

On Apple Silicon macOS and x86_64 Linux, install with Homebrew:

```bash
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code
```

See [Installation](install.md) for the prebuilt binary, Cargo, Nix, and other platforms.

## Set Up Credentials

The easiest way to authenticate is with an Anthropic API key:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

You can also sign in with a Claude account:

- Run `ox`, then `/login` to sign in through your browser.
- If [Claude Code](https://code.claude.com/docs) is installed and signed in, oxide-code uses that login automatically.

[Authentication](configuration.md#authentication) lists all credential sources and the order in which they apply.

## Start a Session

Run `ox` to start an interactive terminal session, then type a task and press **Enter**:

```text
❯ Read main.rs and explain the agent loop.
```

The assistant uses its built-in tools to read files, run commands, and edit code until it returns a final answer.

In the chat:

- **Scroll**: Use the mouse wheel or **PageUp** / **PageDown**, and press **Ctrl+End** to follow new output again.
- **Expand output**: Press **Ctrl+O** to show long tool output and finished thinking in full, and again to collapse them.
- **Switch modes**: Press **Shift+Tab** to switch to the read-only `plan` [permission mode](permissions.md#modes) and back.
- **Select and copy**: Drag over chat text to select it, and release to copy it to the clipboard.

[Terminal UI](terminal-ui.md) covers the input keys, queued prompts, interrupting and exiting, and copying over SSH.

## Approve Tool Calls

In the default `manual` permission mode, tool calls fall into these groups:

- **Runs without asking**: reads, searches, read-only commands such as `ls`, `cat`, and `grep`, and edits within the working directory.
- **Asks unless a rule decides**: other `bash` commands and edits outside the working directory.
- **Asks even when an allow pattern matches**: edits to protected config such as `.git`, `.vscode`, or `.envrc`, and forced recursive deletes such as `rm -rf`.
- **Denied**: reads of SSH keys and credential files.

The approval prompt takes these keys:

| Key     | Effect                                                        |
| ------- | ------------------------------------------------------------- |
| **Y**   | Approve the call                                              |
| **A**   | Approve, and allow matching calls for the rest of the session |
| **N**   | Deny the call                                                 |
| **F**   | Deny with a note the model reads                              |
| **Esc** | Deny and end the turn                                         |

When a pattern can name the call, pressing **A** opens the pattern it will allow, such as `cargo test`, which you can edit before pressing **Enter**, for example to widen it to `cargo test *`. Session rules last until you exit or start another session, and `/status` lists them.

See [Permissions](permissions.md) for the other modes, the rest of the prompt's behavior, and how to write patterns.

## What It Can Do

oxide-code has nine built-in tools:

| Tool          | Purpose                                          |
| ------------- | ------------------------------------------------ |
| `bash`        | Run shell commands                               |
| `bash_output` | Read what a background shell printed             |
| `bash_stop`   | Stop a background shell                          |
| `read`        | Read files with line numbers                     |
| `write`       | Create or overwrite files                        |
| `edit`        | Replace exact strings in files                   |
| `glob`        | Find files by pattern                            |
| `grep`        | Search file contents with regex                  |
| `tasks`       | Keep a task list for multi-step work, shown live |

The assistant chooses tools based on your request, so specific requests work best. For example, "edit the function signature in `src/lib.rs`" is more useful than "fix the code".

- **Task list**: For multi-step work, the assistant maintains a task list that the TUI displays above the input. It marks completed tasks with `✓`, the task in progress with `●`, and pending tasks with `○`. The list clears when all tasks are done and stays with the session through `/compact` and resume.
- **Background shells**: For a dev server, watcher, or long build, the assistant can run a command in the background and read its output with `bash_output` while it continues working. `/status` lists open shells. They keep running if you interrupt a turn with **Esc** and end when you exit or start another session with `/clear`, `/archive`, or `/resume`. To stop one sooner, ask the assistant.

## Customize Behavior

Add an `AGENTS.md` file to your project root with project-specific instructions, such as coding conventions and commands to run after a change. An existing `CLAUDE.md` works too. Run `/init` to have the assistant draft one based on the codebase.

See [Instruction Files](instructions.md) for where these files can live and how they combine.
