# System Prompt

The `prompt` module builds what the model reads about its role and its session: static sections shared by every session, an environment section for the session's machine and directory, and a user context that carries the instruction files, the date, the model's identity, and the permission mode.

## Request Layout

The client sends the system prompt as text blocks, with the identity block `You are Claude Code, Anthropic's official CLI for Claude.` placed as [Client](client.md#request-shape) describes, followed by the static sections and then the environment section. The static sections are identical in every session, so their block carries the prompt-cache breakpoint described in [Client](client.md#cache-breakpoints). The environment varies by session and follows the breakpoint.

The user context travels as a `<system-reminder>` in a user message that leads the conversation on the wire, while the transcript never stores it. An edited instruction file therefore leaves the system prompt byte-identical and its cache entry valid. A `/model` swap writes a new cache entry, since prompt caches are scoped to the model, but the earlier model's entries stay valid for a swap back within their lifetime.

## Static Sections

The static sections follow the order and much of the wording of Claude Code's prompt, whose layout [System Prompt Architecture](../research/api/system-prompt.md) records: an introduction, System, Doing tasks, Executing actions with care, Using your tools, Tone and style, and Output efficiency. They differ wherever `ox` behaves differently from Claude Code:

- **Harness.** The introduction says the model runs in oxide-code, names the config files and the `AGENTS.md` instruction files `ox` reads, says a project's `.ox/config.toml` cannot add allow rules or change credentials, the endpoint, or TLS trust, says Claude Code's settings, hooks, skills, slash commands, and MCP servers have no effect, and points usage questions to `/help` and the [user guides](../guide/README.md).
- **Permissions.** The System section describes the three [permission modes](../guide/permissions.md#modes) and explains that the check reads each command of a chain on its own while shell syntax it cannot analyze needs approval in `manual`. Each denial's tool result names what denied the call, and a `plan` denial names the tools `plan` still runs and tells the model to finish investigating and present its plan.
- **Links.** Tone and style asks for full issue and pull request URLs, which terminals open, since the TUI renders `owner/repo#123` as plain text.

## Environment

The environment section lists the working directory, whether it is in a Git repository, the platform, the shell that runs `bash` tool calls, the user's login shell from `$SHELL`, and the OS version. The `bash` tool always runs `bash -c`, so the section names bash as the shell and lists the login shell separately, for commands the model suggests the user run in their own terminal.

A session records the environment with its prompt context, as [Persistence](persistence.md#prompt-context) describes, and a resume in the same directory replays it. A resume in another directory detects the environment again.

## User Context

The leading user message carries the user context: the instruction files that [Instruction Files](../guide/instructions.md) describes, the local date, the model's name, exact ID, and knowledge cutoff, and the active permission mode. The System section describes each mode, so the mode section only names the active one, and in `plan` it also names the tools that still run and asks the model to investigate and then present its plan. The `plan` denial names the same tools through `plan_runs` over `ToolRegistry::plan_tools`, so the two texts cannot disagree. `PromptParts::seen` records what the model has seen of these values.

Before each prompt, the engine loads the user context again and compares it with what the model has seen. When an instruction file, the date, the model, or the mode changed, it appends a `<system-reminder>` with only the changed values to the prompt's user message, and transcript displays hide the reminder. A removed instruction file produces a reminder that the earlier instructions no longer apply. A mode switch through `/mode`, Tab, or Shift+Tab applies before the next user context render, gated call, or round boundary, whichever comes first. At each round boundary, the engine also compares the mode with what the model has seen and appends any reminder to the round's tool results, so a switch during a turn reaches the model with its next request, ahead of any prompt queued with it, and `seen` names the mode the model was last told. A resumed session whose recorded context predates the mode, or that resumes in another mode, names the current one on its first prompt. Compaction renders a fresh user context for the summarized transcript and counts it as seen, since no earlier request started with that transcript.

## Tool Descriptions

Each tool's description states the facts that would otherwise cost the model a failed call: how `bash` runs a command, the `read` row format that `edit` must strip from `old_string`, the regex dialect `grep` accepts and its line-by-line matching, and the files a search skips without saying so. Facts that a tool result already reports, such as a binary file refusal, a truncated listing or excerpt, or files a search skipped for size, stay in the result.

## Design Decisions

1. **Keep the identity block and name oxide-code in the introduction.** Non-Haiku OAuth requests fail without the exact identity text as its own block, and the API validates the first block after attribution, so the block stays byte-identical. The model still needs to know its real harness. One that believes it is Claude Code edits `.claude/settings.json` to stop a prompt, which `ox` never reads, so the introduction names oxide-code and what it reads.

2. **Follow Claude Code's sections and rewrite every sentence `ox` contradicts.** Claude models are tuned against Claude Code's prompt, so reusing its structure and wording where the harnesses behave alike keeps behavior predictable. Copying the prompt whole describes Claude Code's harness instead, such as a persistent working directory between `bash` calls and clickable `owner/repo#123` references, so each sentence is checked against what `ox` does. Rules the copied prompt states twice are merged into one sentence.

3. **Describe the permission modes statically, and name the active one in the user context.** The modes and the chain rule are the same in every session, so they belong in the cached static prefix. The active mode changes within a session, so it travels in the user context, where a switch costs one reminder and leaves the cached prefix intact, and the model plans in `plan` without first spending a denied call. The `plan` denial still tells the model to present its plan, since a denied call is the latest instruction it reads.

4. **Name the shell that runs commands.** Claude Code reports `$SHELL` because its Bash tool runs commands through the user's shell. `ox` runs every command with `bash -c`, so reporting only `$SHELL` invites zsh or fish syntax that bash rejects.

5. **Lead the messages with the user context.** Claude Code does the same. An instruction edit then changes only a message, so the cached system prompt survives it, and a catch-up reminder delivers that edit or a model swap without rewriting history the model has already read.

6. **Keep tool descriptions to facts that prevent a failed call.** Every request sends every description, so a fact the result already reports would cost tokens on each request to save one round in a rare case.

## Sources

- `crates/ox-core/src/client/anthropic.rs`: `SYSTEM_PROMPT_PREFIX` and `build_system_blocks`.
- `crates/ox-core/src/engine/dispatch.rs`: `denied_output` and `plan_denied_output`, the results of denied calls.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, which catches the mode up at each round boundary.
- `crates/ox-core/src/prompt.rs`: `PromptParts`, `UserContext`, `build_prompt`, `restore_prompt`, `mode_rules`, `plan_runs`, and the user context and reminder rendering.
- `crates/ox-core/src/prompt/environment.rs`: `Environment`, `model_identity`, and the date.
- `crates/ox-core/src/prompt/instructions.rs`: instruction file discovery and rendering.
- `crates/ox-core/src/prompt/sections.rs`: the static sections.
- `crates/ox-core/src/tool.rs`: `ToolDefinition`, whose description each tool's `spec` fills, and `ToolRegistry::plan_tools`.
