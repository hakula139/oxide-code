# Quickstart

## Install

On Apple Silicon macOS and x86_64 Linux, install with Homebrew:

```bash
brew install hakulab/tap/oxide-code
```

See [Installation](install.md) for the prebuilt binary, Cargo, Nix, and other platforms.

## Set Up Credentials

Use any of these:

- **An Anthropic API key**: `export ANTHROPIC_API_KEY=sk-ant-...`
- **A Claude account**: Run `ox`, then `/login` to sign in through your browser.
- **An existing [Claude Code](https://code.claude.com/docs) login**: oxide-code uses it automatically.

[Authentication](authentication.md) lists every credential source and the order in which they apply.

## Start a Session

Run `ox`, type a task, and press **Enter**:

```text
❯ Read main.rs and explain the agent loop.
```

| Key                                    | Action                                                     |
| -------------------------------------- | ---------------------------------------------------------- |
| Mouse wheel, **PageUp** / **PageDown** | Scroll the chat, and **Ctrl+End** follows new output again |
| **Ctrl+O**                             | Expand or collapse long tool output and finished thinking  |
| **Tab** / **Shift+Tab**                | Step through the [permission modes](permissions.md#modes)  |
| Drag                                   | Select chat text, and copy it to the clipboard on release  |
| **Esc**                                | Stop a running turn                                        |
| **Ctrl+C** twice, or **Ctrl+D** twice  | Exit, with **Ctrl+D** working only on an empty input       |

[Terminal UI](terminal-ui.md) covers new lines, the other input keys, queued prompts, and copying over SSH.

## Approve Tool Calls

In the default `auto` permission mode, a separate model approves routine tool calls and prompts on the rest:

| Key     | Effect                                                        |
| ------- | ------------------------------------------------------------- |
| **Y**   | Approve the call                                              |
| **A**   | Approve, and allow matching calls for the rest of the session |
| **N**   | Deny the call                                                 |
| **F**   | Deny with a note the model reads                              |
| **Esc** | Deny and end the turn                                         |

When offered, **A** opens the proposed pattern (such as `cargo test`) for editing before **Enter** (for example widening to `cargo test *`). See [Permissions](permissions.md) for modes, rules, and pattern syntax.

## Tasks and Background Commands

- **Task list**: For multi-step work, the assistant tracks tasks above the input. The list clears once every task is done and persists across `/compact` and resume.
- **Background shells**: The assistant can run a dev server, watcher, or long build in the background and read output via `bash_output` while continuing. `/status` lists open shells. They survive **Esc** and stop on exit, `/clear`, `/archive`, or `/resume` (or ask the assistant to stop one sooner).

## Customize Behavior

Add an `AGENTS.md` at your project root for conventions and verification commands (`CLAUDE.md` works too, and `/init` drafts one from the codebase). See [Instruction Files](instructions.md) for locations and precedence.
