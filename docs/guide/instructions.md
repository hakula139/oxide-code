# Instruction Files

Instruction files let you give the assistant persistent, project-specific context. They are Markdown files discovered on every turn and injected into the system prompt.

## Supported filenames

At each location, oxide-code looks for these filenames in order:

1. `CLAUDE.md`
2. `AGENTS.md`

The first file found at each location wins. Use whichever convention your project prefers.

## Where instruction files can live

Instruction files are loaded from three scopes:

| Scope            | Path                                                                      | Purpose                                    |
| ---------------- | ------------------------------------------------------------------------- | ------------------------------------------ |
| User global      | `~/.claude/CLAUDE.md`                                                     | Personal preferences that apply everywhere |
| Project          | `<dir>/CLAUDE.md` — every directory from the project root down to the CWD | Team-shared conventions                    |
| Project (hidden) | `<dir>/.claude/CLAUDE.md` — same walk, inside `.claude/`                  | Same as above, but out of the project root |

More specific locations override broader ones. For a working directory of `/repo/crates/core`, files are loaded (and merged) in this order: `~/.claude/` → `/repo/` → `/repo/crates/` → `/repo/crates/core/`, checking both the root-level and `.claude/` variants at each step. The project root is the git repository root when available, otherwise the current working directory.

## Writing effective instructions

Instruction files are injected verbatim into the system prompt:

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
- Put personal preferences in the global file; put team-shared conventions at the project level.
