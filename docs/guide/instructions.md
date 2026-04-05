# Instruction Files

Instruction files let you customize the assistant's behavior with persistent, project-specific context. They are Markdown files discovered automatically each turn and injected into the system prompt.

## Supported filenames

At each location, the following filenames are checked in priority order:

1. `CLAUDE.md`
2. `AGENTS.md`

The first file found at each location wins — if `CLAUDE.md` exists, `AGENTS.md` at the same location is skipped. The dual-filename support means you can use whichever convention your project prefers.

## Discovery hierarchy

Files are discovered from three scopes, loaded in this order (earlier = lower priority):

### 1. User global

```text
~/.claude/CLAUDE.md   or   ~/.claude/AGENTS.md
```

Instructions that apply to all your projects. Useful for personal preferences like coding style, communication tone, or tool usage patterns.

### 2. Project root-level

```text
<dir>/CLAUDE.md   or   <dir>/AGENTS.md
```

Checked at every directory from the project root down to your working directory. The project root is the git repository root when available, otherwise the current working directory.

### 3. Project `.claude/` directory

```text
<dir>/.claude/CLAUDE.md   or   <dir>/.claude/AGENTS.md
```

Same walk as root-level, but inside a `.claude/` subdirectory at each level. Useful for keeping instruction files out of the project root.

### Walk example

For a working directory of `/repo/crates/core`, instruction files are checked at:

| Order | Path                                  | Scope                           |
| ----- | ------------------------------------- | ------------------------------- |
| 1     | `~/.claude/CLAUDE.md`                 | User global                     |
| 2     | `/repo/CLAUDE.md`                     | Project (root)                  |
| 3     | `/repo/.claude/CLAUDE.md`             | Project .claude/ (root)         |
| 4     | `/repo/crates/CLAUDE.md`              | Project (intermediate)          |
| 5     | `/repo/crates/.claude/CLAUDE.md`      | Project .claude/ (intermediate) |
| 6     | `/repo/crates/core/CLAUDE.md`         | Project (CWD)                   |
| 7     | `/repo/crates/core/.claude/CLAUDE.md` | Project .claude/ (CWD)          |

Later entries take higher priority — subdirectory-specific instructions override root-level ones.

## Writing effective instructions

Instruction files are injected verbatim into the system prompt. Write them as direct guidance:

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

- Keep instructions concise — they consume tokens on every API call.
- Focus on rules the assistant can't infer from the code itself.
- Use the global file (`~/.claude/CLAUDE.md`) for personal preferences that apply everywhere.
- Use project-level files for project-specific conventions, build commands, and constraints.
