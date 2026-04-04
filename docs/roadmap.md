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
- Extended thinking — full streaming pipeline for `thinking`, `redacted_thinking`, `server_tool_use`, and signature handling with round-trip preservation. Unrecognized future content block types are silently skipped.

### Authentication & Configuration

- OAuth authentication via Claude Code credentials (`~/.claude/.credentials.json`).
- API key authentication via `ANTHROPIC_API_KEY` environment variable.
- Configurable model, base URL, and max tokens via environment variables.

### Tools

- Bash — execute shell commands with timeout, head+tail output truncation, and structured metadata (exit code, description).
- File — read (line-numbered output, pagination, byte budget), write (with directory creation), edit (exact string replacement with CRLF handling).
- Search — glob-based file pattern matching, regex content search with output modes (content / files / count), context lines, and head limit.
- Tool definitions sent via the Anthropic `tools` API parameter.
- Tool output with structured metadata — title and tool-specific fields for TUI rendering, separate from model-facing content.

## Current Focus

### macOS Keychain OAuth

- Read OAuth tokens from macOS Keychain (`"Claude Code-credentials"` service) instead of only `~/.claude/.credentials.json`.
- Claude Code uses Keychain as the primary store on macOS; the file is a fallback. The two can diverge, causing stale-token errors.
- Write refreshed tokens back to both Keychain and file.
- See `.claude/plans/macos-keychain-oauth.md` for full design.

### System Prompt

- System prompt construction with tool definitions and project context.
- Load and inject `CLAUDE.md` files (global + project).
- Conversation context management (token counting, message history).

## Next Phase

### Terminal UI

- Replace the bare REPL with a ratatui-based TUI.
- Real-time streaming display of assistant responses.
- Inline tool call / result display using `ToolMetadata::title`.
- Multi-line input editor.

### Tool Enhancements

- Centralized output truncation — move truncation from individual tools into the tool dispatch layer. Enables consistent behavior and large-output persistence to disk.
- File-change tracking — track read files and their mtimes. Return a stub on re-read when content hasn't changed (saves tokens). Enable read-before-write guards to prevent blind overwrites.

### Session Persistence

- JSONL-based conversation logs for session resume.
- Session listing and management.

### Context Compression

- Summarize older messages when approaching the context limit.
- Preserve critical context (task state, modified files, decisions).

## Later

- Task management (task board, dependency tracking).
- Plan mode with approval workflow.
- Subagent spawning with isolated context.
- Background task execution.
- Agent team coordination with message passing.
- Git worktree isolation for parallel agent work.
- MCP client and server support.
- Permission system and sandbox execution.

## Not the Goal Right Now

- Multi-provider LLM support (Anthropic only to start).
- IDE integration or GUI.
- Plugin system beyond MCP.
- Feature parity with Claude Code — focus on the core workflow first.
