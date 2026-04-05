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

- OAuth authentication via Claude Code credentials — reads from macOS Keychain (`"Claude Code-credentials"` service) and `~/.claude/.credentials.json`, preferring whichever has the later expiry. Keychain access via `security-framework` crate (macOS-only). Falls back to file-only on Linux.
- API key authentication via `ANTHROPIC_API_KEY` environment variable.
- Configurable model, base URL, and max tokens via environment variables.

### Tools

- Bash — execute shell commands with timeout, head+tail output truncation, and structured metadata (exit code, description).
- File — read (line-numbered output, pagination, byte budget), write (with directory creation), edit (exact string replacement with CRLF handling).
- Search — glob-based file pattern matching, regex content search with output modes (content / files / count), context lines, and head limit.
- Tool definitions sent via the Anthropic `tools` API parameter.
- Tool output with structured metadata — title and tool-specific fields for TUI rendering, separate from model-facing content.

### System Prompt

- Section-based system prompt builder: identity (OAuth-required prefix), task guidance, tool usage guidance, tone / style.
- CLAUDE.md / AGENTS.md discovery and injection — user global (`~/.claude/`), project root to CWD walk (root-level and `.claude/` at each directory level). Fallback filename: first found wins per location.
- Runtime environment detection — working directory, platform, shell, git info (branch, clean / dirty status), date, model name.

## Current Focus

### Terminal UI

- Replace the bare REPL with a ratatui-based TUI.
- Real-time streaming display of assistant responses.
- Inline tool call / result display using `ToolMetadata::title`.
- Multi-line input editor.

### Configuration File

- TOML config file (`~/.config/ox/config.toml` or `ox.toml` in project root) to replace env-var-only configuration.
- Layered loading: global defaults → user config → project config → env var overrides.
- All current env vars (`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `OX_SHOW_THINKING`, etc.) become config keys, with env vars still taking precedence.
- Configurable instruction directories — allow users to specify additional directories to scan for instruction files (e.g., `.codex/`, `.opencode/`) beyond the hardcoded `.claude/`.

### Tool Enhancements

- Centralized output truncation — move truncation from individual tools into the tool dispatch layer. Enables consistent behavior and large-output persistence to disk.
- File-change tracking — track read files and their modification times. Return a stub on re-read when content hasn't changed (saves tokens). Enable read-before-write guards to prevent blind overwrites.

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
