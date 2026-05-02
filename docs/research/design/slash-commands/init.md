# /init

Synthesizes a fixed prompt that asks the model to author or update an `AGENTS.md` (preferred) or `CLAUDE.md` instruction file at the project root, then forwards it to the agent loop as if the user had typed it. The user types the four-character shorthand `/init`; the chat shows the typed line; the wall-of-text expansion is invisible in the live session, and the assistant streams its response into the same conversation.

This is the first slash command of the **prompt-submit** kind. The cross-command surface (registry, parser, popup, dispatch) lives in [Slash Commands](README.md); this doc covers `/init` only.

## Reference

- **Claude Code** (`commands/init.ts`) — `type: 'prompt'`. Two prompt bodies live behind a feature flag: `OLD_INIT_PROMPT` (concise, single-shot ask) and `NEW_INIT_PROMPT` (multi-phase: AskUserQuestion clarifications, hooks/skills wiring, formatter detection, install recommendations). Reads `progressMessage: 'analyzing your codebase'` while the model works. Calls `maybeMarkProjectOnboardingComplete()` on dispatch.
- **Codex** (`SlashCommand::Init`) — submits a fixed prompt asking the model to create `AGENTS.md`. No interactive sub-flow.
- **opencode** — no `/init` analog. Onboarding lives outside the slash surface.

oxide-code adopts the Claude Code `OLD_INIT_PROMPT` shape: a single fixed prompt, no clarifying questions, no hooks/skills wiring. The interactive `NEW_INIT_PROMPT` flow needs `AgentEvent::PromptRequest` plumbing oxide-code doesn't have today — deferred until that lands.

## oxide-code Today

`InitCmd::execute` returns `Ok(SlashOutcome::PromptSubmit(PROMPT))` where `PROMPT` is the static body. The dispatcher (`slash::dispatch`) hands the body back to `App::apply_action_locally`, which:

1. Pushes the typed `/init` line as a `UserMessage` block (the chat affordance).
2. Disables input + flips status to `Streaming` (turn-start UI side effects, mirroring a typed prompt).
3. Forwards `UserAction::SubmitPrompt(body)` through `user_tx` to the agent loop.

The agent loop's existing `SubmitPrompt` arm records the body as `Message::user(...)`, persists it to JSONL, and runs `agent_turn` — no agent-side branching for `/init`.

## Design Decisions for oxide-code

1. **Prompt body = `OLD_INIT_PROMPT` adapted for `AGENTS.md`.** oxide-code's instruction loader walks both `AGENTS.md` and `CLAUDE.md` (root-to-cwd, root-level + `.claude/` at each level); AGENTS.md is the AI-coding-assistant-neutral filename also used by Codex and others, so the prompt asks the model to write AGENTS.md by default and update an existing CLAUDE.md or AGENTS.md in place rather than overwrite. Wording is concise — every line passes the "would the model get this wrong without it?" gate.
2. **`SlashOutcome::PromptSubmit(String)` is a typed third command kind.** `SlashCommand::execute` returns `Result<SlashOutcome, String>`; the dispatcher translates `PromptSubmit` into a `UserAction::SubmitPrompt` forward. Modeling this as an enum (rather than a side-channel field on `SlashContext`) makes the third command kind self-documenting and lets the type system enforce the contract.
3. **`/init` overrides `is_read_only` to `false`.** A prompt-submit command kicks off a turn that mutates `messages` and the session writer; running it mid-turn would race the in-flight one. The busy-branch dispatcher refuses with `/init runs only when idle. Try again after the turn finishes.` — the same gate `/clear` uses. Read-only commands stay safe to fire mid-turn (`/help`, `/status`, `/diff`, `/config`).
4. **Send-after dispatch.** Unlike `/clear` (which writes `UserAction::Clear` to `user_tx` from inside `execute`), `/init` returns the body via `SlashOutcome` and lets the App-side dispatcher decide when to flip UI state and forward. This way the turn-start side effects (input disabled, status `Streaming`) land _before_ the agent-loop-bound `SubmitPrompt` is queued — no race where the user could squeeze in a typed prompt between dispatch and forward.
5. **The expanded body is invisible in the live session.** Only the typed `/init` line lands as a chat block. The body shows up in resumed sessions because the JSONL records `Message::user(body)` faithfully — accepted trade-off. A polish pass could add a JSONL-level "display alias" so resumed transcripts also show `/init`; not blocking anything.
6. **No alias.** Claude Code accepts only `/init`; Codex accepts only `/init`. Adding `/setup` or `/onboard` would need a real user pull. Defer.
7. **No interactive clarification flow.** Claude Code's `NEW_INIT_PROMPT` asks the user mid-prompt via `AskUserQuestion`. oxide-code has no `AgentEvent::PromptRequest` plumbing today. When that lands, `/init` becomes the natural first consumer.

## Deferred

Behaviors Claude Code's `/init` ships that oxide-code skips today, and the subsystem each gates on:

1. **Multi-phase interactive flow** (`NEW_INIT_PROMPT` — phases 1–8: AskUserQuestion, hook wiring, skill suggestions, formatter detection). Lands with `AgentEvent::PromptRequest` plumbing. Until then `/init` ships the single-shot `OLD_INIT_PROMPT` body.
2. **`progressMessage`** (`'analyzing your codebase'` while the model works). oxide-code's status bar already shows `Streaming` / tool names; a dedicated "analyzing" string would need a status-bar variant. Trivial to add — defer until a second prompt-submit command needs it.
3. **`maybeMarkProjectOnboardingComplete`.** Claude Code records `/init` runs into `~/.claude.json`'s per-project onboarding state. oxide-code's stance against silent mega-file writes (decision 6 in [Slash Commands](README.md#design-decisions-for-oxide-code)) rules this out by default. If onboarding state ever matters, it lands as an explicit user-opted-in path.
4. **Parent-of-existing-instructions detection.** Claude Code reads `.cursor/rules/`, `.cursorrules`, `.github/copilot-instructions.md`, `.windsurfrules`, `.clinerules`, `.mcp.json` and folds the load-bearing parts into CLAUDE.md. The `OLD_INIT_PROMPT` body asks the model to do this from the prompt rather than from harness-side detection — the model is already a tool-using agent, so harness pre-discovery is redundant.

## Sources

- `crates/oxide-code/src/slash/init.rs` — `InitCmd`, `PROMPT`, `is_read_only` override.
- `crates/oxide-code/src/slash/registry.rs` — `SlashOutcome { Local, PromptSubmit(String) }`, registers `InitCmd`.
- `crates/oxide-code/src/slash.rs` — `dispatch` returns `Option<String>` (the prompt body); `dispatch_with` translates `SlashOutcome::PromptSubmit` into the return.
- `crates/oxide-code/src/tui/app.rs` — `apply_action_locally` slash branch handles the `Some(prompt)` path: flip input + status, forward `UserAction::SubmitPrompt(body)` via `forward_to_agent`. `forward_to_agent` is the extracted helper shared with the regular dispatch path.
- `claude-code/src/commands/init.ts` — reference flow (`OLD_INIT_PROMPT` adopted, `NEW_INIT_PROMPT` deferred).
