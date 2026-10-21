# System Prompt Architecture

Research notes on how Claude Code constructs its system prompt. Based on [Claude Code](https://github.com/hakula139/claude-code).

## Section-Based Assembly

Claude Code builds the system prompt from **sections**, discrete units with lazy, memoized resolution. Sections are split into two categories by a `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker:

- **Static sections** (before the boundary): identity, system guidance, task guidance, tool usage, tone / style. Globally cacheable via prompt caching.
- **Dynamic sections** (after the boundary): session-specific guidance, CLAUDE.md memory, environment info, MCP instructions, language preference, output style, token budget. Not cacheable.

Resolution pipeline:

1. `getSystemPrompt()` collects section definitions (static + dynamic).
2. Static sections resolve immediately. Dynamic sections are promises with memoization.
3. `resolveSystemPromptSections()` awaits all section promises.
4. `buildEffectiveSystemPrompt()` applies priority logic: override > coordinator > agent > custom > default.
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

- **`@include` directives**: `@./relative/path`, `@~/home`, `@/absolute`, recursive include with max depth 5.
- **Conditional rules**: `.md` files with `paths:` frontmatter, glob-matched to decide inclusion.
- **HTML comment stripping**: Block-level only, via marked lexer.
- **MEMORY.md truncation**: Lines after 200 are truncated.

## Context Injection Channels

Claude Code splits dynamic context across two API surfaces. This is critical for prompt caching efficiency.

### System Parameter → Static Content

The `system` field in the API request contains only **static, cacheable** content:

- Identity prefix (separate block for API validation).
- Billing attribution header.
- Tool usage guidance, task guidance, tone / style instructions.
- Session-specific guidance, MCP instructions, output style.
- Git status and cache-breaker (appended via `appendSystemContext()`).

All of these are assembled from `getSystemPrompt()` sections and `getSystemContext()`. They receive `cache_control` with `global` or `org` scope via `buildSystemPromptBlocks()`.

### Messages[0] → Dynamic User Context

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
2. **Per-conversation memoization**: `getUserContext()` is memoized once per conversation (not per turn), so the CLAUDE.md walk and date computation happen once.

### Context Sources

| Function             | Returns                     | Injected via                                 |
| -------------------- | --------------------------- | -------------------------------------------- |
| `getSystemContext()` | `gitStatus`, `cacheBreaker` | `appendSystemContext()` → `system` parameter |
| `getUserContext()`   | `claudeMd`, `currentDate`   | `prependUserContext()` → `messages[0]`       |

Both are memoized per conversation. `getSystemContext()` skips git status in remote sessions.

## XML Tag Conventions

Claude Code uses XML-like tags extensively in message content for structured metadata. The model is instructed to recognize these tags as system-generated (not user-written). Key tags:

| Tag                      | Purpose                                                   | Where used                  |
| ------------------------ | --------------------------------------------------------- | --------------------------- |
| `<system-reminder>`      | Wraps injected context (CLAUDE.md, tool results metadata) | User messages, tool results |
| `<local-command-caveat>` | Marks locally-run command output (not a user prompt)      | User messages               |
| `<local-command-stdout>` | Stdout from local `!` commands                            | User messages               |
| `<local-command-stderr>` | Stderr from local `!` commands                            | User messages               |
| `<bash-stdout>`          | Tool result stdout                                        | Tool results                |
| `<bash-stderr>`          | Tool result stderr                                        | Tool results                |
| `<command-name>`         | Skill / slash command identifier                          | User messages               |
| `<task-notification>`    | Background task completion                                | User messages               |
| `<teammate-message>`     | Inter-agent communication                                 | User messages               |

The system prompt instructs the model about `<system-reminder>` tags:

> "Tool results and user messages may include `<system-reminder>` or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."

## API Request Metadata

Each API request includes a `metadata` field with a `user_id` containing a stringified JSON object:

```json
{
  "metadata": {
    "user_id": "{\"device_id\":\"<device-id>\",\"account_uuid\":\"<oauth-uuid>\",\"session_id\":\"<session-uuid>\"}"
  }
}
```

- `session_id`: UUID v4, generated once per session via `randomUUID()`. Regenerated on conversation clear.
- `device_id`: 64 hex characters from `randomBytes(32)`, created once and saved as `userID` in `~/.claude.json` by `getOrCreateUserID()` (2.1.87).
- `account_uuid`: OAuth account UUID when using OAuth auth. Empty string for API key auth.

The `user_id` value is `JSON.stringify()`'d, so the API receives it as a string.

## Tool Definitions

Tool schemas are sent via the API `tools` parameter, **not** in the system prompt. The system prompt contains only tool _guidance_ (how / when to use tools) and availability info (which tools are enabled). This is consistent across both Claude Code and opencode.

## Prompt Caching

The API supports prompt caching via `cache_control` on `TextBlockParam` blocks. Cache scopes:

- `global`: Static instructions identical across all sessions, cached across every user of the first-party API. See [Anthropic API § Prompt Caching Scope](anthropic.md#prompt-caching-scope) for the invariance rule.
- _(absent)_: Default org-scoped ephemeral cache.
- `null` (no `cache_control`): Dynamic content excluded from caching.

The `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker separates cacheable from non-cacheable content. Effective caching requires the static prefix to be identical across requests.

oxide-code ships `scope: "global"` only when the configured base URL points at the first-party API. On any other host the static prefix still gets ephemeral caching at org scope. The dynamic sections and block order are identical in both modes.

## System Block Layout

Claude Code transforms its flat sections array into the block layout actually sent to the API, consuming the boundary marker so it never appears in the request. oxide-code builds the static and dynamic text as the two fields of `prompt::SystemPrompt`, so it needs no marker. Agentic streaming emits the same 4-block shape (billing / attribution + identity + static + dynamic). The billing block ships on every base URL, and only the `cache_control` on the static block varies by base URL.

**First-party base URL** (`api.anthropic.com`), global cache active:

| #   | Content                          | `cache_control`                          |
| --- | -------------------------------- | ---------------------------------------- |
| 0   | Billing / attribution header     | -                                        |
| 1   | Identity prefix                  | -                                        |
| 2   | Static sections joined (`\n\n`)  | `{ type: "ephemeral", scope: "global" }` |
| 3   | Dynamic sections joined (`\n\n`) | -                                        |

**Third-party base URL** (gateway, self-hosted, anything else), default scope:

| #   | Content                          | `cache_control`         |
| --- | -------------------------------- | ----------------------- |
| 0   | Billing / attribution header     | -                       |
| 1   | Identity prefix                  | -                       |
| 2   | Static sections joined (`\n\n`)  | `{ type: "ephemeral" }` |
| 3   | Dynamic sections joined (`\n\n`) | -                       |

Omitting the `scope` field is deliberate: org is the absent-field default, and the resulting wire shape is what the Anthropic SDK ships for non-global ephemeral caches.

Static sections (before boundary): intro, system, doing tasks, actions, tools, tone / style, output efficiency. Dynamic sections (after boundary): session guidance, environment, language, MCP instructions, etc.

The attribution header is the billing `x-anthropic-billing-header` block. The identity prefix is `"You are Claude Code, Anthropic's official CLI for Claude."`, matched by `CLI_SYSPROMPT_PREFIXES`.

## Request Headers

Claude Code sends these headers on every Messages API request:

| Header                                      | Value                                  | Notes                              |
| ------------------------------------------- | -------------------------------------- | ---------------------------------- |
| `User-Agent`                                | `claude-cli/{VERSION} (external, cli)` | `getUserAgent()` in http.ts        |
| `x-app`                                     | `cli`                                  |                                    |
| `x-claude-code-session-id`                  | UUID v4 per session                    |                                    |
| `anthropic-version`                         | `2023-06-01`                           |                                    |
| `anthropic-beta`                            | Comma-joined beta feature headers      | See below                          |
| `anthropic-dangerous-direct-browser-access` | `true`                                 | From SDK `dangerouslyAllowBrowser` |
| `x-stainless-lang`                          | `js`                                   | Anthropic SDK auto-injected        |
| `x-stainless-os`                            | `MacOS` / `Linux` / etc.               | Anthropic SDK auto-injected        |
| `x-stainless-arch`                          | `arm64` / `x64` / etc.                 | Anthropic SDK auto-injected        |

[Anthropic API § Per-model beta sets](anthropic.md#per-model-beta-sets) lists the beta sequence Claude Code sends on agentic Claude 5.5 requests. Additional betas are added conditionally: `oauth-2025-04-20` for subscribers, tool-search headers when tool search is enabled, `redact-thinking-*` for thinking redaction, etc.

The URL includes a `?beta=true` query parameter.

## Sources

- `claude-code/src/bootstrap/state.ts`: `getSessionId()`, UUID v4 session ID generation
- `claude-code/src/constants/prompts.ts`: section content, `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`, `<system-reminder>` guidance
- `claude-code/src/constants/systemPromptSections.ts`: section caching system
- `claude-code/src/constants/xml.ts`: XML tag constants (`<system-reminder>`, `<local-command-*>`, etc.)
- `claude-code/src/context.ts`: `getUserContext()` (CLAUDE.md + date), `getSystemContext()` (git status)
- `claude-code/src/query.ts`: `prependUserContext` / `appendSystemContext` wiring at query time
- `claude-code/src/services/api/claude.ts`: `queryModel`, `buildSystemPromptBlocks`, `getAPIMetadata()`, `getCacheControl()`
- `claude-code/src/utils/api.ts`: `splitSysPromptPrefix`, `prependUserContext`, `appendSystemContext`, cache scope assignment
- `claude-code/src/utils/claudemd.ts`: `getMemoryFiles`, `@include`, conditional rules
- `claude-code/src/utils/config.ts`: `getOrCreateUserID()` (`device_id`)
- `claude-code/src/utils/context.ts`: token budgeting, `getUserContext`
- `claude-code/src/utils/betas.ts`: `getAllModelBetas`, `getMergedBetas`, `shouldUseGlobalCacheScope`
- `claude-code/src/utils/http.ts`: `getUserAgent()` (`claude-cli/VERSION (USER_TYPE, ENTRYPOINT)`)
- `claude-code/src/utils/systemPrompt.ts`: `buildEffectiveSystemPrompt`, priority logic
- `claude-code/src/utils/userAgent.ts`: `getClaudeCodeUserAgent()` (`claude-code/VERSION`)
