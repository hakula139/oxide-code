# Instruction Files

Instruction files are Markdown files that provide persistent, project-specific context for the assistant. oxide-code loads them when a session starts and includes them with every request.

## Supported Filenames

At each location, oxide-code loads `CLAUDE.md`, or `AGENTS.md` if `CLAUDE.md` is missing, empty, or unreadable. A file that exists but cannot be read, such as one without read permission or one that is not UTF-8 text, is skipped with a notice. Choose the filename your project prefers. Each file is read as written, without expanding Claude Code's `@path` imports.

## Where Instruction Files Can Live

Instruction files are loaded from three scopes:

| Scope            | Path                                                                                   | Purpose                                    |
| ---------------- | -------------------------------------------------------------------------------------- | ------------------------------------------ |
| User global      | `~/.claude/CLAUDE.md`                                                                  | Personal preferences that apply everywhere |
| Project          | `<dir>/CLAUDE.md`, every directory from the project root down to the working directory | Team-shared conventions                    |
| Project (hidden) | `<dir>/.claude/CLAUDE.md`, same walk inside `.claude/`                                 | Same as above, but out of the project root |

The project root is the Git repository root, or the working directory if it is outside a repository. Files closer to the working directory appear later in the prompt and conventionally take precedence. For a working directory of `/repo/crates/core`, files load in this order:

1. `~/.claude/`
2. `/repo/`, then `/repo/.claude/`
3. `/repo/crates/`, then `/repo/crates/.claude/`
4. `/repo/crates/core/`, then `/repo/crates/core/.claude/`

## When Changes Take Effect

- **Edits during a session** reach the model as a reminder with your next prompt. A date change since the session started arrives the same way.
- **Resuming a session** with `/resume` or `ox -c` keeps the instructions and date from when the session started, so the cached prompt prefix stays reusable. Edits made since then arrive as a reminder with your next prompt.
- **`/clear` and `/compact`** start from the current files and date.

## Writing Effective Instructions

A typical file lists coding conventions and project-specific rules:

```markdown
# CLAUDE.md

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
