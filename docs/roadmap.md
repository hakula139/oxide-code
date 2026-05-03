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

- Built-in: `/clear` (aliases `/new`, `/reset`), `/config`, `/diff`, `/help`, `/init`, `/model`, `/status`. See the [user guide](guide/slash-commands.md).
- Autocomplete popup on typing `/`, with ranked filter and Tab completion.
- `/model` swaps the active model mid-session via three-tier resolution (alias `opus` / `sonnet` / `haiku`, exact id, unique substring against a curated 5-row set). `[1m]` is first-class for the 1M-context variants of Opus 4.7 and Sonnet 4.6. Effort re-clamps to the new model's ceiling and the confirmation surfaces the change explicitly (`clamped from xhigh`, `effort cleared`). Session-only; restart returns to the user-declared model.
- Read-only by design — no slash command writes user config files; runtime mutations stay session-local.

### Authentication & Configuration

- Anthropic API key via `ANTHROPIC_API_KEY` or config file.
- Claude Code OAuth credentials picked up automatically (macOS Keychain, Linux file).
- TOML config with layered precedence: defaults → user (`~/.config/ox/config.toml`) → project (`ox.toml`) → environment.

## Current Focus

### Slash Commands (continuation)

The first wave (`/clear`, `/config`, `/diff`, `/help`, `/init`, `/model`, `/status`) plus the autocomplete popup ship under Working Today. Remaining surface:

- Session: `/resume`.
- Mid-session swap: `/theme`.
- `/model` interactive picker — Claude Code-style modal with arrow-key model navigation and `← →` effort adjustment. Follow-up PR; the textual list view ships under Working Today. Needs new key routing (modal-mode flag), a chat-anchored interactive `ChatBlock`, and effort-adjuster state plumbing.
- Deferred: `/compact` (summarization), `/cost` (token persistence), `/login` / `/logout` (interactive OAuth), custom user commands (templates), `/init` multi-phase flow (`AgentEvent::PromptRequest`), lossless effort across `/model` swap-backs, argument-aware popup completion (`SlashCommand::complete(args_partial)` hook).

Persistence stance: `/model` and `/theme` mutate runtime state for the current session only; restart returns to the user-declared config. Persisting a slash-command choice across restarts is intentionally deferred until there is a clear case for it. When the case arrives, the design will be an **explicit subcommand** writing to an **explicit user-opted-in path** (e.g. `/model save claude-sonnet-4-6` writing into `~/.config/ox/config.toml.local` or similar) — never a silent merge into the user's main config file. This rejects Claude Code's `~/.claude.json` mega-file pattern (telemetry, recent files, login state, per-project state all in one silently-written blob); a single corrupt write should never erase the user's preferences, and a NixOS-style declarative config should remain valid.

### Permission & Approval

- Per-tool approval prompts before destructive actions (bash, write, edit).
- Project-level allowlists to auto-approve trusted commands.
- Plan mode: read-only review of the agent's proposed changes before any tool runs.

### Context Compression

- Summarize older messages when approaching the context limit so long sessions keep responding.

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
