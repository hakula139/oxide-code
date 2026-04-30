# Roadmap

oxide-code is still early. This roadmap is the high-level product view: what works, what's being built next, and what is intentionally out of scope for now.

The direction is simple:

- Build a useful terminal-based AI coding assistant in Rust.
- Follow the agent-harness architecture: the model is the agent, everything else is harness (tools, context, permissions, coordination).
- Keep the architecture understandable. New features should fit the current model instead of forcing large abstractions too early.

## Working Today

### Agent Loop

- Async REPL that streams responses from the Anthropic Messages API.
- Tool-use round-trip: the model calls tools, results feed back, and the loop continues until a text-only response.
- Extended thinking with optional dimmed display.

### Authentication & Configuration

- Anthropic API key via `ANTHROPIC_API_KEY` or config file.
- Claude Code OAuth credentials picked up automatically on macOS (Keychain) and Linux (file).
- TOML config with layered precedence: built-in defaults → user (`~/.config/ox/config.toml`) → project (`ox.toml`) → environment variables.

### Tools

| Tool    | Purpose                         |
| ------- | ------------------------------- |
| `bash`  | Run shell commands with timeout |
| `read`  | Read files with line numbers    |
| `write` | Create or overwrite files       |
| `edit`  | Replace exact strings in files  |
| `glob`  | Find files by pattern           |
| `grep`  | Search file contents with regex |

### System Prompt

- Runtime environment (cwd, platform, shell, git status, date, model) injected every turn.
- `CLAUDE.md` / `AGENTS.md` discovered from user-global and project scopes (root-to-CWD walk, root-level and `.claude/` at each level).

### Terminal UI

- Streaming display with markdown rendering and syntax-highlighted code blocks.
- Runtime-loaded theme files — pick from 5 built-in palettes (Catppuccin Mocha default, plus Macchiato / Frappe / Latte / Material) or point at a user TOML, with per-slot overrides on top. Hex / ANSI named / indexed / `reset` color formats supported.
- Per-tool result views:
  - Tool-call blocks with per-tool icons and success / error indicators.
  - GitHub-style Edit diffs with line-number gutters and red / green row tints.
  - Styled Read excerpts with line-numbered bodies and path / range headers.
  - Grouped Grep results — per-file blocks of line-numbered matches with dim context.
  - Structured Glob file lists with combined TUI / tool truncation footers.
  - Multi-line input area and a status bar with model, working directory, and streaming spinner.
- Works in TUI, bare REPL (`--no-tui`), and headless (`-p`) modes, with panic-safe terminal restore.

### Session Persistence

- Every conversation is saved as JSONL under `$XDG_DATA_HOME/ox/sessions/{project}/`.
- `ox --list` shows recent sessions in the current project; `-a` / `--all` widens to every project.
- `ox -c` resumes the most recent session; `ox -c <id-prefix>` picks one by prefix; `ox -c <path.jsonl>` resumes from an external path.
- AI-generated titles (3-7 words) land shortly after the first prompt.

### File-Change Tracking

- Per-session tracker records every Read so re-reads of unchanged files return a cache-hit stub instead of the full body.
- Edit and Write require a prior full Read of the file and refuse if the on-disk bytes have drifted since (xxh64 fallback for cloud-sync mtime touches).
- Tracker state persists into the session JSONL on clean exit and restores stat-verified entries on resume — no forced re-Read across sessions.

## Current Focus

### Terminal UI

- Turn interruption and queued input — Esc / Ctrl+C cancels an in-flight stream or tool, double-press Ctrl+C exits from idle, typed prompts during streaming queue and fire after the current turn (see [Cancellation and Queued Input](research/design/cancellation-and-queued-input.md)).
- Viewport virtualization for long conversations after the richer result views and block measurement hooks are in place.

### Tool & Prompt Enhancements

- Configurable instruction directories beyond `.claude/`.

### Context Compression

- Summarize older messages when approaching the context limit.

## Later

### Slash Commands

Interactive commands typed in the REPL / TUI, processed locally before reaching the model.

- **Session:** `/resume`, `/compact`, `/clear`.
- **Info:** `/help`, `/cost`, `/status`.
- **Config:** `/model`, `/config`.
- **Workflow:** `/init`, `/review`, `/commit` — user-extensible skills backed by prompt templates.
- **Auth:** `/login`, `/logout`.

### Agent Infrastructure

- Task management, plan-mode approval, subagent spawning, background tasks, agent-team coordination, git-worktree isolation.

### Platform

- MCP client and server support.
- Permission system and sandbox execution.

## Not the Goal Right Now

- Multi-provider LLM support (Anthropic only to start).
- IDE integration or GUI.
- Plugin system beyond MCP.
- Feature parity with Claude Code — focus on the core workflow first.
