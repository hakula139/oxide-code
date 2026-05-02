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

- `/help`, `/diff`, `/status`, `/config`, `/clear`, `/init` — every command is a one-file `SlashCommand` impl in `slash/`, dispatched locally before reaching the agent loop, output as a `SystemMessageBlock` (or, for `/init`, forwarded back to the agent as a synthesized prompt).
- `/clear` (aliases `/new`, `/reset`) rolls the session UUID: finalizes the current JSONL, opens a fresh one, drops the in-memory message history and file-tracker state, points the API client at the new id, and clears the AI title. The old session stays resumable via `ox -c <old-id>`. State-mutating commands forward through `SlashContext.user_tx`; the agent loop owns the lifecycle. Title-generator events carry their session id so a slow Haiku call straddling `/clear` doesn't paint the old title onto the fresh session.
- `/init` synthesizes a fixed prompt asking the model to author or update the project's `AGENTS.md` / `CLAUDE.md`, then forwards it to the agent loop as the next user turn. The user sees only the typed `/init` line; the wall-of-text body is invisible in the live session. Three-kinds outcome enum (`SlashOutcome { Local, PromptSubmit(String) }`) on `SlashCommand::execute` carries the body back to the dispatcher — no ad-hoc side channels.
- Autocomplete popup: typing `/` opens a two-column overlay above the input with name + description rows; Up / Down navigate, Tab completes `/{name}` plus a trailing space, Enter submits, Esc dismisses. Selected row paints normal-bold; the rest are dim. Ranks name-prefix > alias-prefix > name-substring > alias-substring; aliases parenthesize only the alias the user typed.
- Names accept ASCII letters / digits plus `_`, `-`, `:`, `.` so a future plugin-namespace layer (e.g. `/plugin:cmd`) doesn't need a parser rewrite.
- Aliases display inline in `/help` (`/clear (new, reset)` shape); typing any alias routes to the canonical impl.
- Read-only by design: no slash command writes to user config files. Mutations to runtime state (`/model`, `/theme`) will be session-local on a NixOS-style declarative setup, restart returns to the user-declared values.

### Authentication & Configuration

- Anthropic API key via `ANTHROPIC_API_KEY` or config file.
- Claude Code OAuth credentials picked up automatically (macOS Keychain, Linux file).
- TOML config with layered precedence: defaults → user (`~/.config/ox/config.toml`) → project (`ox.toml`) → environment.

## Current Focus

### Slash Commands (continuation)

The first wave (`/help`, `/diff`, `/status`, `/config`, `/clear`, `/init`) plus the autocomplete popup ship under Working Today. Remaining surface:

- Session: `/resume`.
- Mid-session swap: `/model`, `/theme`.
- Deferred: `/compact` (needs a summarization call we don't have), `/cost` (needs token persistence we don't have), `/login` / `/logout` (interactive OAuth), custom user commands (markdown templates), `/init`'s multi-phase interactive flow (needs `AgentEvent::PromptRequest` plumbing — cleanup-follow-ups item 7).

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

- User-extensible commands backed by prompt templates: `/init`, `/review`, `/commit`.
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
