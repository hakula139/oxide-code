# oxide-code

[![CI](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml/badge.svg)](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A terminal-based AI coding assistant written in Rust, inspired by [Claude Code](https://code.claude.com/docs).

## Overview

oxide-code is a Rust reimplementation of Claude Code — an interactive CLI agent that helps developers with software engineering tasks. It communicates with LLM APIs to understand codebases, write code, run commands, and manage development workflows directly from the terminal.

## Status

Early development. See [`docs/roadmap.md`](docs/roadmap.md) for the current roadmap.

## Usage

```bash
ox
```

## Building from Source

Requires [Rust](https://www.rust-lang.org/tools/install) 1.85+ (edition 2024).

```bash
cargo build --release
```

The binary will be at `target/release/ox`.

## Development

```bash
cargo build                    # Build
cargo fmt --all --check        # Check formatting
cargo clippy --all-targets     # Lint (pedantic)
cargo test                     # Run tests
```

CI runs these same checks on every push and pull request via GitHub Actions.

## License

Copyright (c) 2026 [Hakula](https://hakula.xyz). Licensed under the [MIT License](LICENSE).
