# System Prompt Architecture

Research notes on how Claude Code constructs its system prompt. Based on [`claude-code`](https://github.com/hakula139/claude-code) (v2.1.87) and [`opencode`](https://github.com/anomalyco/opencode).

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

opencode (Go) uses a similar hierarchical approach:

- **Agent-specific base prompts**: Each agent type (build, plan, explore) has its own prompt template.
- **Config-driven instructions**: `instructions: string[]` in config, concatenated into the system prompt.
- **8-level config precedence**: Managed → account → inline → `.opencode/` → `opencode.json` → custom path → global → remote.
- **Three-phase compaction**: Pruning (erase old tool outputs) → summarization (compaction agent) → truncation (replace with summary).

## Sources

- `claude-code/src/constants/prompts.ts` — section content, `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`
- `claude-code/src/constants/systemPromptSections.ts` — section caching system
- `claude-code/src/utils/systemPrompt.ts` — `buildEffectiveSystemPrompt`, priority logic
- `claude-code/src/utils/claudemd.ts` — `getMemoryFiles`, `@include`, conditional rules
- `claude-code/src/utils/context.ts` — token budgeting, `getUserContext`
- `claude-code/src/utils/api.ts` — `splitSysPromptPrefix`, cache scope assignment
- `claude-code/src/services/api/claude.ts` — `queryModel`, `buildSystemPromptBlocks`
