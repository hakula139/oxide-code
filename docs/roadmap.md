# Roadmap

oxide-code is still early. This roadmap is the high-level product view: what works, what's being built next, and what is out of scope for now.

The direction is simple:

- Keep the terminal as the primary interface: streaming chat, tool output, and session controls stay keyboard-first.
- Keep context and state visible: model, instructions, compaction, queued prompts, and session identity should be inspectable from the UI.
- Add workflow depth only when it fits the current agent-harness model.

## Working Today

### Terminal UI

- Streaming chat with markdown, syntax-highlighted code, and clear tool output.
- Multi-line input, a configurable status line with context / estimated-cost usage, and a focused welcome screen for new sessions.
- Theme support with built-in palettes and user-defined TOML themes.
- Full TUI, bare REPL (`--no-tui`), and headless (`-p`) modes.

### Agent Loop

- Anthropic-powered streaming turns with tool use and multi-step continuation.
- Optional extended-thinking display for models that support it.

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

- Interrupt busy turns without losing partial output.
- Queue follow-up prompts while the assistant is working, then edit or cancel them from idle.
- Exit intentionally with a guarded Ctrl+C confirmation.

### System Prompt

- Project environment and model context are injected every turn.
- `CLAUDE.md` / `AGENTS.md` instructions are loaded from user and project scopes.

### Session Persistence

- Conversations are saved per project and can be listed or resumed later.
- Mid-session `/resume` switches chats without restarting the app.
- Short AI-generated titles make session history easier to scan.

### File-Change Tracking

- Tracks reads so edits are made against files the assistant has actually seen.
- Refuses stale writes when files changed on disk.
- Restores edit-safety state when a session resumes.

### Slash Commands

- Built-in commands cover session control, config/status, model and theme changes, diffs, compaction, and help. See the [user guide](guide/slash-commands.md).
- Autocomplete, typed shortcuts, and modal pickers keep common actions quick.
- Destructive session actions require confirmation.

### Context Compression

- Manual `/compact [instructions]` and default auto-compaction keep long sessions usable.
- Compaction keeps a visible history boundary and makes future edits require fresh reads.

### Authentication & Configuration

- Supports Anthropic API keys and Claude Code OAuth pickup.
- Layered TOML configuration supports user, project, and environment overrides.

## Current Focus

### Permission & Approval

- Approval prompts for destructive tool actions.
- Project allowlists for trusted commands.
- Plan mode for reviewing the assistant's proposed work before tools run.

### Slash Commands (continuation)

Remaining surface beyond Working Today:

- Login/logout, custom commands, and a guided `/init` flow.

Persistence stance: session commands should feel reversible. Cross-session writes will require an explicit user action.

### Viewport Virtualization

- Keep very long sessions responsive by rendering only the visible chat region.

## Later

### MCP Integration

- MCP client support for external tool servers.
- MCP server mode so other agents can call oxide-code.

### Agent Infrastructure

- Task tracking for multi-step work.
- Subagents for self-contained delegation.
- Background shell processes and stronger parallel-work support.

### Sandboxing

- Sandboxed `bash`, `write`, and `edit` execution.

### Workflow Skills

- User-extensible workflow templates.
- Auth slash commands.
- Configurable instruction directories.

### Status Line Extensions

- Additional segments for queue state, session identity, theme, account-limit usage, pull requests, and task progress.

## Not the Goal Right Now

- Multi-provider LLM support (Anthropic only to start).
- IDE integration or GUI.
- Plugin system beyond MCP.
- Feature parity with Claude Code. Focus on the core workflow first.
