# Context Compression / `/compact` (Reference)

Research on how `/compact` (context compression via summarization) is implemented across Claude Code, OpenAI Codex, and opencode. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode) (v1.3.15).

Companion to [commands.md](commands.md). Storage-layer notes (JSONL entry shape, resume semantics) live in [session/persistence.md](../session/persistence.md). This file focuses on the _summarization_ surface: trigger, prompt, replacement strategy, auto-fire policy.

## Claude Code (TypeScript + Ink)

Most elaborate of the three: 9-section rubric, partial-compact rewind UI, hook-driven bookkeeping, and auto-compact on a fixed token buffer.

- **Trigger**: `/compact [instructions]` is a `local` command (`src/commands/compact/index.ts:4-13`). Args append as `customInstructions`. Partial-compact directions (`summarize`, `summarize_up_to`) ride a separate [`MessageSelector`](https://github.com/hakula139/claude-code/blob/main/src/components/MessageSelector.tsx) rewind UI sharing the same summarizer stack.

- **Prompt**: `BASE_COMPACT_PROMPT` requests an `<analysis>` plus `<summary>` block in nine numbered sections (Primary Request, Key Concepts, Files, Errors, Problem Solving, **All user messages**, Pending Tasks, Current Work, Optional Next Step), with verbatim quotes from recent turns. `NO_TOOLS_PREAMBLE` hard-bans tool calls. Custom args append under `Additional Instructions:`.

- **Summarizer**: Same model as the main loop, streaming. Fork-cache-sharing path reuses the parent prefix for cache hits, while a fallback uses a minimal system prompt plus the rubric in the user message.

- **Replacement**: `formatCompactSummary` wraps the reply as a synthetic user message flagged `isCompactSummary` and materializes as `[boundaryMarker, ...summary, ...kept, ...attachments, ...hookResults]`. The boundary is a `system / subtype: 'compact_boundary'` cell carrying `compactMetadata { trigger, preTokens, userContext, messagesSummarized }` and `logicalParentUuid`. Resume loaders truncate on it past `SKIP_PRECOMPACT_THRESHOLD`, and `parentUuid` chains reset across it.

- **Bookkeeping**: `readFileState` cleared and file attachments recreated under a budget. Plan-mode files, MCP instructions, and deferred tool listings re-attached. `preCompactDiscoveredTools` recorded for tool-schema continuity. `PreCompact` / `PostCompact` / `SessionStart` hooks fire around the boundary, and the pre-compact segment is archived via `writeSessionTranscriptSegment`.

- **Auto-compact**: Threshold is `effective_context_window - 13_000` (the buffer constant), checked at the start of every `query()` after `snip` and `microcompact`. Token signal: last assistant `usage.total_tokens` plus tail estimate. Circuit breaker disables auto after 3 consecutive failures. Env opt-out: `DISABLE_COMPACT` / `DISABLE_AUTO_COMPACT`.

- **Microcompact**: Pre-stage that clears specific tool-result bodies (Read, shell, Grep, Glob, Edit, Write, web) older than a TTL with `[Old tool result content cleared]` placeholders, adding its own boundary marker.

- **UX**: Spinner cycles `Running PreCompact hooks...` → `Compacting conversation` → `Running PostCompact hooks...`, then a dim `Compacted` line on success. Not-enough-messages aborts, mid-summarization abort shows "Compaction canceled.", prompt-too-long retries 3× trimming oldest rounds, and auto failures stay suppressed from the user.

## OpenAI Codex (Rust + Ratatui)

Terse prompt and history rebuild that drops every non-user-text round. Auto-compact fires both pre-sampling and mid-turn.

- **Trigger**: `SlashCommand::Compact` (`slash_command.rs:11-83`), `available_during_task: false`, no inline args. TUI emits `AppEvent::Compact` → `AppCommand::Compact` → `thread_compact_start`.

- **Prompt**: One short paragraph asking for a handoff summary (current progress, key decisions, context, remaining work, critical references), at `core/templates/compact/prompt.md`. A second template (`summary_prefix.md`) frames the next-turn view: _"Another language model started to solve this problem and produced a summary..."_. Users can override via `Config.compact_prompt`.

- **Local vs. remote**: Local path streams the same `model_client.stream` as a regular turn. OpenAI / Azure providers post to `responses/compact` and get back the rewritten history.

- **History rebuild** (local): `replace_compacted_history` walks backward collecting user-text messages under `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`, skipping anything matching `is_summary_message` so prior summaries don't re-walk. Assistant, tool-call, tool-output, and reasoning blocks are dropped. The new history is the retained user backlog plus `SUMMARY_PREFIX + summary_suffix`, all materialized as `role: user`.

- **Persistence**: Appends `RolloutItem::Compacted { message, replacement_history? }` to the rollout JSONL. The test suite covers `compact → shutdown → resume → fork`.

- **Auto-compact**: Fires at `(context_window * 9) / 10` (90%). Two sites in `core/src/session/turn.rs`: pre-sampling before a new turn enters `run_turn`, and mid-turn after `run_sampling_request` returns when the turn still `needs_follow_up`. Token signal: `Session::get_total_token_usage` (last API total plus client estimates).

- **UX**: Regular task-running spinner during compaction. Post-success the thread renders a `ThreadItem::ContextCompaction` info line.

- **Failures**: Stream errors retry up to `stream_max_retries` with a `Reconnecting...` notice. `Interrupted` propagates as Cancelled. `ContextWindowExceeded` during the compact request itself trims oldest rows when `turn_input_len > 1`, else surfaces an error. `steer_input` rejects mid-compaction queueing with `NonSteerableTurnKind::Compact`.

## opencode (TypeScript + Solid)

Dedicated `compaction` agent, anchored re-summarization via `<previous-summary>`, and a `prune` pass that wipes old tool-output bodies before the summarization call.

- **Trigger**: `/compact` (alias `/summarize`) calls `sdk.client.session.summarize({ sessionID, modelID, providerID })`. No free-text args. An optional `auto: bool` field distinguishes manual from auto. Handler runs `revert.cleanup`, then `SessionCompaction.create`, then `SessionPrompt.loop`.

- **Compaction agent**: Dedicated agent `compaction` (`mode: "primary"`, `hidden: true`, every tool set to `deny`). System prompt anchors the operation with _"Summarize only the conversation history you are given..."_ and _"If the prompt includes a `<previous-summary>` block, treat it as the current anchored summary. Update it..."_. The anchored pattern is opencode's signature.

- **Rubric**: Markdown template with terse bullets in this order: Goal · Constraints · Progress · Key Decisions · Next Steps · Critical Context · Relevant Files. Exact paths, commands, and error strings are preserved verbatim.

- **Input selection**: `select` finds the older prefix and excludes ranges between previously-completed `compaction`+`summary` pairs so prior summaries aren't re-summarized. Tool outputs truncate to 2,000 chars, media is stripped, and default model is the user's selected one unless `agent.compaction.model` is set.

- **Replacement**: Same session, no roll. Appends an empty `user` row with `type: "compaction"`, then an `assistant` row with `summary: true, mode: "compaction"` carrying the text. `MessageV2.filterCompacted` makes the active context behave like `[marker, summary, optional verbatim tail]` while older turns remain in storage. The compaction part stores `tail_start_id` so forks find the boundary.

- **Auto-continue plugin**: After auto-success, `experimental.compaction.autocontinue` can synthesize a `synthetic: true` continue prompt to drive the next turn without user input.

- **Auto-compact**: `isOverflow({ tokens, model })` returns true when `count >= usable(model)` where `count` is the last assistant `tokens.total` and `usable = limit.input - reserved`. Fires after a non-summary assistant turn finishes, and again when the stream processor returns a `"compact"` signal from a `ContextOverflowError`.

- **Config**: `compaction.auto`, `compaction.prune`, `compaction.tail_turns`, `compaction.preserve_recent_tokens`, `compaction.reserved`. Env overrides: `OPENCODE_DISABLE_AUTOCOMPACT`, `OPENCODE_DISABLE_PRUNE`.

- **Prune pre-stage**: `SessionCompaction.prune` stamps `part.state.time.compacted` on old tool outputs to wipe their bodies before any compaction LLM call. Analogous to Claude Code's microcompact.

- **UX**: Top-bordered box captioned ` Compaction ` (or ` Auto Compaction `) above the user row, with the summary markdown inside. Still-too-large after stripping surfaces `ContextOverflowError` _"Session too large to compact - context exceeds model limit even after stripping media"_.

## Comparison

| Aspect              | Claude Code                                  | Codex (Rust)                                 | opencode                                 |
| ------------------- | -------------------------------------------- | -------------------------------------------- | ---------------------------------------- |
| Slash trigger       | `/compact [instructions]`                    | `/compact` (no args, idle only)              | `/compact`, alias `/summarize`           |
| Auto-compact        | `window - 13k`, default on, env opt-out      | 90% of window, default on                    | `usage ≥ usable`, default on             |
| Auto check site     | start of every `query()`                     | pre-sampling + mid-turn loop                 | post-finished-turn + mid-stream          |
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

## Patterns Worth Borrowing for oxide-code

1. **Free-text custom instructions.** `/compact <instructions>` lets the user steer the summary. Matches the existing `/rename <title>` / `/model <id>` typed-arg shape, and Claude Code's experience says it noticeably improves recall.

2. **Synthetic `role: user` message carrying the summary.** All three CLIs converge on this. Sidesteps two API constraints (assistant can't lead a turn, system is one-shot at the prefix) and keeps the next turn shape exactly like a fresh first prompt.

3. **Distinct boundary entry.** Claude Code's `compact_boundary` and Codex's `RolloutItem::Compacted` both let resume loaders find the cut point in O(line). Record `pre_message_count` (and later `pre_tokens`) for the post-compact UI line, free at write time.

4. **Strip tool calls / results / thinking from the summarizer input.** Codex's pattern is the strictest and the simplest. Only user texts plus the new summary survive.

5. **Hard-ban tool calls during the compaction turn.** Passing an empty tool registry at the API layer is cleaner than `NO_TOOLS_PREAMBLE`-style prompt enforcement.

6. **`summary_prefix` framing on the synthesized message.** Without a prefix the next turn often redundantly asks "what would you like me to do?". Codex and Claude Code both prep the model to _use_ the summary directly.

7. **Refuse mid-turn.** All three CLIs treat manual compaction as a between-turns operation. oxide-code's existing `Mutating` classification already encodes this.

8. **Surface a post-compact boundary block.** oxide-code's dedicated `CompactedBlock` fits a `Compacted X messages → 1 summary` header plus rendered summary markdown.

## Patterns to Reject

1. **9-section structured rubric (Claude Code).** Aimed at resumption across an indefinite gap, across sessions, across humans. oxide-code's compaction is in-process and single-user, where "All user messages: List ALL user messages" and verbatim-quote requirements bloat the summary 3-5×. A short directive (intent, decisions, code paths, next step, user constraints) gets the same outcome at one-third the size. Codex's prompt is the right reference shape.

2. **Anchored `<previous-summary>` re-compaction (opencode).** Clever for very long sessions, but the summarizer must distinguish "still true" from "stale" without the original, and quality slowly degrades through repeated rewrites. Defer until the second-compaction case hits.

3. **Auto-compact mid-turn (Codex `MidTurn`).** Requires hot-path token counting, pausing the in-flight reply, and a careful resume after history rebuild. Manual plus auto-fire at turn boundaries is enough.

4. **Microcompact / prune as a separate stage.** Useful at scale, but expensive to ship correctly without metrics (which results, when, what placeholder, how it interacts with file-tracker state). Defer until auto-compact telemetry exists.

5. **Partial / range compaction (Claude Code).** Adds a message-selector UI, a dual-direction summarizer, and a transcript splice. Land full-transcript compaction first.

6. **Hooks (PreCompact / PostCompact).** oxide-code doesn't have a hook surface yet, and introducing one for compaction would commit the project to that abstraction. Defer with "user-extensible workflow skills" on the roadmap.

7. **Synthetic auto-continue (opencode).** Quietly inserts a fake "Continue if you have next steps..." after auto-compact. oxide-code should land the summary and wait for the user. Surprises beat conveniences here.

8. **Pre-flight `count_tokens` API call.** Every reference CLI uses last assistant `usage.total_tokens` plus tail estimate and accepts the ~2-5% drift. The extra round-trip isn't worth it.
