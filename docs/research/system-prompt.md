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

**Claude Code:**

- `claude-code/src/constants/prompts.ts` — section content, `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`
- `claude-code/src/constants/systemPromptSections.ts` — section caching system
- `claude-code/src/services/api/claude.ts` — `queryModel`, `buildSystemPromptBlocks`
- `claude-code/src/utils/api.ts` — `splitSysPromptPrefix`, cache scope assignment
- `claude-code/src/utils/claudemd.ts` — `getMemoryFiles`, `@include`, conditional rules
- `claude-code/src/utils/context.ts` — token budgeting, `getUserContext`
- `claude-code/src/utils/systemPrompt.ts` — `buildEffectiveSystemPrompt`, priority logic

**opencode:**

- `opencode/packages/opencode/src/config/config.ts` — 4-level config layering, MDM support
- `opencode/packages/opencode/src/session/compaction.ts` — pruning + summarization strategies
- `opencode/packages/opencode/src/session/instruction.ts` — instruction file discovery, walk-up semantics
- `opencode/packages/opencode/src/session/prompt.ts` — per-turn prompt assembly
- `opencode/packages/opencode/src/session/system.ts` — provider-specific templates, environment detection
