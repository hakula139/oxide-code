# System Prompt Architecture

Research notes on how Claude Code constructs its system prompt, injects context, and shapes Messages API requests.

## Section-Based Assembly

Claude Code builds its system prompt from discrete sections with lazy, memoized resolution, split by a dynamic boundary marker into two groups:

- **Static sections** (before the boundary, globally cacheable): identity, system guidance, task guidance, actions, tool usage, tone / style, output efficiency.
- **Dynamic sections** (after the boundary, not cacheable): session guidance, CLAUDE.md memory, environment info, MCP instructions, language preference, output style, token budget.

Assembly pipeline:

1. Collect static and dynamic section definitions, resolving static sections immediately and memoizing dynamic sections lazily.
2. Resolve all sections and apply prompt priority: override > coordinator > agent > custom > default.
3. Split on cache boundaries (consuming the boundary marker so it never reaches the request), assign each block a cache scope (`global` / `org` / `null`), and wrap each block in a `TextBlockParam` with `cache_control`.

## CLAUDE.md Loading Hierarchy

Files load in priority order, latest highest:

| Order | Type    | Path                                  | Description                           |
| ----- | ------- | ------------------------------------- | ------------------------------------- |
| 1     | Managed | `/etc/claude-code/CLAUDE.md`          | Organization-level instructions       |
| 2     | User    | `~/.claude/CLAUDE.md`                 | User's global instructions            |
| 3     | Project | `CLAUDE.md`, `.claude/CLAUDE.md`      | Checked-in project instructions       |
| 4     | Rules   | `.claude/rules/*.md`                  | Conditional rules with path globs     |
| 5     | Local   | `CLAUDE.local.md`                     | Private project-specific (gitignored) |
| 6     | AutoMem | `~/.claude/projects/<slug>/MEMORY.md` | Auto-accumulated memory               |

Project files are discovered by walking from the git root down to the working directory, so subdirectory instructions load later and take higher priority than root-level ones. For `/repo/crates/core`:

```text
/repo/CLAUDE.md              /repo/.claude/CLAUDE.md
/repo/crates/CLAUDE.md       /repo/crates/.claude/CLAUDE.md
/repo/crates/core/CLAUDE.md  /repo/crates/core/.claude/CLAUDE.md
```

Features:

- **`@include` directives**: `@./relative/path`, `@~/home`, `@/absolute`, recursive up to depth 5.
- **Conditional rules**: `.md` files with `paths:` frontmatter, glob-matched to decide inclusion.
- **HTML comment stripping**: block-level HTML comments are stripped.
- **`MEMORY.md` truncation**: lines after 200 are dropped.

## Context Injection Channels

Claude Code splits dynamic context across two API surfaces to keep the cached prompt prefix stable. Both system context and user context are memoized once per conversation.

### System Parameter → Static Content

The `system` field holds cacheable prompt content marked with `global` or `org` scope, plus appended system context:

- Billing attribution header and identity prefix (separate blocks for API validation).
- Tool usage, task, and tone / style guidance.
- Session guidance, MCP instructions, and output style.
- System context: git status (skipped in remote sessions) and a cache-breaker.

### Messages[0] → Dynamic User Context

CLAUDE.md content and the current date go into a synthetic user message prepended to `messages`:

```json
{
  "role": "user",
  "content": "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\n<CLAUDE.md content>\n# currentDate\nToday's date is 2026-04-12.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n"
}
```

The message is marked `isMeta: true` (hidden from the UI, visible to the model) and wraps named sections (`# claudeMd`, `# currentDate`) in `<system-reminder>`. Splitting user context out of `system` serves two purposes:

1. **Prompt caching.** Static system blocks stay identical across turns and sessions, avoiding busting ~20K+ cached tokens whenever CLAUDE.md changes.
2. **Per-conversation memoization.** The CLAUDE.md walk and date computation run once per conversation.

## XML Tag Conventions

Claude Code marks structured metadata in message content with XML-like tags that the model is instructed to treat as system-generated:

| Tag                      | Purpose                                                  | Where used                  |
| ------------------------ | -------------------------------------------------------- | --------------------------- |
| `<system-reminder>`      | Wraps injected context (CLAUDE.md, tool result metadata) | User messages, tool results |
| `<local-command-caveat>` | Marks locally-run command output (not a user prompt)     | User messages               |
| `<local-command-stdout>` | Stdout from local `!` commands                           | User messages               |
| `<local-command-stderr>` | Stderr from local `!` commands                           | User messages               |
| `<bash-stdout>`          | Tool result stdout                                       | Tool results                |
| `<bash-stderr>`          | Tool result stderr                                       | Tool results                |
| `<command-name>`         | Skill / slash command identifier                         | User messages               |
| `<task-notification>`    | Background task completion                               | User messages               |
| `<teammate-message>`     | Inter-agent communication                                | User messages               |

System prompt guidance for `<system-reminder>`:

> "Tool results and user messages may include `<system-reminder>` or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."

## API Request Metadata

Each request includes `metadata.user_id` as a JSON-stringified object:

```json
{
  "metadata": {
    "user_id": "{\"device_id\":\"<device-id>\",\"account_uuid\":\"<oauth-uuid>\",\"session_id\":\"<session-uuid>\"}"
  }
}
```

- `session_id`: UUID v4, generated once per session and regenerated on conversation clear.
- `device_id`: 64 hex characters (32 random bytes), created once and saved as `userID` in `~/.claude.json` (2.1.87).
- `account_uuid`: OAuth account UUID under OAuth, empty string under API-key auth.

## Tool Definitions

Tool schemas go in the API `tools` parameter, while the system prompt carries only tool guidance and enabled-tool descriptions. Claude Code and opencode follow the same split.

## System Block Layout

Prompt caching attaches `cache_control` to `TextBlockParam` blocks following the scopes and invariance rules in [Anthropic API § Prompt Caching Scope](anthropic.md#prompt-caching-scope). Caching requires the static prefix to stay identical across requests. [System Prompt](../../design/prompt.md) and [Client](../../design/client.md#request-shape) cover oxide-code's block assembly and base-URL gating.

Agentic streaming emits four system blocks (verified 2026-10-21 for Claude Code 2.1.87): the billing attribution header (`x-anthropic-billing-header`), the identity prefix (`"You are Claude Code, Anthropic's official CLI for Claude."`), static sections, and dynamic sections. The billing block ships on every base URL, and only the static block's `cache_control` varies by base URL.

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

The third-party shape omits `scope` because org is the absent-field default (matching the Anthropic SDK for non-global ephemeral caches). `ttl` follows `prompt_cache_ttl`: the tables show `"1h"`, while `"5m"` omits the field as the server default.

## Request Headers

Claude Code sends these headers on every Messages API request, with `?beta=true` on the URL:

| Header                                      | Value                                  | Notes                              |
| ------------------------------------------- | -------------------------------------- | ---------------------------------- |
| `User-Agent`                                | `claude-cli/{VERSION} (external, cli)` |                                    |
| `x-app`                                     | `cli`                                  |                                    |
| `x-claude-code-session-id`                  | UUID v4 per session                    |                                    |
| `anthropic-version`                         | `2023-06-01`                           |                                    |
| `anthropic-beta`                            | Comma-joined beta feature headers      | See below                          |
| `anthropic-dangerous-direct-browser-access` | `true`                                 | From SDK `dangerouslyAllowBrowser` |
| `x-stainless-lang`                          | `js`                                   | Anthropic SDK auto-injected        |
| `x-stainless-os`                            | `MacOS` / `Linux` / etc.               | Anthropic SDK auto-injected        |
| `x-stainless-arch`                          | `arm64` / `x64` / etc.                 | Anthropic SDK auto-injected        |

[Anthropic API § Per-Model Beta Sets](anthropic.md#per-model-beta-sets) lists the beta sequence for agentic Claude 5.5 requests. Additional betas join conditionally, including `oauth-2025-04-20` for subscribers, tool-search headers when tool search is enabled, and `redact-thinking-*` for thinking redaction.
