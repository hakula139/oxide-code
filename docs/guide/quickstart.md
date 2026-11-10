# Quickstart

## Install

On Apple Silicon macOS and x86_64 Linux, install with Homebrew:

```bash
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code
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

In the default `auto` permission mode, a separate model approves routine tool calls and asks for your confirmation on the rest. The approval prompt takes these keys:

| Key     | Effect                                                        |
| ------- | ------------------------------------------------------------- |
| **Y**   | Approve the call                                              |
| **A**   | Approve, and allow matching calls for the rest of the session |
| **N**   | Deny the call                                                 |
| **F**   | Deny with a note the model reads                              |
| **Esc** | Deny and end the turn                                         |

**A** opens the pattern it will allow, such as `cargo test`, which you can edit before pressing **Enter**, for example to widen it to `cargo test *`. [Permissions](permissions.md) covers the other modes, your own rules, and the pattern syntax.

## Tasks and Background Commands

- **Task list**: For multi-step work, the assistant keeps a task list that the TUI shows above the input. It clears once every task is done and stays with the session through `/compact` and resume.
- **Background shells**: The assistant can run a dev server, watcher, or long build in the background and read its output with `bash_output` while it keeps working. `/status` lists open shells. They survive **Esc** and end when you exit or start another session with `/clear`, `/archive`, or `/resume`. To stop one sooner, ask the assistant.

## Customize Behavior

Add an `AGENTS.md` file to your project root with project-specific instructions, such as coding conventions and the commands to run after a change. An existing `CLAUDE.md` works too, and `/init` has the assistant draft one from the codebase. [Instruction Files](instructions.md) covers where these files can live and how they combine.
