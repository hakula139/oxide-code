# oxide-code

[![CI](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml/badge.svg)](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/hakula139/oxide-code/graph/badge.svg)](https://codecov.io/gh/hakula139/oxide-code)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A terminal-based AI coding assistant written in Rust, inspired by [Claude Code].

[Claude Code]: https://code.claude.com/docs

## Overview

oxide-code is a Rust reimplementation of Claude Code — an interactive CLI agent that helps developers with software engineering tasks. It communicates with LLM APIs to understand codebases, write code, run commands, and manage development workflows directly from the terminal.

## Status

Early development. What works today:

- Terminal UI with markdown rendering, syntax-highlighted code blocks, and streaming display
- Runtime-loaded themes — 4 built-in Catppuccin variants and per-slot overrides via your config file (`~/.config/ox/config.toml` or project-level `ox.toml`)
- Agent loop with streaming and extended thinking
- File and search tools (read, write, edit, glob, grep, bash)
- System prompt with CLAUDE.md / AGENTS.md injection
- Authentication (API key and Claude Code OAuth)
- TOML config file with layered loading
- Session persistence with JSONL conversation logs, resume, and listing

See [`docs/roadmap.md`](docs/roadmap.md) for current focus and plans.

## Usage

```bash
export ANTHROPIC_API_KEY=sk-ant-...
ox
```

## Documentation

| Document                                        | Description                                      |
| ----------------------------------------------- | ------------------------------------------------ |
| [Quickstart](docs/guide/quickstart.md)          | Install, first run, basic usage                  |
| [Configuration](docs/guide/configuration.md)    | API credentials, model selection, environment    |
| [Instruction Files](docs/guide/instructions.md) | CLAUDE.md / AGENTS.md setup and discovery rules  |
| [Sessions](docs/guide/sessions.md)              | Session persistence, listing, and resume         |
| [Theming](docs/guide/theming.md)                | Built-in palettes, custom themes, slot overrides |

## Building from Source

Requires [Rust](https://www.rust-lang.org/tools/install) 1.91+ (edition 2024).

```bash
cargo build --release
```

The binary will be at `target/release/ox`.

## Development

```bash
cargo fmt --all --check                            # Check formatting
cargo build                                        # Build
cargo clippy --all-targets -- -D warnings          # Lint (pedantic, zero warnings)
cargo test                                         # Run tests
cargo llvm-cov --ignore-filename-regex 'main\.rs'  # Check test coverage
```

CI runs these same checks on every push and pull request via GitHub Actions.

## License

Copyright (c) 2026 [Hakula](https://hakula.xyz). Licensed under the [MIT License](LICENSE).
