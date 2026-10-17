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

- **Terminal UI**: streaming Markdown, syntax-highlighted code blocks, a [configurable status line](docs/guide/configuration.md#tui-terminal-ui), and 5 built-in [themes](docs/guide/theming.md) with custom TOML overrides
- **[Headless mode](docs/guide/headless.md)**: one-shot `-p` runs for scripts and pipelines, with output as text, a JSON result, or a JSON event stream, plus a bare REPL (`--no-tui`)
- **Models**: Claude Opus 5.5, Sonnet 5.5, and Haiku 5.5, each with a 1M-token context window, adjustable effort, and extended thinking
- **Tools**: `read`, `write`, `edit`, `glob`, `grep`, and `bash`, which can also keep dev servers, watchers, and long builds running in the background
- **Task list**: the model tracks multi-step work in a list shown live and carried through resume and compaction
- **[Tool permissions](docs/guide/configuration.md#permission-tool-approval)**: `auto` (default), `plan`, and `yolo` modes, allow / deny rules for each command in a `bash` command line, protected config paths that always ask in `auto` mode, and an approval prompt that can allow matching calls for the session
- **Turn control**: interrupt with Esc or Ctrl+C, queue follow-up prompts that join the running turn between tool calls, and confirm exit with a second Ctrl+C
- **[Slash commands](docs/guide/slash-commands.md)**: commands for sessions, models and effort, compaction, sign-in, themes, `/init`, and status and config views, with autocomplete
- **Context compaction**: manual `/compact` and default-on auto-compaction
- **[Instruction files](docs/guide/instructions.md)**: `CLAUDE.md` / `AGENTS.md` discovery
- **[Sessions](docs/guide/sessions.md)**: JSONL logs with AI-generated titles, listing, archiving, and resume
- **File-change tracking**: a Read-before-Edit gate and on-disk drift detection
- **[Authentication](docs/guide/configuration.md#authentication)**: an Anthropic API key, browser sign-in with `/login`, the Claude Code login or a long-lived OAuth token, or an LLM gateway bearer token with custom headers
- **[Configuration](docs/guide/configuration.md)**: layered TOML from user, drop-in, and project files, with environment variable overrides

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

[Installation](docs/guide/install.md) covers every platform and method, plus updating and uninstalling. The most common commands:

```bash
# macOS aarch64 (Apple Silicon). Set VERSION to the newest tag on the Releases page.
VERSION=v0.1.0-alpha.3
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

The [user guide](docs/guide/) covers installation, a quickstart, configuration, slash commands, instruction files, sessions, headless mode, and theming.

## Development

[`AGENTS.md`](AGENTS.md) is the contributor entry point for people and coding agents alike. It covers the verification checks and links the [conventions](docs/conventions/) for Rust, testing, writing, GitHub, and the development environment.

## License

Copyright (c) 2026 [Hakula](https://hakula.xyz). Licensed under the [MIT License](LICENSE).
