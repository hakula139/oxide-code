# Roadmap

oxide-code is still early. This roadmap is the high-level product view: it should show what works, what is being built next, and what is intentionally out of scope for now.

The project direction is simple:

- Build a useful terminal-based AI coding assistant in Rust.
- Follow the agent-harness architecture: the model is the agent, everything else is harness (tools, context, permissions, coordination).
- Keep the architecture understandable. New features should fit the current model instead of forcing large abstractions too early.

## Working Today

### Agent Loop

- Async REPL that reads user input and streams responses from the Anthropic Messages API.
- Agent loop: the LLM can request tool execution, results feed back into the conversation, looping until a text-only response.
- Extended thinking — full streaming pipeline for `thinking`, `redacted_thinking`, `server_tool_use`, and signature handling with round-trip preservation. Unrecognized future content block types are silently skipped. Optional dimmed thinking display (`OX_SHOW_THINKING`).

### Authentication & Configuration

- TOML config file with layered loading: built-in defaults → user config (`~/.config/ox/config.toml`, respects `$XDG_CONFIG_HOME`) → project config (`ox.toml`, walks CWD upward) → env var overrides. Sectioned layout (`[client]`, `[tui]`) for forward compatibility.
- All configurable values (`api_key`, `model`, `base_url`, `max_tokens`, `show_thinking`) settable in config files with env vars still taking precedence.
- OAuth authentication via Claude Code credentials — reads from macOS Keychain (`"Claude Code-credentials"` service) and `~/.claude/.credentials.json`, preferring whichever has the later expiry. Keychain access via `security-framework` crate (macOS-only). Falls back to file-only on Linux.
- API key authentication via `ANTHROPIC_API_KEY` environment variable or `api_key` config key.

### Tools

- Bash — execute shell commands with timeout, head+tail output truncation, and structured metadata (exit code, description).
- File — read (line-numbered output, pagination, byte budget), write (with directory creation), edit (exact string replacement with CRLF handling).
- Search — glob-based file pattern matching, regex content search with output modes (content / files / count), context lines, and head limit.
- Tool definitions sent via the Anthropic `tools` API parameter.
- Tool output with structured metadata — title and tool-specific fields for TUI rendering, separate from model-facing content.

### System Prompt

- Section-based system prompt builder with static / dynamic cache boundary: identity (OAuth-required prefix), system, task guidance, caution, tool usage, tone / style, output efficiency (static); environment (dynamic).
- Two-channel context injection: static sections go in the API `system` parameter with `cache_control`; dynamic content (CLAUDE.md, date) goes in a synthetic `messages[0]` user message wrapped in `<system-reminder>`.
- CLAUDE.md / AGENTS.md discovery and injection — user global (`~/.claude/`), project root to CWD walk (root-level and `.claude/` at each directory level). Fallback filename: first found wins per location.
- Runtime environment detection — working directory, platform, shell, git status, OS version, date, model marketing name, knowledge cutoff.
- Custom prompt text — trimmed from Claude Code's original to remove references to unimplemented features. Sections to re-add when features ship:
  - **Permission system**: "Tools are executed in a user-selected permission mode" (tool approval / deny flow).
  - **Hooks**: shell commands that execute on tool call events (`<user-prompt-submit-hook>`).
  - **Context compression**: "The system will automatically compress prior messages as it approaches context limits."
  - **Security testing restrictions**: CTF / pentesting / DoS guardrails (re-evaluate when expanding user base).

### Terminal UI

- ratatui + crossterm TUI with `tokio::select!` event loop, 60 FPS render coalescing, and synchronized output (DEC 2026) for flicker prevention.
- `AgentSink` trait decouples the agent loop from display — same code drives TUI (`ChannelSink`), bare REPL (`--no-tui`, `StdioSink`), and headless mode (`-p`).
- Component architecture: `ChatView` (scrollable message list), `InputArea` (multi-line textarea), `StatusBar` (model + spinner + status + cwd).
- Catppuccin Mocha theme with transparent background. Extensible `Theme` struct with role-specific style helpers (text, tool borders, thinking, semantic accents).
- Markdown rendering for assistant messages via pulldown-cmark + syntect, with streaming-aware line-based commit boundary for partial render during streaming.
- Tool call display with per-tool icons (`$ → ← ✎ ✱ ⌕`), styled left borders, and success / error result indicators.
- Extended thinking display — dimmed italic block, respects `show_thinking` config, clears on stream start.
- Multi-line input with `ratatui-textarea`: dynamic height (1–6 lines), Shift+Enter for newline, placeholder text.
- Braille spinner animation (~80 ms per frame) during streaming and tool execution.
- Right-aligned working directory in status bar with `~/` home prefix.
- Empty-state welcome screen.
- Alternate screen, panic-safe terminal restore.

## Current Focus

### Terminal UI (Remaining)

- Viewport virtualization for long conversations.

### Tool & Prompt Enhancements

- Centralized output truncation — move truncation from individual tools into the tool dispatch layer. Enables consistent behavior and large-output persistence to disk.
- File-change tracking — track read files and their modification times. Return a stub on re-read when content hasn't changed (saves tokens). Enable read-before-write guards to prevent blind overwrites.
- Configurable instruction directories — allow users to specify additional directories to scan for instruction files (e.g., `.codex/`, `.opencode/`) beyond the hardcoded `.claude/`.

### Session Persistence & Context

- JSONL-based conversation logs for session resume.
- Session listing and management.
- Context compression — summarize older messages when approaching the context limit. Preserve critical context (task state, modified files, decisions).

## Later

- Task management (task board, dependency tracking).
- Plan mode with approval workflow.
- Subagent spawning with isolated context.
- Background task execution.
- Agent team coordination with message passing.
- Git worktree isolation for parallel agent work.
- MCP client and server support.
- Permission system and sandbox execution (re-add prompt guidance when implemented).

## Not the Goal Right Now

- Multi-provider LLM support (Anthropic only to start).
- IDE integration or GUI.
- Plugin system beyond MCP.
- Feature parity with Claude Code — focus on the core workflow first.
