# Quickstart

## Install

Requires [Rust](https://www.rust-lang.org/tools/install) 1.91+ (edition 2024).

```bash
cargo install --path crates/oxide-code
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

This opens an interactive REPL. Type a task and press Enter:

```text
> Read main.rs and explain the agent loop.
```

The assistant reads files, runs commands, and edits code using its built-in tools until it produces a final answer.

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
