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
- Catppuccin Mocha theme, tool-call blocks with per-tool icons and success / error indicators, GitHub-style Edit diffs with line-number gutters, styled Read excerpts, grouped Grep results, structured Glob file lists, multi-line input, and a status bar with model, working directory, and streaming spinner.
- Works in TUI, bare REPL (`--no-tui`), and headless (`-p`) modes, with panic-safe terminal restore.

### Session Persistence

- Every conversation is saved as JSONL under `$XDG_DATA_HOME/ox/sessions/{project}/`.
- `ox --list` shows recent sessions in the current project; `-a` / `--all` widens to every project.
- `ox -c` resumes the most recent session; `ox -c <id-prefix>` picks one by prefix; `ox -c <path.jsonl>` resumes from an external path.
- AI-generated titles (3-7 words) land shortly after the first prompt.

## Current Focus

### Terminal UI

- Viewport virtualization for long conversations after the richer result views and block measurement hooks are in place.
- Runtime-loaded theme files so users can pick a built-in palette or override individual color slots without recompiling.

### Tool & Prompt Enhancements

- Centralized output truncation in the tool dispatcher.
- File-change tracking — skip re-reads when content hasn't changed, and guard against blind overwrites.
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
