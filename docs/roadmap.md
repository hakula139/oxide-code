# Roadmap

oxide-code is still early. This roadmap is the high-level product view: what works, what's being built next, and what is intentionally out of scope for now.

The direction is simple:

- Build a useful terminal-based AI coding assistant in Rust.
- Follow the agent-harness architecture: the model is the agent, everything else is harness (tools, context, permissions, coordination).
- Keep the architecture understandable. New features should fit the current model instead of forcing large abstractions too early.

## Working Today

### Terminal UI

- Streaming output with markdown rendering and syntax-highlighted code blocks.
- Multi-line input with a prompt marker, dynamic placeholder hints, and a status bar showing model, working directory, and run state.
- Rich per-tool views: edit diffs with line gutters, line-numbered read excerpts, grouped grep matches, structured glob lists, and bash output.
- Themable via runtime-loaded TOML — 5 built-in palettes (Catppuccin Mocha, Macchiato, Frappe, Latte, Material) with per-slot overrides.
- Three modes: full TUI, bare REPL (`--no-tui`), and headless (`-p`).

### Agent Loop

- Async streaming from the Anthropic Messages API.
- Tool-use round-trip: the model calls tools, results feed back, and the loop continues until a text-only response.
- Extended thinking with optional dimmed display.

### Tools

| Tool    | Purpose                         |
| ------- | ------------------------------- |
| `bash`  | Run shell commands with timeout |
| `read`  | Read files with line numbers    |
| `write` | Create or overwrite files       |
| `edit`  | Replace exact strings in files  |
| `glob`  | Find files by pattern           |
| `grep`  | Search file contents with regex |

### Turn Interruption & Queueing

- Esc / Ctrl+C while busy interrupts the in-flight turn; partial output is preserved with a clear `(interrupted)` marker.
- Type during a busy turn to queue prompts; queued prompts splice into the same multi-step turn at the next round boundary (between tool calls), so follow-ups land without aborting in-flight work. Tool-less turns drain queued prompts at the turn boundary instead.
- Esc on idle pops the most recent queued prompt back into the input for editing.
- Idle Ctrl+C arms a 1-second exit confirmation; a second press confirms.

### System Prompt

- Runtime environment (cwd, platform, shell, git status, date, model) injected every turn.
- `CLAUDE.md` / `AGENTS.md` discovered from user-global and project scopes (root-to-CWD walk, root-level and `.claude/` at each level).

### Session Persistence

- Every conversation saved as JSONL under `$XDG_DATA_HOME/ox/sessions/{project}/`.
- `ox --list` browses recent sessions; `ox -c` resumes by recency, prefix, or path.
- AI-generated 3-7-word titles land shortly after the first prompt.

### File-Change Tracking

- Per-session tracker remembers each Read; unchanged re-reads return a cache-hit stub instead of the full body.
- Edit and Write require a prior full Read and refuse if the on-disk bytes have drifted (xxh64 fallback for cloud-sync mtime touches).
- Tracker state persists into the session JSONL on clean exit and restores on resume.

### Slash Commands

- Built-in: `/clear` (aliases `/new`, `/reset`), `/config`, `/diff`, `/effort`, `/help`, `/init`, `/model`, `/status`. See the [user guide](guide/slash-commands.md).
- Autocomplete popup on typing `/`, with ranked filter and Tab completion.
- Mid-session swap: `/model` and `/effort`. Session-only — no slash command writes user config files.
- Modal UI primitive (focus-grabbing overlays above the input). Bare `/model` opens a combined model + effort picker — Up / Down for models, `← →` for effort, number keys to jump, atomic single-action submit. `/status` opens a read-only kv overview.

### Authentication & Configuration

- Anthropic API key via `ANTHROPIC_API_KEY` or config file.
- Claude Code OAuth credentials picked up automatically (macOS Keychain, Linux file).
- TOML config with layered precedence: defaults → user (`~/.config/ox/config.toml`) → project (`ox.toml`) → environment.

## Current Focus

### Permission & Approval

- Per-tool approval prompts before destructive actions (bash, write, edit).
- Project-level allowlists to auto-approve trusted commands.
- Plan mode: read-only review of the agent's proposed changes before any tool runs.

### Context Compression

- Summarize older messages when approaching the context limit so long sessions keep responding.

### Slash Commands (continuation)

Remaining surface beyond Working Today:

- Session: `/resume`.
- Mid-session swap: `/theme`.
- `/effort` slider — horizontal Speed ←→ Intelligence visual that gives bare `/effort` its own picker without retreading the `/model` modal.
- Inline argument placeholder — dim ghost-text hint (e.g. `[id]`) after a slash command's trailing space.
- Deferred: `/compact`, `/cost`, `/login` / `/logout`, custom user commands, `/init` multi-phase flow, argument-aware popup completion.

Persistence stance: `/model`, `/effort`, and `/theme` mutate session state only; restart returns to user-declared config. Cross-session persistence will land as an **explicit subcommand** writing to an **explicit user-opted-in path** — never a silent merge. (Rejects Claude Code's `~/.claude.json` mega-file pattern.)

### Viewport Virtualization

- Render only the visible chat region for sessions with thousands of blocks.

## Later

### MCP Integration

- MCP client to call external tool servers (Atlassian, GitHub, custom).
- MCP server mode to expose oxide-code as a tool to other agents.

### Agent Infrastructure

- Task management for multi-step work (TodoWrite-style tracking).
- Subagent spawning to delegate self-contained sub-tasks.
- Background tasks for long-running shell processes.
- Agent-team coordination across multiple subagents.
- Git-worktree isolation for parallel implementation attempts.

### Sandboxing

- Sandboxed execution for `bash` / `write` / `edit` so the agent runs without trusting the host shell.

### Workflow Skills

- User-extensible templates that can override built-ins or add new ones (e.g. project-local `~/.claude/commands/review.md`). Built-ins like `/init` ship under Working Today.
- Auth slash commands: `/login`, `/logout`.
- Configurable instruction directories beyond `.claude/`.

### Welcome Screen

- Richer first-impression banner for empty sessions — name + version, active model and effort, working directory, and starter slash commands.
- Pairs with Slash Commands (the welcome is where they get advertised); not blocking anything else.

## Not the Goal Right Now

- Multi-provider LLM support (Anthropic only to start).
- IDE integration or GUI.
- Plugin system beyond MCP.
- Feature parity with Claude Code — focus on the core workflow first.
