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
  - **Context compression**: "The system will automatically compress prior messages as it approaches context limits".
  - **Security testing restrictions**: CTF / pentesting / DoS guardrails (re-evaluate when expanding user base).

### Terminal UI

- ratatui + crossterm TUI with `tokio::select!` event loop, 60 FPS render coalescing, and synchronized output (DEC 2026) for flicker prevention.
- `AgentSink` trait decouples the agent loop from display — same code drives TUI (`ChannelSink`), bare REPL (`--no-tui`, `StdioSink`), and headless mode (`-p`).
- Component architecture: `ChatView` (scrollable message list), `InputArea` (multi-line textarea), `StatusBar` (model + spinner + status + cwd).
- Catppuccin Mocha theme with transparent background. Extensible `Theme` struct with role-specific style helpers (text, headings, code, links, blockquotes, list markers, tool borders, thinking, semantic accents). User messages use Peach, assistant messages use Lavender for clear visual distinction.
- Markdown rendering for assistant messages via pulldown-cmark + syntect, fully themed through `Theme` — no hardcoded colors. Supports headings, inline styles, code blocks (syntax-highlighted), lists (ordered / unordered / nested), blockquotes, links, horizontal rules, and tables (box-drawing borders with column alignment). Long paragraph / heading lines wrap to terminal width at block boundaries. Streaming-aware line-based commit boundary and stable-prefix cache for O(new lines) per-token cost.
- Compact icon-on-bar prefixes (`❯ ▎` / `⟡ ▎`) distinguish user and assistant messages without full-line role labels.
- Tool call display with per-tool icons (`$ → ← ✎ ✱ ⌕`), styled left borders, success / error result indicators, and truncated output body (5 lines with overflow count).
- Extended thinking display — dimmed italic block, respects `show_thinking` config, clears on stream start.
- Multi-line input with `ratatui-textarea`: dynamic height (1–6 lines), Shift+Enter for newline, placeholder text. Visual line count estimation via `unicode-width` for correct height under word-wrap. Viewport-relative cursor positioning via tracked scroll offset.
- Marketing model name in status bar (e.g., "Claude Opus 4.7" instead of raw API ID). Braille spinner animation (~80 ms per frame) during streaming and tool execution, starts immediately on prompt submit.
- Right-aligned working directory in status bar with `~/` home prefix.
- Empty-state welcome screen.
- Alternate screen, panic-safe terminal restore.

### Session Persistence

- JSONL-based conversation logs — append-only, one entry per line, immediate flush. Forward-compatible entry types: `header` (session metadata with format `version`), `message` (UUID + `parent_uuid` chain for future forking / partial replay), `title` (re-appendable, with `source`: `first_prompt` / `ai_generated` / `user_provided`), `summary` (exit marker with message count), and an `Unknown` catch-all so new variants land additively.
- Project-scoped storage at `$XDG_DATA_HOME/ox/sessions/{project}/`, where `{project}` is a filesystem-safe subdirectory name derived from the working directory. One-time migration on startup moves any flat-layout or unprefixed files into place. Files are `{unix_timestamp}-{uuid}.jsonl`.
- Session resume via `ox -c` (most recent in current project), `ox -c <id-prefix>` (specific session), or `ox -c <path.jsonl>` (external file path — useful for sessions migrated between machines). `--all` / `-a` widens `--list` and `--continue` across every project; resume by session ID also falls back to other projects automatically. Fork-friendly concurrency — two processes resuming the same session both get append handles immediately; on the next load, the UUID DAG picks the newest-timestamped leaf and walks back via `parent_uuid` to reconstruct a linear chain, so losing fork branches stay in the file for audit but are invisible to later resumes. Matches claude-code's `--fork-session` model.
- Session listing via `ox --list` / `ox -l` — reads the header (line 1) and streams the rest of the file for the latest re-appended `Entry::Title` and `Entry::Summary`. Sorted by file mtime (most recently active first) so resumed sessions bubble to the top. Shows session ID prefix, last-active time (local), message count, and title.
- AI-generated session titles — on the first user prompt of a fresh session, a detached tokio task asks `claude-haiku-4-5` for a concise 3-7 word sentence-case title (via the `structured-outputs-2025-12-15` beta with a `{"title": string}` JSON schema) and appends it as a new `Entry::Title { source: AiGenerated }`. The latest-`updated_at` title wins on listing, so the AI title supersedes the first-prompt fallback automatically; the TUI status bar refreshes live via `AgentEvent::SessionTitleUpdated`. Failures warn-log only — the first-prompt title stays intact.
- Resume sanitization on load: strips trailing `thinking`, drops unresolved assistant `tool_use` blocks and orphan user `tool_result` blocks (both halves of a crashed tool turn), drops empty messages, merges any adjacent same-role survivors, and injects synthetic user / assistant sentinels at the head or tail when the transcript would otherwise start with assistant or end with an orphan-only user turn — keeps the transcript API-valid after mid-turn crashes or JSONL corruption.
- Resumed conversation history displayed in the TUI chat view with full fidelity — text, tool calls paired with their results via a per-load `tool_use_id` → label map, and thinking blocks (gated by the `show_thinking` config). `RedactedThinking` blocks are always dropped. The resumed title (first-prompt or AI-generated, whichever has the newer `updated_at`) surfaces in the TUI status bar between model and status.
- On Unix, session files are created with mode `0o600` so verbatim tool output (which may include secrets) stays owner-only.
- Works across all modes (TUI, bare REPL, headless). Session ID flows through to the `x-claude-code-session-id` API header. AI title generation runs in TUI only — REPL / headless keep the first-prompt title on disk so listings stay accurate.

### Testing

- 97%+ line coverage (measured via `cargo llvm-cov --ignore-filename-regex 'main\.rs'`).
- `wiremock` for HTTP round-trip coverage of the Anthropic streaming client, the non-streaming completion path, the OAuth token-refresh flow, and `session::title_generator::generate_and_record`.
- `ratatui::backend::TestBackend` + `insta` snapshots for `InputArea`, `ChatView`, `StatusBar`, and `App::draw_frame` at representative sizes and states. Review updates via `cargo insta review`.
- `temp-env` for the `Config::load` precedence matrix (env > user config > defaults), `util::env::{string, bool}` empty-is-absent semantics, and `SessionStore::open` XDG / HOME resolution.
- An `AgentClient` trait with an in-process fake drives `agent_turn` end-to-end: happy path, multi-round tool dispatch, unknown-tool recovery, `MAX_TOOL_ROUNDS` safety cap, and mid-stream error propagation.
- Parameterized Tool-trait-contract tests assert every tool's `name`, `description`, `input_schema`, `icon`, and `summarize_input` uniformly.

## Current Focus

### Terminal UI (Remaining)

- Viewport virtualization for long conversations.

### Test Coverage (Remaining)

- `StdioSink::send` formatting tests — extract per-variant formatting into a testable helper, then unit-test ANSI escapes, title display, and trimmed output.
- `App::run` event loop — requires a full terminal / crossterm mock; deferred as low value-per-effort (the reducer methods it drives are all independently covered).

### Tool & Prompt Enhancements

- Centralized output truncation — move truncation from individual tools into the tool dispatch layer. Enables consistent behavior and large-output persistence to disk.
- File-change tracking — track read files and their modification times. Return a stub on re-read when content hasn't changed (saves tokens). Enable read-before-write guards to prevent blind overwrites.
- Configurable instruction directories — allow users to specify additional directories to scan for instruction files (e.g., `.codex/`, `.opencode/`) beyond the hardcoded `.claude/`.

### Context Compression

- Context compression — summarize older messages when approaching the context limit. Preserve critical context (task state, modified files, decisions).

## Later

### Slash Commands

Interactive commands typed in the REPL / TUI input, processed locally before reaching the model. Requires a command parser, registry, and per-command handlers.

**Session**: `/resume` (resume a previous session from within the REPL), `/compact` (trigger context compression), `/clear` (reset conversation history).

**Info**: `/help` (list available commands), `/cost` (token usage and cost breakdown), `/status` (session info, model, context usage).

**Config**: `/model` (switch model mid-session), `/config` (view / modify settings).

**Workflow / Skills**: `/init` (create / update CLAUDE.md), `/review` (review code changes), `/commit` (stage and commit). These are user-extensible "skills" — slash commands backed by prompt templates, not hardcoded handlers.

**Auth**: `/login`, `/logout` (manage API credentials).

### Agent Infrastructure

- Task management (task board, dependency tracking).
- Plan mode with approval workflow.
- Subagent spawning with isolated context.
- Background task execution.
- Agent team coordination with message passing.
- Git worktree isolation for parallel agent work.

### Platform

- MCP client and server support.
- Permission system and sandbox execution (re-add prompt guidance when implemented).

## Not the Goal Right Now

- Multi-provider LLM support (Anthropic only to start).
- IDE integration or GUI.
- Plugin system beyond MCP.
- Feature parity with Claude Code — focus on the core workflow first.
