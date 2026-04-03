# Roadmap

oxide-code is still early. This roadmap is the high-level product view: it should show what works, what is being built next, and what is intentionally out of scope for now.

The project direction is simple:

- Build a useful terminal-based AI coding assistant in Rust.
- Follow the agent-harness architecture: the model is the agent, everything else is harness (tools, context, permissions, coordination).
- Keep the architecture understandable. New features should fit the current model instead of forcing large abstractions too early.

## Working Today

- Async REPL that reads user input and streams responses from the Anthropic Messages API.
- OAuth authentication via Claude Code credentials (`~/.claude/.credentials.json`).
- API key authentication via `ANTHROPIC_API_KEY` environment variable.
- Configurable model, base URL, and max tokens via environment variables.
- Agent loop: the LLM can request tool execution, results feed back into the conversation, looping until a text-only response.
- Bash tool — execute shell commands with timeout and head+tail output truncation.
- File tools — read (line-numbered output, pagination, byte budget), write (with directory creation), edit (exact string replacement with CRLF handling).
- Search tools — glob-based file pattern matching, regex content search with output modes (content / files / count), context lines, and head limit.
- Tool definitions sent via the Anthropic `tools` API parameter.

## Current Focus

### Streaming Robustness

- Handle unknown content block types (`thinking`, `redacted_thinking`, `signature_delta`, etc.) gracefully instead of crashing on deserialization.
- Required before enabling extended thinking support.

### System Prompt

- System prompt construction with tool definitions and project context.
- Load and inject `CLAUDE.md` files (global + project).
- Conversation context management (token counting, message history).

## Next Phase

### Terminal UI

- Replace the bare REPL with a ratatui-based TUI.
- Real-time streaming display of assistant responses.
- Inline tool call / result display.
- Multi-line input editor.

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
