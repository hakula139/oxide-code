# System Prompt Architecture

Research notes on how Claude Code and opencode construct their system prompts. Based on [`claude-code`](https://github.com/hakula139/claude-code) (v2.1.87) and [`opencode`](https://github.com/anomalyco/opencode).

## Section-Based Assembly

Claude Code builds the system prompt from **sections** — discrete units with lazy, memoized resolution. Sections are split into two categories by a `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker:

- **Static sections** (before the boundary): identity, system guidance, task guidance, tool usage, tone / style. Globally cacheable via prompt caching.
- **Dynamic sections** (after the boundary): session-specific guidance, CLAUDE.md memory, environment info, MCP instructions, language preference, output style, token budget. Not cacheable.

Resolution pipeline:

1. `getSystemPrompt()` collects section definitions (static + dynamic).
2. Static sections resolve immediately; dynamic sections are promises with memoization.
3. `resolveSystemPromptSections()` awaits all section promises.
4. `buildEffectiveSystemPrompt()` applies priority logic — override > coordinator > agent > custom > default.
5. `splitSysPromptPrefix()` splits by cache boundaries and assigns `cacheScope` (`global` / `org` / `null`).
6. `buildSystemPromptBlocks()` wraps in `TextBlockParam` with `cache_control` for the API.

Each block carries a `cacheScope` so the API can reuse cached prefixes across sessions.

## CLAUDE.md Loading Hierarchy

Files are loaded in priority order (latest = highest priority):

| Order | Type    | Path                                  | Description                           |
| ----- | ------- | ------------------------------------- | ------------------------------------- |
| 1     | Managed | `/etc/claude-code/CLAUDE.md`          | Organization-level instructions       |
| 2     | User    | `~/.claude/CLAUDE.md`                 | User's global instructions            |
| 3     | Project | `CLAUDE.md`, `.claude/CLAUDE.md`      | Checked-in project instructions       |
| 4     | Rules   | `.claude/rules/*.md`                  | Conditional rules with path globs     |
| 5     | Local   | `CLAUDE.local.md`                     | Private project-specific (gitignored) |
| 6     | AutoMem | `~/.claude/projects/<slug>/MEMORY.md` | Auto-accumulated memory               |

Project files (Order 3) are discovered by walking from the git root down to CWD, checking at each intermediate directory. For a CWD of `/repo/crates/core`:

```text
/repo/CLAUDE.md              /repo/.claude/CLAUDE.md
/repo/crates/CLAUDE.md       /repo/crates/.claude/CLAUDE.md
/repo/crates/core/CLAUDE.md  /repo/crates/core/.claude/CLAUDE.md
```

This walk ensures subdirectory-specific instructions appear later (higher priority) than root-level ones.

Features:

- **`@include` directives**: `@./relative/path`, `@~/home`, `@/absolute` — recursive include with max depth 5.
- **Conditional rules**: `.md` files with `paths:` frontmatter — glob-matched to decide inclusion.
- **HTML comment stripping**: Block-level only, via marked lexer.
- **MEMORY.md truncation**: Lines after 200 are truncated.

## Context Injection Channels

Claude Code splits dynamic context across two API surfaces, not just the `system` parameter. This is critical for both prompt caching efficiency and compatibility with third-party API gateways that impose body size limits on system blocks.

### System parameter → static content

The `system` field in the API request contains only **static, cacheable** content:

- Identity prefix (separate block for API validation).
- Billing attribution header.
- Tool usage guidance, task guidance, tone / style instructions.
- Session-specific guidance, MCP instructions, output style.
- Git status and cache-breaker (appended via `appendSystemContext()`).

All of these are assembled from `getSystemPrompt()` sections and `getSystemContext()`. They receive `cache_control` with `global` or `org` scope via `buildSystemPromptBlocks()`.

### Messages[0] → dynamic user context

CLAUDE.md content, current date, and other user-facing context are **not** placed in the system prompt. Instead, Claude Code prepends a synthetic user message at the front of the `messages` array via `prependUserContext()`:

```json
{
  "role": "user",
  "content": "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\n<CLAUDE.md content>\n# currentDate\nToday's date is 2026-04-12.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n"
}
```

This message is marked `isMeta: true` (hidden from the UI, visible to the model). The context is structured as named sections (`# claudeMd`, `# currentDate`) within the `<system-reminder>` wrapper.

Why this split matters:

1. **Prompt caching**: Static system blocks stay identical across turns and sessions → server reuses the cached prefix. If CLAUDE.md were in `system`, any edit would bust the cache for the entire prompt (~20K+ tokens).
2. **Gateway compatibility**: Some third-party gateways reject requests where any `system` text block exceeds ~2700 characters. Moving CLAUDE.md to a user message sidesteps this limit entirely.
3. **Per-conversation memoization**: `getUserContext()` is memoized once per conversation (not per turn), so the CLAUDE.md walk and date computation happen once.

### Context sources

| Function             | Returns                       | Injected via                                    |
| -------------------- | ----------------------------- | ----------------------------------------------- |
| `getSystemContext()` | `gitStatus`, `cacheBreaker`   | `appendSystemContext()` → `system` parameter    |
| `getUserContext()`   | `claudeMd`, `currentDate`     | `prependUserContext()` → `messages[0]`          |

Both are memoized per conversation. `getSystemContext()` skips git status in remote sessions.

## XML Tag Conventions

Claude Code uses XML-like tags extensively in message content for structured metadata. The model is instructed to recognize these tags as system-generated (not user-written). Key tags:

| Tag                      | Purpose                                                   | Where used                           |
| ------------------------ | --------------------------------------------------------- | ------------------------------------ |
| `<system-reminder>`      | Wraps injected context (CLAUDE.md, tool results metadata) | User messages, tool results          |
| `<local-command-caveat>` | Marks locally-run command output (not a user prompt)      | User messages                        |
| `<local-command-stdout>` | Stdout from local `!` commands                            | User messages                        |
| `<local-command-stderr>` | Stderr from local `!` commands                            | User messages                        |
| `<bash-stdout>`          | Tool result stdout                                        | Tool results                         |
| `<bash-stderr>`          | Tool result stderr                                        | Tool results                         |
| `<command-name>`         | Skill / slash command identifier                          | User messages                        |
| `<task-notification>`    | Background task completion                                | User messages                        |
| `<teammate-message>`     | Inter-agent communication                                 | User messages                        |

The system prompt instructs the model about `<system-reminder>` tags:

> "Tool results and user messages may include `<system-reminder>` or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."

## API Request Metadata

Each API request includes a `metadata` field with a `user_id` containing a stringified JSON object:

```json
{
  "metadata": {
    "user_id": "{\"device_id\":\"<device-uuid>\",\"account_uuid\":\"<oauth-uuid>\",\"session_id\":\"<session-uuid>\"}"
  }
}
```

- `session_id`: UUID v4, generated once per session via `randomUUID()`. Regenerated on conversation clear.
- `device_id`: Persistent UUID stored in `~/.claude/.user-id` (created once, reused across sessions).
- `account_uuid`: OAuth account UUID when using OAuth auth; empty string for API key auth.

The `user_id` value is `JSON.stringify()`'d — the API receives a string, not a nested object.

## Tool Definitions

Tool schemas are sent via the API `tools` parameter, **not** in the system prompt. The system prompt contains only tool _guidance_ (how / when to use tools) and availability info (which tools are enabled). This is consistent across both Claude Code and opencode.

## Prompt Caching

The API supports prompt caching via `cache_control` on `TextBlockParam` blocks. Claude Code assigns cache scopes:

- `global` — static instructions identical across all sessions (first-party only).
- `org` — organization-scoped prefix.
- `null` — dynamic content, not cached.

The `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker separates cacheable from non-cacheable content. Effective caching requires the static prefix to be identical across requests.

## opencode Patterns

opencode (TypeScript / Bun) uses a similar hierarchical approach:

- **Provider-specific prompt templates**: Different base prompts for GPT, Claude, Gemini, etc. — selected by model at prompt assembly time.
- **Instruction file hierarchy**: `AGENTS.md` → `CLAUDE.md` → `CONTEXT.md` (deprecated), first match wins. Walk-up from CWD with per-message claim tracking to prevent duplicate attachment.
- **Per-turn rebuild**: System prompt assembled per user message (not per session), enabling dynamic skill updates and environment refresh.
- **4-level config precedence**: Managed (macOS MDM) → global (`~/.opencode/`) → instance (project) → plugins. Instructions arrays are concatenated with deduplication, not replaced.
- **Two-phase compaction**: Backward-walk pruning (truncate older tool outputs, protect recent 40K tokens) → manual summarization. Messages are never removed, only tool output is truncated.

## Sources

### Claude Code

- `claude-code/src/bootstrap/state.ts` — `getSessionId()`, UUID v4 session ID generation
- `claude-code/src/constants/prompts.ts` — section content, `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`, `<system-reminder>` guidance
- `claude-code/src/constants/systemPromptSections.ts` — section caching system
- `claude-code/src/constants/xml.ts` — XML tag constants (`<system-reminder>`, `<local-command-*>`, etc.)
- `claude-code/src/context.ts` — `getUserContext()` (CLAUDE.md + date), `getSystemContext()` (git status)
- `claude-code/src/query.ts` — `prependUserContext` / `appendSystemContext` wiring at query time
- `claude-code/src/services/api/claude.ts` — `queryModel`, `buildSystemPromptBlocks`, `getAPIMetadata()`, `getCacheControl()`
- `claude-code/src/utils/api.ts` — `splitSysPromptPrefix`, `prependUserContext`, `appendSystemContext`, cache scope assignment
- `claude-code/src/utils/claudemd.ts` — `getMemoryFiles`, `@include`, conditional rules
- `claude-code/src/utils/context.ts` — token budgeting, `getUserContext`
- `claude-code/src/utils/systemPrompt.ts` — `buildEffectiveSystemPrompt`, priority logic

### opencode

- `opencode/packages/opencode/src/config/config.ts` — 4-level config layering, MDM support
- `opencode/packages/opencode/src/session/compaction.ts` — pruning + summarization strategies
- `opencode/packages/opencode/src/session/instruction.ts` — instruction file discovery, walk-up semantics
- `opencode/packages/opencode/src/session/prompt.ts` — per-turn prompt assembly
- `opencode/packages/opencode/src/session/system.ts` — provider-specific templates, environment detection
