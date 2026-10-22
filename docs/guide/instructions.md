# Instruction Files

Instruction files are Markdown files that provide persistent, project-specific context for the assistant. oxide-code loads them when a session starts and includes them with every request.

## Supported Filenames

oxide-code reads `AGENTS.md`, the file Codex and other agents read too, and falls back to Claude Code's `CLAUDE.md`, so existing Claude Code instructions work unchanged. Each location loads at most one file: the first in the table below that has content. A file that exists but cannot be read, such as one without read permission or one that is not UTF-8 text, is skipped with a notice, and the next file is tried. Each file is read as written, without expanding Claude Code's `@path` imports.

## Where Instruction Files Can Live

Instruction files are loaded from two scopes:

| Scope   | Files, in the order tried                                                                                                                   | Purpose                                    |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| User    | `~/.config/ox/AGENTS.md` (or `$XDG_CONFIG_HOME/ox/AGENTS.md`), then `~/.claude/CLAUDE.md` (or `$CLAUDE_CONFIG_DIR/CLAUDE.md`)               | Personal preferences that apply everywhere |
| Project | `<dir>/AGENTS.md`, `<dir>/CLAUDE.md`, then `<dir>/.claude/CLAUDE.md`, in each directory from the project root down to the working directory | Team-shared conventions                    |

The project root is the nearest directory at or above the working directory that holds a `.git` directory or file, or the working directory if it is outside a repository. [Sessions](sessions.md#where-sessions-live) are grouped by the same project. Files closer to the working directory appear later in the prompt and conventionally take precedence. For a working directory of `/repo/crates/core`, files load in this order:

1. The user file
2. `/repo/`
3. `/repo/crates/`
4. `/repo/crates/core/`

A file loads once, at the first location that resolves to it. A `CLAUDE.md` symlink to `AGENTS.md` therefore adds nothing, and neither does the user `CLAUDE.md` when a project location resolves to it, as `~/.claude/CLAUDE.md` does with the working directory at your home directory and `CLAUDE_CONFIG_DIR` unset. A directory whose first file with content was already loaded, such as a subdirectory `AGENTS.md` that links to the root's, loads nothing further.

Project instruction files come from the checkout, so oxide-code loads one only when it resolves to a regular Markdown (`.md`) file inside the project root. A symlink from `CLAUDE.md` to `AGENTS.md` or to `docs/ai.md` works. A project file that resolves outside the root or to a file without the `.md` extension is skipped with a notice, and oxide-code tries the next file at that location. When the project root is your home directory or one of its ancestors, oxide-code also skips any project file it reaches through a symlink. User files can link anywhere.

## When Changes Take Effect

- **Edits during a session** reach the model as a reminder with your next prompt. A date change since the session started arrives the same way.
- **Resuming a session** with `/resume` or `ox -c` keeps the instructions and date from when the session started, so the cached prompt prefix stays reusable. Edits made since then arrive as a reminder with your next prompt.
- **`/clear` and `/compact`** start from the current files and date.

## Writing Effective Instructions

A typical file lists coding conventions and project-specific rules:

```markdown
# AGENTS.md

## Coding conventions

- Use snake_case for function names and SCREAMING_SNAKE_CASE for constants.
- All public functions must have doc comments.
- Error handling: use `anyhow::Result` in application code, `thiserror` for library errors.

## Project-specific rules

- Do not modify files in `vendor/` or `generated/`.
- Run `cargo test` after any code change.
- Commit messages follow conventional commits: `type(scope): description`.
```

Tips:

- Run `/init` to have the assistant draft a file from the codebase or suggest edits to an existing one.
- Keep instructions concise because they consume tokens on every API call.
- Focus on rules the assistant cannot infer from the code itself.
