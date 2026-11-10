# System Prompt Architecture

Research notes on how [Claude Code](https://github.com/hakula139/claude-code) constructs its system prompt.

## Section-Based Assembly

Claude Code builds the system prompt from sections, discrete units with lazy, memoized resolution, which a `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker splits in two:

- **Static sections** (before the boundary): identity, system guidance, task guidance, tool usage, tone / style. Globally cacheable.
- **Dynamic sections** (after the boundary): session-specific guidance, CLAUDE.md memory, environment info, MCP instructions, language preference, output style, token budget. Not cacheable.

Resolution pipeline:

1. `getSystemPrompt()` collects the static and dynamic section definitions.
2. Static sections resolve immediately, and dynamic sections are memoized promises.
3. `resolveSystemPromptSections()` awaits all section promises.
4. `buildEffectiveSystemPrompt()` applies the priority order override > coordinator > agent > custom > default.
5. `splitSysPromptPrefix()` splits by cache boundaries and assigns each block a `cacheScope` (`global` / `org` / `null`), so the API can reuse cached prefixes across sessions.
6. `buildSystemPromptBlocks()` wraps each block in a `TextBlockParam` with `cache_control` for the API.

## CLAUDE.md Loading Hierarchy

Files load in priority order, the latest highest:

| Order | Type    | Path                                  | Description                           |
| ----- | ------- | ------------------------------------- | ------------------------------------- |
| 1     | Managed | `/etc/claude-code/CLAUDE.md`          | Organization-level instructions       |
| 2     | User    | `~/.claude/CLAUDE.md`                 | User's global instructions            |
| 3     | Project | `CLAUDE.md`, `.claude/CLAUDE.md`      | Checked-in project instructions       |
| 4     | Rules   | `.claude/rules/*.md`                  | Conditional rules with path globs     |
| 5     | Local   | `CLAUDE.local.md`                     | Private project-specific (gitignored) |
| 6     | AutoMem | `~/.claude/projects/<slug>/MEMORY.md` | Auto-accumulated memory               |

Project files are discovered by walking from the git root down to the working directory, checking each directory on the way, so subdirectory instructions come later, at higher priority, than root-level ones. For a working directory of `/repo/crates/core`:

```text
/repo/CLAUDE.md              /repo/.claude/CLAUDE.md
/repo/crates/CLAUDE.md       /repo/crates/.claude/CLAUDE.md
/repo/crates/core/CLAUDE.md  /repo/crates/core/.claude/CLAUDE.md
```

Features:

- **`@include` directives**: `@./relative/path`, `@~/home`, `@/absolute`, recursive include with max depth 5.
- **Conditional rules**: `.md` files with `paths:` frontmatter, glob-matched to decide inclusion.
- **HTML comment stripping**: block-level only, via the marked lexer.
- **MEMORY.md truncation**: lines after 200 are dropped.

## Context Injection Channels

Claude Code splits dynamic context across two API surfaces, which keeps the cached prompt prefix stable.

### System Parameter → Static Content

The request's `system` field holds only static, cacheable content, assembled from `getSystemPrompt()` sections and `getSystemContext()`, which `buildSystemPromptBlocks()` marks with `global` or `org` scope:

- Identity prefix (separate block for API validation).
- Billing attribution header.
- Tool usage guidance, task guidance, tone / style instructions.
- Session-specific guidance, MCP instructions, output style.
- Git status and cache-breaker (appended via `appendSystemContext()`).

### Messages[0] → Dynamic User Context

CLAUDE.md content, the current date, and other user-facing context go into a synthetic user message that `prependUserContext()` puts at the front of the `messages` array:

```json
{
  "role": "user",
  "content": "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\n<CLAUDE.md content>\n# currentDate\nToday's date is 2026-04-12.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n"
}
```

The message is marked `isMeta: true`, hidden from the UI and visible to the model, and holds named sections (`# claudeMd`, `# currentDate`) inside the `<system-reminder>` wrapper. The split serves two ends:

1. **Prompt caching.** Static system blocks stay identical across turns and sessions, so the server reuses the cached prefix. With CLAUDE.md in `system`, any edit would bust the cache for the entire prompt (~20K+ tokens).
2. **Per-conversation memoization.** `getUserContext()` is memoized once per conversation, so the CLAUDE.md walk and date computation happen once.

### Context Sources

| Function             | Returns                     | Injected via                                 |
| -------------------- | --------------------------- | -------------------------------------------- |
| `getSystemContext()` | `gitStatus`, `cacheBreaker` | `appendSystemContext()` → `system` parameter |
| `getUserContext()`   | `claudeMd`, `currentDate`   | `prependUserContext()` → `messages[0]`       |

Both are memoized per conversation, and `getSystemContext()` skips git status in remote sessions.

## XML Tag Conventions

Claude Code marks structured metadata in message content with XML-like tags, and the model is told to treat them as system-generated. Key tags:

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

The system prompt describes `<system-reminder>` tags this way:

> "Tool results and user messages may include `<system-reminder>` or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."

## API Request Metadata

Each API request includes a `metadata` field whose `user_id` is a JSON object passed through `JSON.stringify()`, so the API receives a string:

```json
{
  "metadata": {
    "user_id": "{\"device_id\":\"<device-id>\",\"account_uuid\":\"<oauth-uuid>\",\"session_id\":\"<session-uuid>\"}"
  }
}
```

- `session_id`: a UUID v4 from `randomUUID()`, generated once per session and again on conversation clear.
- `device_id`: 64 hex characters from `randomBytes(32)`, created once and saved as `userID` in `~/.claude.json` by `getOrCreateUserID()` (2.1.87).
- `account_uuid`: the OAuth account UUID under OAuth, and an empty string under API-key auth.

## Tool Definitions

Tool schemas go in the API `tools` parameter, and the system prompt holds only tool guidance and which tools are enabled. Claude Code and opencode agree on this.

## Prompt Caching

`cache_control` on `TextBlockParam` blocks enables prompt caching, with the scopes and invariance rule that [Anthropic API § Prompt Caching Scope](anthropic.md#prompt-caching-scope) gives. The `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marker separates cacheable from non-cacheable content, and caching works only while the static prefix stays identical across requests. [Client](../../design/client.md) covers which hosts oxide-code sends the global scope to.

## System Block Layout

Claude Code turns its flat section array into the block layout the API receives, consuming the boundary marker so it never reaches the request (`splitSysPromptPrefix` in `src/utils/api.ts` of the frozen 2.1.87 source, checked 2026-10-21). oxide-code builds the static and dynamic text as the two fields of `prompt::SystemPrompt`, so it needs no marker. Agentic streaming emits the same 4-block shape: billing / attribution, identity, static, and dynamic. The billing block ships on every base URL, and only the static block's `cache_control` varies by base URL.

**First-party base URL** (`api.anthropic.com`), global cache active:

| # | Content                          | `cache_control`                                     |
| - | -------------------------------- | --------------------------------------------------- |
| 0 | Billing / attribution header     | -                                                   |
| 1 | Identity prefix                  | -                                                   |
| 2 | Static sections joined (`\n\n`)  | `{ type: "ephemeral", scope: "global", ttl: "1h" }` |
| 3 | Dynamic sections joined (`\n\n`) | -                                                   |

**Third-party base URL** (gateway, self-hosted, anything else), default scope:

| # | Content                          | `cache_control`                    |
| - | -------------------------------- | ---------------------------------- |
| 0 | Billing / attribution header     | -                                  |
| 1 | Identity prefix                  | -                                  |
| 2 | Static sections joined (`\n\n`)  | `{ type: "ephemeral", ttl: "1h" }` |
| 3 | Dynamic sections joined (`\n\n`) | -                                  |

The third-party shape omits `scope` because org is the absent-field default, and that is the shape the Anthropic SDK ships for non-global ephemeral caches. `ttl` follows `prompt_cache_ttl`: the tables show `"1h"`, and `"5m"` omits the field as the server default.

The static sections are intro, system, doing tasks, actions, tools, tone / style, and output efficiency. The dynamic sections include session guidance, environment, language, and MCP instructions.

The attribution header is the billing `x-anthropic-billing-header` block, and the identity prefix is `"You are Claude Code, Anthropic's official CLI for Claude."`, matched by `CLI_SYSPROMPT_PREFIXES`.

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

[Anthropic API § Per-model beta sets](anthropic.md#per-model-beta-sets) lists the beta sequence Claude Code sends on agentic Claude 5.5 requests. Other betas join conditionally: `oauth-2025-04-20` for subscribers, tool-search headers when tool search is on, and `redact-thinking-*` for thinking redaction, among others.

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
