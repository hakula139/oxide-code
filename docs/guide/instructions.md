# Instruction Files

Instruction files are Markdown files that give the assistant persistent, project-specific context. oxide-code loads them when a session starts and includes them with every request.

## Where Instruction Files Can Live

oxide-code reads `AGENTS.md`, the file Codex and other agents read, and falls back to Claude Code's `CLAUDE.md`, so existing Claude Code instructions work unchanged. Each location loads the first file in the table that has content:

| Scope   | Files, in the order tried                                                                                                                   | Purpose                                    |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| User    | `~/.config/ox/AGENTS.md` (or `$XDG_CONFIG_HOME/ox/AGENTS.md`), then `~/.claude/CLAUDE.md` (or `$CLAUDE_CONFIG_DIR/CLAUDE.md`)               | Personal preferences that apply everywhere |
| Project | `<dir>/AGENTS.md`, `<dir>/CLAUDE.md`, then `<dir>/.claude/CLAUDE.md`, in each directory from the project root down to the working directory | Team-shared conventions                    |

The project root is the nearest directory at or above the working directory that holds a `.git` directory or file, or the working directory outside a repository. [Sessions](sessions.md#where-sessions-live) are grouped by the same project. Files closer to the working directory appear later in the prompt and conventionally take precedence. For a working directory of `/repo/crates/core`, files load in this order:

1. The user file
2. `/repo/`
3. `/repo/crates/`
4. `/repo/crates/core/`

- **Each file loads once**, at the first location that resolves to it, so a `CLAUDE.md` symlink to `AGENTS.md` adds nothing. A location whose first file with content was already loaded loads nothing further.
- **A project file must resolve to a regular `.md` file inside the project root**, since it comes from the checkout. A symlink to `AGENTS.md` or to `docs/ai.md` works. A file that fails either check is skipped with a notice, and oxide-code tries the next file at that location. When the project root is your home directory or one of its ancestors, oxide-code skips any project file it reaches through a symlink. User files can link anywhere.
- **A file that cannot be read**, such as one without read permission or one that is not UTF-8 text, is skipped with a notice, and the next file is tried.
- **`@path` imports** stay as written, since oxide-code does not expand them as Claude Code does.

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

Keep instructions concise, since they consume tokens on every request, and focus on rules the assistant cannot infer from the code. Run `/init` to have the assistant draft a file from the codebase or suggest edits to an existing one.
