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

- Terminal UI: streaming output, markdown rendering, syntax-highlighted code blocks, and 5 built-in themes with custom-TOML overrides
- Agent loop with extended thinking and tool-use round-trip
- File and search tools: `read`, `write`, `edit`, `glob`, `grep`, `bash`
- Turn interruption (Esc / Ctrl+C) plus mid-turn queued follow-up prompts that splice into the same turn between tool calls, with double-press Ctrl+C exit confirmation
- Slash commands with `/`-triggered autocomplete (name and curated-arg modes): `/clear`, `/compact`, `/config`, `/delete`, `/diff`, `/effort`, `/help`, `/init`, `/model`, `/rename`, `/resume`, `/status`, `/theme`
- `CLAUDE.md` / `AGENTS.md` instruction-file discovery
- Session persistence with JSONL conversation logs, listing, and resume
- Per-session file-change tracking with a Read-before-Edit gate and on-disk drift detection
- Authentication (Anthropic API key, Claude Code OAuth) and layered TOML config

See [`docs/roadmap.md`](docs/roadmap.md) for current focus and plans.

## Installation

Four install paths are supported: prebuilt tarball, Homebrew tap, `cargo install`, and Nix flake. See [Installation](docs/guide/install.md) for the full matrix. The most common one-liners:

```bash
# macOS aarch64 (Apple Silicon)
curl -fsSL https://github.com/hakula139/oxide-code/releases/latest/download/oxide-code-aarch64-apple-darwin.tar.gz | tar -xz
sudo mv ox /usr/local/bin/

# Homebrew (macOS, Linux)
brew tap hakula139/oxide-code https://github.com/hakula139/oxide-code
brew install oxide-code

# From source (any Rust-supported platform)
cargo install --git https://github.com/hakula139/oxide-code --locked

# Nix
nix profile install github:hakula139/oxide-code
```

## Usage

```bash
export ANTHROPIC_API_KEY=sk-ant-...
ox
```

## Documentation

See the [user guide](docs/guide/) for installation, configuration, slash commands, instruction files, sessions, and theming.

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
