# System Prompt

The `prompt` module constructs the model's context: shared static sections, a per-session environment block, and a user context carrying instruction files, local date, model identity, and permission mode.

## Request Layout

The client sends the system prompt as text blocks: the identity block `You are Claude Code, Anthropic's official CLI for Claude.` ([Client](client.md#request-shape)), the shared static sections carrying the prompt-cache breakpoint ([Client](client.md#cache-breakpoints)), and the per-session environment section after the breakpoint.

User context travels as a `<system-reminder>` in an unpersisted leading user message on the wire, keeping the system prompt byte-identical and cached across instruction edits. `/model` swaps write a new model-scoped cache entry while prior models' entries stay warm until expiry.

## Static Sections

Static sections follow Claude Code's order and wording ([System Prompt Architecture](../research/api/system-prompt.md): introduction, System, Doing tasks, Executing actions with care, Using your tools, Tone and style, Output efficiency), diverging wherever `ox` differs:

- **Harness.** Identifies oxide-code, names `ox` config and `AGENTS.md` files, notes that project `.ox/config.toml` cannot add allow rules or change credentials, endpoint, or TLS trust, notes that Claude Code settings, hooks, skills, slash commands, and MCP servers do not apply, and points usage questions to `/help` and the [user guides](../guide/README.md).
- **Permissions.** Explains the four [permission modes](../guide/permissions.md#modes) and per-segment shell matching. Denials name their cause, and `plan` denials list the tools `plan` allows and direct the model to finish investigating and present its plan.
- **Links.** Requests full issue and PR URLs because the TUI renders `owner/repo#123` as plain text.

## Environment

Lists working directory, Git repo presence, platform, `bash` (the tool shell), the user's `$SHELL` separately (for user-run suggestions), and OS version. Recorded with prompt context ([Persistence](persistence.md#prompt-context)) and replayed verbatim on same-directory resume, or re-detected when resuming in another directory.

## User Context

The leading user message carries [instruction files](../guide/instructions.md), local date, model name / exact ID / knowledge cutoff, and active permission mode (in `plan`, also listing allowed tools via `plan_runs` over `ToolRegistry::plan_tools` so prompt and denial text agree). `PromptParts::seen` tracks what the model has seen.

Before each prompt, the engine reloads user context and appends a display-hidden `<system-reminder>` with only changed values (or a removal notice when an instruction file disappears):

- **Mode switches.** Applied before the next user-context render, gated call, or round boundary. At round boundaries, any unseen mode change appends a reminder to tool results (folded into the last result on the wire) ahead of queued prompts and updates `seen`.
- **Resume.** Names the current mode on the first prompt if saved context predates mode tracking or resumed under a different mode.
- **Compaction.** Renders fresh user context for the summary root and marks it seen.

## Tool Descriptions

Descriptions state only facts needed to avoid failed calls (`bash` execution model, `read` line prefixes that `edit` must strip from `old_string`, `grep`'s regex dialect and line-by-line matching, and silently skipped search files). Result-reported facts (binary refusals, truncation, size-skipped files) stay in tool output.

## Design Decisions

1. **Keep the identity block and name oxide-code in the introduction.** Non-Haiku OAuth requests require the exact identity block after attribution, while naming oxide-code in the introduction stops the model from editing `.claude/settings.json` or assuming Claude Code features.
2. **Follow Claude Code's sections and rewrite every sentence `ox` contradicts.** Reusing familiar structure keeps Claude's behavior predictable while stripping inaccurate assumptions (such as persistent `bash` cwd or clickable `owner/repo#123`) and merging duplicated rules.
3. **Describe the permission modes statically, and name the active one in the user context.** Keeps static rules in the cached prefix while mode switches cost one reminder without busting cache or spending a denied call in `plan` (keeping the plan reminder on denials as the latest instruction).
4. **Name the shell that runs commands.** Reporting `bash` alongside `$SHELL` prevents zsh / fish syntax in `bash -c` calls (unlike Claude Code, which invokes `$SHELL`).
5. **Lead the messages with the user context.** Matches Claude Code so instruction edits leave the system cache valid and catch-up reminders propagate edits or model swaps without rewriting history.
6. **Keep tool descriptions to facts that prevent a failed call.** Avoids spending per-request tokens repeating details already returned in tool results.
7. **Fold a reminder after tool results into the last result on the wire.** A standalone text block after tool results looks like a new user turn and can teach the model to end its turn empty, so the Anthropic wire folds trailing text into the preceding tool result (matching Claude Code) while the transcript keeps the reminder separate for hidden display and exact replay.

## Sources

- `crates/ox-core/src/client/anthropic.rs`: `SYSTEM_PROMPT_PREFIX` and `build_system_blocks`.
- `crates/ox-core/src/client/anthropic/wire.rs`: `RequestMessage::of`, which folds text after tool results.
- `crates/ox-core/src/engine/dispatch.rs`: `denied_output` and `plan_denied_output`, the results of denied calls.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, which catches the mode up at each round boundary.
- `crates/ox-core/src/prompt.rs`: `PromptParts`, `UserContext`, `build_prompt`, `restore_prompt`, `mode_rules`, `plan_runs`, and the user context and reminder rendering.
- `crates/ox-core/src/prompt/environment.rs`: `Environment`, `model_identity`, and the date.
- `crates/ox-core/src/prompt/instructions.rs`: instruction file discovery and rendering.
- `crates/ox-core/src/prompt/sections.rs`: the static sections.
- `crates/ox-core/src/tool.rs`: `ToolDefinition`, whose description each tool's `spec` fills, and `ToolRegistry::plan_tools`.
