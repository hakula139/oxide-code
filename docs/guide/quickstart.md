# Quickstart

## Install

See [Installation](install.md) for the full matrix (prebuilt tarball, Homebrew, cargo, Nix). The shortest path on macOS / Linux:

```bash
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code
```

## Set up credentials

oxide-code needs an Anthropic API credential. The simplest way is to set your API key:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

Alternatively, if you have [Claude Code](https://code.claude.com/docs) installed and authenticated, oxide-code can use its OAuth credentials automatically. See [Configuration](configuration.md) for details.

## Start a session

```bash
ox
```

This opens an interactive terminal session. Type a task and press Enter:

```text
❯ Read main.rs and explain the agent loop.
```

The assistant reads files, runs commands, and edits code using its built-in tools until it produces a final answer.

In the default `auto` permission mode, reads and searches run without asking, as do file edits inside the working directory. A `bash` command or a file change outside the working directory opens an approval prompt unless a rule settles it first. An edit to protected config such as `.vscode` or `.envrc` opens one even when an allow rule matches, and edits under the project's own `.git` are denied. In the prompt, press **Y** to approve, and **N** or **Esc** to deny. Enter does nothing, so a keystroke meant for the input box cannot approve a call.

## What it can do

oxide-code has six built-in tools:

| Tool    | Purpose                         |
| ------- | ------------------------------- |
| `bash`  | Run shell commands              |
| `read`  | Read files with line numbers    |
| `write` | Create or overwrite files       |
| `edit`  | Replace exact strings in files  |
| `glob`  | Find files by pattern           |
| `grep`  | Search file contents with regex |

The assistant decides which tools to use based on your request. You can guide it by being specific: "edit the function signature in `src/lib.rs`" is better than "fix the code".

## Customize behavior

Drop a `CLAUDE.md` file in your project root to give the assistant project-specific instructions:

```markdown
# CLAUDE.md

- Use snake_case for all function names.
- Run `cargo test` after making changes.
- Do not modify files in the `vendor/` directory.
```

See [Instruction Files](instructions.md) for the full discovery hierarchy.
