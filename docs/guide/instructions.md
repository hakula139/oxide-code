# Instruction Files

Instruction files are Markdown files loaded at session start and included with every request.

## Where Instruction Files Can Live

oxide-code reads `AGENTS.md` (used by Codex and other agents) and falls back to Claude Code's `CLAUDE.md`. Each location loads its first non-empty file:

| Scope   | Files, in the order tried                                                                                                                   | Purpose                                    |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| User    | `~/.config/ox/AGENTS.md` (or `$XDG_CONFIG_HOME/ox/AGENTS.md`), then `~/.claude/CLAUDE.md` (or `$CLAUDE_CONFIG_DIR/CLAUDE.md`)               | Personal preferences that apply everywhere |
| Project | `<dir>/AGENTS.md`, `<dir>/CLAUDE.md`, then `<dir>/.claude/CLAUDE.md`, in each directory from the project root down to the working directory | Team-shared conventions                    |

The project root is the nearest directory at or above the working directory holding a `.git` directory or file, or the working directory outside a repository (matching [Sessions](sessions.md#where-sessions-live)). Files closer to the working directory appear later in the prompt and conventionally take precedence. For `/repo/crates/core`, load order is:

1. The user file
2. `/repo/`
3. `/repo/crates/`
4. `/repo/crates/core/`

- **Deduplication**: Each file loads once at its first resolved location (so a `CLAUDE.md` symlink to `AGENTS.md` adds nothing). If a location's first non-empty file was already loaded, that location loads nothing else.
- **Project containment**: Project files must resolve to regular `.md` files inside the project root (symlinks to `AGENTS.md` or `docs/ai.md` work). When the project root is your home directory or an ancestor, any project file reached through a symlink is skipped. Invalid files emit a notice and fall through to the next candidate. User files may link anywhere.
- **Unreadable files**: Unreadable or non-UTF-8 files emit a notice and fall through to the next candidate.
- **`@path` imports**: Kept verbatim (not expanded as in Claude Code).

## When Changes Take Effect

- **Mid-session edits and date changes**: Delivered as a reminder on your next prompt.
- **Resume** (`/resume`, `ox -c`): Keeps the original instructions and date so the cached prompt prefix stays valid. Subsequent edits arrive as a reminder on your next prompt.
- **`/clear` and `/compact`**: Reload current files and date.

## Writing Effective Instructions

Keep instructions concise (they consume tokens on every request) and focus on rules the assistant cannot infer from the code. Run `/init` to draft or suggest edits to a file from the codebase.
