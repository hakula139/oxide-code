# Compaction (Reference)

Research on how manual `/compact` and automatic context compression work in Claude Code, OpenAI Codex, and opencode. Based on [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

Verified 2026-10-26 against Claude Code 2.1.87 (the frozen source snapshot), Codex `2351d9e1b608`, and opencode `v1.4.11-1713-g7d5e91b80`.

Companion to [commands.md](../slash/commands.md), which covers the command surface. This note focuses on the summarization path: trigger, prompt, replacement strategy, persistence, and the automatic policy.

## Claude Code (TypeScript)

The most elaborate of the three: a 9-section rubric, a partial-compact rewind UI, hook-driven bookkeeping, and auto-compaction on a fixed token buffer.

- **Trigger**: `/compact [instructions]` is a `local` command (`src/commands/compact/index.ts:4-13`). Args append as `customInstructions`. Partial-compact directions (`summarize`, `summarize_up_to`) ride a separate [`MessageSelector`](https://github.com/hakula139/claude-code/blob/main/src/components/MessageSelector.tsx) rewind UI sharing the same summarizer stack.

- **Prompt**: `BASE_COMPACT_PROMPT` requests an `<analysis>` plus `<summary>` block in nine numbered sections (Primary Request, Key Concepts, Files, Errors, Problem Solving, **All user messages**, Pending Tasks, Current Work, Optional Next Step), with verbatim quotes from recent turns. `NO_TOOLS_PREAMBLE` hard-bans tool calls. Custom args append under `Additional Instructions:`.

- **Summarizer**: Same model as the main loop, streaming. The fork-cache-sharing path reuses the parent prefix for cache hits, while a fallback uses a minimal system prompt plus the rubric in the user message.

- **Replacement**: `formatCompactSummary` wraps the reply as a synthetic user message flagged `isCompactSummary` and materializes as `[boundaryMarker, ...summary, ...kept, ...attachments, ...hookResults]`. The boundary is a `system / subtype: 'compact_boundary'` cell carrying `compactMetadata { trigger, preTokens, userContext, messagesSummarized }` and `logicalParentUuid`. Resume loaders truncate on it past `SKIP_PRECOMPACT_THRESHOLD`, and `parentUuid` chains reset across it.

- **Bookkeeping**: `readFileState` cleared and file attachments recreated under a budget. Plan-mode files, MCP instructions, and deferred tool listings re-attached. `preCompactDiscoveredTools` recorded for tool-schema continuity. `PreCompact` / `PostCompact` / `SessionStart` hooks fire around the boundary, and the pre-compact segment is archived via `writeSessionTranscriptSegment`.

- **Auto-compact**: Threshold is `effective_context_window - 13_000`, checked at the start of every `query()` after `snip` and `microcompact`. A warning shows at `threshold - 20_000`, and a blocking limit sits at `effective_window - 3_000`. Token signal: last assistant `usage.total_tokens` plus a tail estimate. The circuit breaker disables auto after 3 consecutive failures. Env opt-out: `DISABLE_COMPACT` / `DISABLE_AUTO_COMPACT`.

- **Microcompact**: A pre-stage that clears specific tool-result bodies (Read, shell, Grep, Glob, Edit, Write, web) older than a TTL with `[Old tool result content cleared]` placeholders, adding its own boundary marker.

- **UX**: Spinner cycles `Running PreCompact hooks...` → `Compacting conversation` → `Running PostCompact hooks...`, then a dim `Compacted` line on success. Not-enough-messages aborts, mid-summarization abort shows "Compaction canceled.", prompt-too-long retries 3× trimming oldest rounds, and auto failures stay suppressed from the user.

Key files:

- `claude-code/src/services/compact/autoCompact.ts`: threshold math, opt-out flags, circuit breaker.
- `claude-code/src/query.ts`: pre-query placement.
- `claude-code/src/utils/context.ts`: context-window detection.

## OpenAI Codex (Rust)

A terse prompt and a history rebuild that drops every non-user-text round. Auto-compact fires both pre-sampling and mid-turn.

- **Trigger**: `SlashCommand::Compact` (`slash_command.rs:11-83`), `available_during_task: false`, no inline args. The TUI emits `AppEvent::Compact` → `AppCommand::Compact` → `thread_compact_start`.

- **Prompt**: One short paragraph asking for a handoff summary (current progress, key decisions, context, remaining work, critical references), at `core/templates/compact/prompt.md`. A second template (`summary_prefix.md`) frames the next-turn view: _"Another language model started to solve this problem and produced a summary..."_. Users can override via `Config.compact_prompt`.

- **Local vs. remote**: The local path streams the same `model_client.stream` as a regular turn. OpenAI and Azure providers post to `responses/compact` and get back the rewritten history.

- **History rebuild** (local): `replace_compacted_history` walks backward collecting user-text messages under `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`, skipping anything matching `is_summary_message` so prior summaries don't re-walk. Assistant, tool-call, tool-output, and reasoning blocks are dropped. The new history is the retained user backlog plus `SUMMARY_PREFIX + summary_suffix`, all materialized as `role: user`.

- **Persistence**: Appends `RolloutItem::Compacted { message, replacement_history? }` to the rollout JSONL. The test suite covers `compact → shutdown → resume → fork`.

- **Auto-compact**: Fires at `(context_window * 9) / 10` (90%). Three sites in `core/src/session/turn.rs`: pre-sampling before a new turn enters `run_turn`, mid-turn after `run_sampling_request` returns when the turn still `needs_follow_up`, and a previous-model pass when the compaction hash changed or the turn switches to a smaller context-window model. A configured `auto_compact_token_limit` replaces the 90% default as an absolute token count, clamped to that 90% ceiling, and the downshift check falls back to `i64::MAX` when the new model has no limit. Token signal: `Session::get_total_token_usage` (last API total plus client estimates).

- **UX**: A regular task-running spinner during compaction. After success the thread renders a `ThreadItem::ContextCompaction` info line.

- **Failures**: Stream errors retry up to `stream_max_retries` with a `Reconnecting...` notice. `Interrupted` propagates as Cancelled. `ContextWindowExceeded` during the compact request itself trims oldest rows when `turn_input_len > 1`, else surfaces an error. `steer_input` rejects mid-compaction queueing with `NonSteerableTurnKind::Compact`.

Key files:

- `codex-rs/protocol/src/openai_models.rs`: 90% default and configured-limit clamp.
- `codex-rs/core/src/session/turn.rs`: pre-turn, mid-turn, and model-downshift triggers.
- `codex-rs/core/src/compact.rs`: inline summarization and history replacement.
- `codex-rs/config/src/config_toml.rs`: config surface.

## opencode (TypeScript)

A dedicated `compaction` agent, anchored re-summarization via `<previous-summary>`, and a `prune` pass that wipes old tool-output bodies before the summarization call.

- **Trigger**: `/compact` (alias `/summarize`) calls `sdk.client.session.summarize({ sessionID, modelID, providerID })`. No free-text args. An optional `auto: bool` field distinguishes manual from auto. The handler runs `revert.cleanup`, then `SessionCompaction.create`, then `SessionPrompt.loop`.

- **Compaction agent**: A dedicated agent `compaction` (`mode: "primary"`, `hidden: true`, every tool set to `deny`). Its system prompt anchors the operation with _"Summarize only the conversation history you are given..."_ and _"If the prompt includes a `<previous-summary>` block, treat it as the current anchored summary. Update it..."_. The anchored pattern is opencode's signature.

- **Rubric**: A Markdown template with terse bullets in this order: Goal · Constraints · Progress · Key Decisions · Next Steps · Critical Context · Relevant Files. Exact paths, commands, and error strings are preserved verbatim.

- **Input selection**: `select` finds the older prefix and excludes ranges between previously completed `compaction` and `summary` pairs so prior summaries aren't re-summarized. Tool outputs truncate to 2,000 chars, media is stripped, and the default model is the user's selected one unless `agent.compaction.model` is set. The tail keeps the last 2 turns, budgeted at 25% of the usable window clamped to 2,000-8,000 tokens.

- **Replacement**: Same session, no roll. Appends an empty `user` row with `type: "compaction"`, then an `assistant` row with `summary: true, mode: "compaction"` carrying the text. `MessageV2.filterCompacted` makes the active context behave like `[marker, summary, optional verbatim tail]` while older turns remain in storage. The compaction part stores `tail_start_id` so forks find the boundary.

- **Auto-continue plugin**: After an automatic success, `experimental.compaction.autocontinue` can synthesize a `synthetic: true` continue prompt to drive the next turn without user input.

- **Auto-compact**: `isOverflow({ tokens, model })` returns true when `count >= usable(model)`, where `count` is the last assistant `tokens.total` and `usable` is `limit.input - reserved`, or `context - max_output_tokens` when the model publishes no input limit. `reserved` defaults to the smaller of 20,000 and the model's max output tokens. It fires after a non-summary assistant turn finishes, and again when the stream processor returns a `"compact"` signal from a `ContextOverflowError`.

- **Config**: `compaction.auto`, `compaction.prune`, `compaction.tail_turns`, `compaction.preserve_recent_tokens`, `compaction.reserved`. Env overrides: `OPENCODE_DISABLE_AUTOCOMPACT` and `OPENCODE_DISABLE_PRUNE`.

- **Prune pre-stage**: `SessionCompaction.prune` stamps `part.state.time.compacted` on old tool outputs to wipe their bodies before any compaction LLM call. It is analogous to Claude Code's microcompact.

- **UX**: A top-bordered box captioned `Compaction` (or `Auto Compaction`) above the user row, with the summary Markdown inside. Still-too-large after stripping surfaces `ContextOverflowError` _"Session too large to compact - context exceeds model limit even after stripping media"_.

Key files:

- `packages/opencode/src/session/overflow.ts`: usable-context threshold.
- `packages/opencode/src/session/prompt.ts`: post-assistant and overflow-triggered compaction scheduling.
- `packages/opencode/src/session/compaction.ts`: prompt, tail preservation, pruning.
- `packages/opencode/src/agent/agent.ts`: the hidden, tool-denied compaction agent.

## Comparison

| Aspect              | Claude Code                                  | Codex (Rust)                                 | opencode                                 |
| ------------------- | -------------------------------------------- | -------------------------------------------- | ---------------------------------------- |
| Slash trigger       | `/compact [instructions]`                    | `/compact` (no args, idle only)              | `/compact`, alias `/summarize`           |
| Auto-compact        | `window - 13k`, default on, env opt-out      | 90% of window, default on                    | `usage ≥ usable`, default on             |
| Auto check site     | start of every `query()`                     | pre-sampling + mid-turn + downshift          | post-finished-turn + mid-stream          |
| Prompt shape        | 9-section rubric, `<analysis>` + `<summary>` | short paragraph + `summary_prefix` for next  | Markdown template, anchored update       |
| Anchored re-compact | no                                           | no, prior summaries skipped on walk-back     | yes, via `<previous-summary>` block      |
| Summarizer model    | same as main loop                            | same as main loop                            | configurable per `agent.compaction`      |
| Streaming           | yes (fork or fallback)                       | yes (local) / no (remote OpenAI / Azure)     | yes                                      |
| Replacement shape   | boundary + synthetic user msg + kept tail    | retained user msgs + summary as `role: user` | compaction marker + summary assistant    |
| Old transcript      | archived to segment, loader truncates        | replaced in memory, rollout records          | retained in DB, `filterCompacted` hides  |
| Persistence record  | `system / compact_boundary` JSONL line       | `RolloutItem::Compacted` + `TurnContextItem` | `compaction` part + `summary: true` row  |
| Tools / reasoning   | dropped (via microcompact)                   | dropped from rebuilt history                 | dropped via `tools: {}` + truncation     |
| Pre-compact pass    | microcompact (clears old tool bodies)        | none                                         | `prune` (stamps old tool outputs)        |
| Post-compact UI     | dim "Compacted" line + token warning bar     | "Context compacted" info line                | `Compaction` boxed pane with summary     |
| Hooks               | PreCompact / PostCompact / SessionStart      | PreCompact / PostCompact (can abort)         | plugin `experimental.session.compacting` |
