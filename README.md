# oxide-code

[![CI](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml/badge.svg)](https://github.com/hakula139/oxide-code/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/hakula139/oxide-code/graph/badge.svg)](https://codecov.io/gh/hakula139/oxide-code)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![WakaTime coding time for oxide-code](https://wakatime.com/badge/user/f4a35a1f-0e29-4093-a647-e66aad164737/project/12cf90d4-91c9-44ab-b8a9-57954dba9812.svg)

A terminal-based AI coding assistant written in Rust, inspired by [Claude Code].

[Claude Code]: https://code.claude.com/docs

## Overview

oxide-code (`ox`) is a terminal agent for software engineering tasks. It runs Claude models through the Anthropic API or a compatible gateway to explore codebases, edit files, and run commands, with tool permissions and resumable sessions.

## Features

Early development. What works today:

- Terminal UI: streaming output, markdown rendering, syntax-highlighted code blocks, [configurable status line](docs/guide/configuration.md#tui-terminal-ui), and 5 built-in themes with custom-TOML overrides
- Bare REPL (`--no-tui`) and [headless one-shot mode](docs/guide/headless.md) (`-p`) for scripts and pipelines, with output as text, a JSON result, or a JSON event stream
- Agent loop with extended thinking and tool-use round-trip
- Claude Opus 5.5, Sonnet 5.5, and Haiku 5.5, each with a 1M-token context window and adjustable effort
- File and search tools: `read`, `write`, `edit`, `glob`, `grep`, `bash`
- Tool permissions: `auto` (default), `plan`, and `yolo` modes, allow / deny rules, protected config paths whose edits need approval in `auto` mode even when an allow rule matches, and a TUI approval prompt for gated calls that can also allow matching calls for the rest of the session
- Turn interruption (Esc / Ctrl+C) plus mid-turn queued follow-up prompts that splice into the same turn between tool calls, with double-press Ctrl+C exit confirmation
- Slash commands with `/`-triggered autocomplete (name and curated-arg modes): `/archive`, `/clear`, `/compact`, `/config`, `/delete`, `/diff`, `/effort`, `/help`, `/init`, `/login`, `/logout`, `/model`, `/rename`, `/resume`, `/status`, `/theme`, `/unarchive`
- Context compression with manual `/compact` and default-on auto-compaction
- `CLAUDE.md` / `AGENTS.md` instruction-file discovery
- Session persistence with JSONL conversation logs, AI-generated titles, listing, archiving, and resume
- Per-session file-change tracking with a Read-before-Edit gate and on-disk drift detection
- Authentication (Anthropic API key, browser sign-in with `/login`, Claude Code login or long-lived OAuth token, LLM gateway bearer token with custom headers) and layered TOML config

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

Releases follow the [versioning policy](RELEASING.md#versioning), and stable releases keep the [stability surface](RELEASING.md#stability-surface) compatible within a minor version.

## Installation

Four install paths are supported: prebuilt tarball, Homebrew tap, `cargo install`, and Nix flake. See [Installation](docs/guide/install.md) for the full matrix. The most common one-liners:

```bash
# macOS aarch64 (Apple Silicon). Set VERSION to the newest tag on the Releases page.
VERSION=v0.1.0-alpha.2
curl -fsSL "https://github.com/hakula139/oxide-code/releases/download/$VERSION/oxide-code-aarch64-apple-darwin.tar.gz" | tar -xz
sudo mv ox /usr/local/bin/

# Homebrew (Apple Silicon macOS, x86_64 Linux)
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

[`AGENTS.md`](AGENTS.md) is the contributor entry point for people and coding agents alike. It covers the verification checks and links the [conventions](docs/conventions/) for Rust, testing, writing, GitHub, and the development environment.

## License

Copyright (c) 2026 [Hakula](https://hakula.xyz). Licensed under the [MIT License](LICENSE).
