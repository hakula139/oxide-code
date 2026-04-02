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

## Current Focus

### Core Agent Loop

- REPL that reads user input and sends it to an LLM.
- Streaming responses from the Anthropic Messages API.
- Tool dispatch: the LLM can request tool execution, and results feed back into the conversation.
- Bash tool as the first tool — execute shell commands with timeout and output capture.

### Basic File Tools

- Read, write, and edit files.
- Glob-based file search and regex content search.

### System Prompt

- Tool definitions in Anthropic tool-use format.
- Project context injection (CLAUDE.md files).
- Conversation history with token budget management.

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
