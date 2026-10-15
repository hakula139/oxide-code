# oxide-code

[![CI](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml/badge.svg)](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/hakula139/oxide-code/graph/badge.svg)](https://codecov.io/gh/hakula139/oxide-code)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A terminal-based AI coding assistant written in Rust, inspired by [Claude Code].

[Claude Code]: https://code.claude.com/docs

## Overview

oxide-code is a Rust reimplementation of Claude Code, an interactive CLI agent that helps developers with software engineering tasks. It communicates with LLM APIs to understand codebases, write code, run commands, and manage development workflows directly from the terminal.

## Features

Early development. What works today:

- Terminal UI: streaming output, markdown rendering, syntax-highlighted code blocks, [configurable status line](docs/guide/configuration.md#tui-terminal-ui), and 5 built-in themes with custom-TOML overrides
- Bare REPL (`--no-tui`) and headless one-shot mode (`-p`) for scripts and pipelines
- Agent loop with extended thinking and tool-use round-trip
- File and search tools: `read`, `write`, `edit`, `glob`, `grep`, `bash`
- Tool permissions: `auto` (default), `plan`, and `yolo` modes, allow / deny rules, protected config paths whose edits need approval in `auto` mode even when an allow rule matches, and a TUI approval prompt for gated calls
- Turn interruption (Esc / Ctrl+C) plus mid-turn queued follow-up prompts that splice into the same turn between tool calls, with double-press Ctrl+C exit confirmation
- Slash commands with `/`-triggered autocomplete (name and curated-arg modes): `/clear`, `/compact`, `/config`, `/delete`, `/diff`, `/effort`, `/help`, `/init`, `/model`, `/rename`, `/resume`, `/status`, `/theme`
- Context compression with manual `/compact` and default-on auto-compaction
- `CLAUDE.md` / `AGENTS.md` instruction-file discovery
- Session persistence with JSONL conversation logs, AI-generated titles, listing, and resume
- Per-session file-change tracking with a Read-before-Edit gate and on-disk drift detection
- Authentication (Anthropic API key, Claude Code OAuth) and layered TOML config

## Scope

oxide-code follows a few principles:

- Keep the terminal as the primary interface: streaming chat, tool output, and session controls stay keyboard-first.
- Keep context and state visible: model, instructions, compaction, queued prompts, and session identity should be inspectable from the UI.
- Add workflow depth only when it fits the current agent-harness model.

Planned work lives in the [open enhancement issues](https://github.com/hakula139/oxide-code/issues?q=is%3Aissue%20is%3Aopen%20label%3Aenhancement), and themes that span several issues are grouped under [epics](https://github.com/hakula139/oxide-code/issues?q=is%3Aissue%20is%3Aopen%20label%3Aepic).

Out of scope for now:

- Editor-specific extensions and a GUI. Editors will be able to connect as ACP clients once [#150](https://github.com/hakula139/oxide-code/issues/150) ships.
- In-process plugin runtimes, such as WASM modules, dynamic libraries, or embedded scripting.
- Compatibility with Claude Code settings files, hook formats, or permission rule syntax. oxide-code reads `CLAUDE.md` instructions and Claude Code OAuth credentials, and its own configuration may diverge where its design calls for it.
- Feature parity with Claude Code. Focus on the core workflow first.

## Installation

Four install paths are supported: prebuilt tarball, Homebrew tap, `cargo install`, and Nix flake. See [Installation](docs/guide/install.md) for the full matrix. The most common one-liners:

```bash
# macOS aarch64 (Apple Silicon). Set VERSION to the newest tag on the Releases page.
VERSION=v0.1.0-alpha.2
curl -fsSL "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz" | tar -xz
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
pnpm format                                        # Check Markdown, TOML, JSON, and YAML formatting
pnpm lint                                          # Lint Markdown
pnpm spellcheck                                    # Spell check
```

CI runs on pushes to `main`, pull requests, and manual dispatch. It runs every check above except `cargo build`, along with an MSRV check, Windows clippy, `nix flake check`, and Nix builds on Linux and macOS. Run `cargo build` locally as a quick compile check.

## License

Copyright (c) 2026 [Hakula](https://hakula.xyz). Licensed under the [MIT License](LICENSE).
