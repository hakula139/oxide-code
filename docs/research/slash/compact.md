# Context Compression / `/compact` (Reference)

Research on how `/compact` (context compression via summarization) is implemented across Claude Code, OpenAI Codex, and opencode. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode) (v1.3.15).

Companion to [commands.md](commands.md). Storage-layer notes (JSONL entry shape, resume semantics) live in [session/persistence.md](../session/persistence.md); this file focuses on the _summarization_ surface — trigger, prompt, replacement strategy, auto-fire policy.

## Claude Code (TypeScript + Ink)

`/compact` is a `local` command accepting optional free-text instructions (`src/commands/compact/index.ts:4-13`); custom args become `customInstructions` to the summarizer. A separate **rewind UI** ([`MessageSelector`](https://github.com/hakula139/claude-code/blob/main/src/components/MessageSelector.tsx)) drives **partial compact** — `summarize` (from a selected anchor onward) and `summarize_up_to` (everything before) — sharing the same summarizer stack.

The compaction prompt is the most elaborate of the three. `BASE_COMPACT_PROMPT` (`prompt.ts:61-143`) asks for an `<analysis>` + `<summary>` block with **nine numbered sections** (Primary Request, Key Concepts, Files, Errors, Problem Solving, **All user messages**, Pending Tasks, Current Work, Optional Next Step) including verbatim quotes from recent turns. A `NO_TOOLS_PREAMBLE` hard-bans tool calls during the compaction turn. Custom user instructions are appended under an `Additional Instructions:` header.

Summarization runs on the **same model as the main loop** (streaming via `queryModelWithStreaming`). Two paths: a **fork-cache-sharing** path that reuses the parent thread's full system prompt and `cacheSafeParams` (cheaper prefix cache hits), and a **fallback** with a minimal system prompt and the rubric in the user message.

The reply is parsed by `formatCompactSummary` into a flat `Summary:` block, then wrapped as a synthetic user message — _"This session is being continued from a previous conversation that ran out of context..."_ — flagged `isCompactSummary: true, isVisibleInTranscriptOnly: true`. The replacement is materialized as `[boundaryMarker, ...summaryMessages, ...messagesToKeep, ...attachments, ...hookResults]`. The **boundary marker** is a `system / subtype: 'compact_boundary'` message carrying `compactMetadata: { trigger, preTokens, userContext, messagesSummarized }` and a `logicalParentUuid` linking back; resume loaders use it to truncate when the file exceeds `SKIP_PRECOMPACT_THRESHOLD`, and `parentUuid` chains reset across it.

Bookkeeping is substantial: `readFileState` cache cleared then file attachments recreated under a budget, plan-mode files / MCP instructions / deferred tool listings re-attached, `preCompactDiscoveredTools` recorded for tool-schema continuity, `PreCompact` / `PostCompact` / `SessionStart` hooks fired, and the pre-compact segment archived via `writeSessionTranscriptSegment`.

**Auto-compact** (`services/compact/autoCompact.ts:62-90`) threshold is `effective_context_window - 13_000` (the buffer constant), checked at the **start of every `query()`** after `snip` and `microcompact`. Token signal is `last assistant usage.total_tokens + estimate of tail`. A circuit breaker disables auto after 3 consecutive failures; env opt-out is `DISABLE_COMPACT` / `DISABLE_AUTO_COMPACT`. A **microcompact** pre-stage clears specific tool result bodies (Read / shell / Grep / Glob / Edit / Write / web) older than a TTL with `[Old tool result content cleared]` placeholders, adding its own boundary marker.

UX cycles `Running PreCompact hooks…` → `Compacting conversation` → `Running PostCompact hooks…`; post-success shows a dim `Compacted` line. Failure paths: not-enough-messages aborts early, abort during summarization shows "Compaction canceled.", prompt-too-long retries up to 3 times trimming oldest API rounds, and auto failures are suppressed from the user.

## OpenAI Codex (Rust + Ratatui)

`/compact` is `SlashCommand::Compact` (`slash_command.rs:11-83`) — no inline args, `available_during_task: false`. TUI dispatch resets the token-usage HUD and emits `AppEvent::Compact` → `AppCommand::Compact` → `thread_compact_start`.

The compaction prompt is **terse** — a single short paragraph asking for a handoff summary covering current progress, key decisions, context / constraints / preferences, remaining work, and critical references (`core/templates/compact/prompt.md`). A second template (`summary_prefix.md`) frames how the **next** turn sees the summary — _"Another language model started to solve this problem and produced a summary..."_ — so the rebuilt history reads as a fresh handoff. Users can override via `Config.compact_prompt`.

Codex distinguishes **local** streaming summarization (same model, same reasoning effort, through `model_client.stream`) from **remote** server-side compaction (OpenAI / Azure providers post to `responses/compact` and get back the rewritten history).

After the local stream completes, `replace_compacted_history` rebuilds the in-memory history: walk **backward** collecting prior **user-text messages** under `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` (skipping anything that matches `is_summary_message` so prior summaries don't re-walk); assistant / tool-call / tool-output / reasoning blocks are dropped; final history is the retained user backlog plus `SUMMARY_PREFIX + summary_suffix`, all materialized as `role: user` `ResponseItem::Message`s. Persistence appends `RolloutItem::Compacted { message, replacement_history? }` to the rollout JSONL; the test suite covers `compact → shutdown → resume → fork`.

**Auto-compact** fires at `(context_window * 9) / 10` (90%), with `model_auto_compact_token_limit` config clamped to that ceiling. Two firing sites in `core/src/session/turn.rs`: pre-sampling before a new user turn enters `run_turn`, and mid-turn inside the sampling loop after `run_sampling_request` returns when the turn still `needs_follow_up`. Token signal is `Session::get_total_token_usage` — last API `total_tokens` plus client-side estimates. UX is the regular task-running spinner; post-success the thread renders a `ThreadItem::ContextCompaction` info line.

Failure: stream errors retry up to `stream_max_retries` with a `Reconnecting…` notice; `Interrupted` propagates as Cancelled; `ContextWindowExceeded` during the compact request itself trims oldest history rows when `turn_input_len > 1` else surfaces an error. `steer_input` rejects mid-compaction queueing with `NonSteerableTurnKind::Compact`.

## opencode (TypeScript + Solid)

`/compact` (alias `/summarize`) calls `sdk.client.session.summarize({ sessionID, modelID, providerID })` (`routes/session/index.tsx:538-563`). The slash handler accepts no free-text args; an optional `auto: bool` field on the payload distinguishes manual from auto. The HTTP handler runs `revert.cleanup`, then `SessionCompaction.create`, then `SessionPrompt.loop`.

Compaction runs as a **dedicated agent** named `compaction` — `mode: "primary"`, `hidden: true`, all tools `deny`-permissioned (`agent/agent.ts:227-241`). Its system prompt anchors the operation: _"Summarize only the conversation history you are given..."_ and crucially _"If the prompt includes a `<previous-summary>` block, treat it as the current anchored summary. Update it..."_ — the **anchored** pattern is opencode's signature. The user-facing rubric is a Markdown template with sections in order: Goal · Constraints · Progress · Key Decisions · Next Steps · Critical Context · Relevant Files; terse bullets, preserve exact paths / commands / error strings.

Input messages come from `select`: find the older prefix (head) and exclude ranges between previously-completed `compaction`+`summary` pairs so prior summaries aren't re-summarized. Tool outputs are truncated to 2,000 chars (`TOOL_OUTPUT_MAX_CHARS`); media stripped. Default model is the user's selected one unless `agent.compaction.model` is set.

Replacement keeps everything on the **same session**: append an empty `user` row with `type: "compaction"` part, then an `assistant` row with `summary: true, mode: "compaction"` carrying the text. `MessageV2.filterCompacted` makes the active context behave like `[compaction marker, summary, optional verbatim tail]` while older turns remain in storage. The compaction part also stores `tail_start_id` so forks find the boundary. After **auto** success, the `experimental.compaction.autocontinue` plugin can synthesize a `synthetic: true` continue prompt to drive the next turn without user input.

**Auto-compact** is `isOverflow({ tokens, model })` — `count >= usable(model)` where `count` is the last assistant `tokens.total` and `usable = limit.input - reserved`. Fires from two sites in `prompt.ts`: after a non-summary assistant turn finishes, and when the stream processor returns a `"compact"` signal from a `ContextOverflowError`. Config knobs (`compaction.auto`, `compaction.prune`, `compaction.tail_turns`, `compaction.preserve_recent_tokens`, `compaction.reserved`) plus env overrides `OPENCODE_DISABLE_AUTOCOMPACT` / `OPENCODE_DISABLE_PRUNE`. A separate `SessionCompaction.prune` pass stamps `part.state.time.compacted` on old tool outputs to wipe their bodies before any compaction LLM call — analogue of Claude Code's microcompact.

UX renders a top-bordered box captioned ` Compaction ` (or ` Auto Compaction `) above the user row with the summary markdown inside. Failure: still-too-large after stripping surfaces `ContextOverflowError` _"Session too large to compact - context exceeds model limit even after stripping media"_.

## Comparison

| Aspect              | Claude Code                                  | Codex (Rust)                                 | opencode                                 |
| ------------------- | -------------------------------------------- | -------------------------------------------- | ---------------------------------------- |
| Slash trigger       | `/compact [instructions]`                    | `/compact` (no args, not during task)        | `/compact`, alias `/summarize`           |
| Auto-compact        | `window - 13k`, default on, env opt-out      | 90% of window, default on                    | `usage ≥ usable`, default on             |
| Auto check site     | start of every `query()`                     | pre-sampling + mid-turn loop                 | post-finished-turn + mid-stream          |
| Prompt shape        | 9-section rubric, `<analysis>` + `<summary>` | short paragraph + `summary_prefix` for next  | Markdown template, anchored update       |
| Anchored re-compact | no                                           | no — prior summaries skipped on rewalk       | yes — `<previous-summary>` block         |
| Summarizer model    | same as main loop                            | same as main loop                            | configurable per `agent.compaction`      |
| Streaming           | yes (fork or fallback)                       | yes (local) / no (remote OpenAI / Azure)     | yes                                      |
| Replacement shape   | boundary + synthetic user msg + kept tail    | retained user msgs + summary as `role: user` | compaction marker + summary assistant    |
| Old transcript      | archived to segment; loader truncates        | replaced in memory; rollout records          | retained in DB; `filterCompacted` hides  |
| Persistence record  | `system / compact_boundary` JSONL line       | `RolloutItem::Compacted` + `TurnContextItem` | `compaction` part + `summary: true` row  |
| Tools / reasoning   | dropped (via microcompact)                   | dropped from rebuilt history                 | dropped via `tools: {}` + truncation     |
| Pre-compact pass    | microcompact (clears old tool bodies)        | none                                         | `prune` (stamps old tool outputs)        |
| Post-compact UI     | dim "Compacted" line + token warning bar     | "Context compacted" info line                | `Compaction` boxed pane with summary     |
| Hooks               | PreCompact / PostCompact / SessionStart      | PreCompact / PostCompact (can abort)         | plugin `experimental.session.compacting` |

## Patterns Worth Borrowing for oxide-code

1. **Optional free-text custom instructions on `/compact`.** Claude Code's `/compact <instructions>` lets the user steer the summary toward what they care about. Cheap to add, matches oxide-code's existing typed-arg shape (`/rename <title>`, `/model <id>`).
2. **Synthetic user message carrying the summary.** All three CLIs converge on materializing the result as `role: user` rather than `system` or `assistant` — sidesteps two API constraints (assistant can't lead a turn; system is one-shot at the prefix) and keeps the next turn shape exactly like a fresh first prompt.
3. **Boundary marker as a distinct entry type.** Claude Code's `compact_boundary` and Codex's `RolloutItem::Compacted` both let resume loaders find the cut point in O(line). Without it you can't tell what was compacted vs. authored. Record `pre_message_count` / `pre_tokens` on the marker — useful for the post-compact UI line and free at write time.
4. **Drop tool calls / tool results / reasoning blocks from the summarizer input.** Codex's pattern is the strictest — only user texts plus the new summary survive — and the simplest to reason about. Strip everything non-conversational and pass the resulting transcript to the summarizer.
5. **Hard ban tool calls during the compaction turn.** Claude Code's `NO_TOOLS_PREAMBLE` is forceful prose; opencode uses agent permissions; Codex achieves it via the rebuild path. Cleanest for oxide-code: pass an empty tool registry to the summarization request so the model can't even attempt one.
6. **`summary_prefix` framing on the synthesized message.** Codex's _"Another language model started to solve this problem..."_ and Claude Code's _"This session is being continued..."_ both prep the next-turn model to _use_ the summary rather than re-ask the user. Without a prefix the next turn often redundantly asks "what would you like me to do?".
7. **Refuse mid-turn (manual `/compact`).** All three CLIs treat manual compaction as "wait until the current turn ends." oxide-code's existing `Mutating` / `ReadOnly` classification already encodes this; `/compact` should be `Mutating`.
8. **Surface a post-compact system message in the chat.** All three CLIs leave a visible artifact. oxide-code already has `SystemMessageBlock` with a left-bar accent — perfect fit for a `Compacted X messages → 1 summary` line plus the rendered summary markdown.

## Patterns to Reject

1. **9-section structured rubric (Claude Code).** The Anthropic team is summarizing for resumption across an indefinite gap, across sessions, across humans. oxide-code's compaction is in-process and single-user — the rubric's "All user messages: List ALL user messages" and verbatim quote requirements bloat the summary 3-5×. A short directive (intent, decisions, code paths, next step, user constraints) gets the same outcome at one-third the size. Codex's prompt is the right reference shape.
2. **Anchored `<previous-summary>` re-compaction (opencode).** Clever for very long sessions, but the summarizer must distinguish "still true" from "stale" without the original, and quality slowly degrades through repeated rewrites. Defer until the second-compaction case hits in practice.
3. **Auto-compact mid-turn (Codex `MidTurn`).** Firing between sampling rounds inside an active turn requires hot-path token counting, pausing the in-flight reply, and a careful resume after history rebuild. Manual + auto-fire only at turn boundaries is enough.
4. **Microcompact / prune as a separate stage.** Claude Code's microcompact and opencode's prune delete tool result bodies in place. Useful at scale; expensive to ship correctly without metrics (which results, when, what placeholder, how it interacts with file-tracker state). Defer until auto-compact telemetry exists.
5. **Partial / range compaction (Claude Code).** "Summarize from here" / "Summarize up to here" needs a message-selector UI, a dual-direction summarizer, and a transcript splice. Land full-transcript compaction first.
6. **Hooks (PreCompact / PostCompact).** All three CLIs ship hooks. oxide-code doesn't have a hook surface yet; introducing one for compaction would commit the project to that abstraction. Defer with "user-extensible workflow skills" on the roadmap.
7. **Synthetic auto-continue user message (opencode).** Quietly inserts a fake "Continue if you have next steps..." after auto-compact. oxide-code should land the summary and _wait_ for the user. Surprises beat conveniences here.
8. **Pre-flight `count_tokens` API call.** Tempting for auto-compact threshold, but every reference CLI uses _last assistant `usage.total_tokens` + tail estimate_ and accepts the ~2-5% drift. Anthropic's `count_tokens` endpoint adds a round-trip per turn for marginal accuracy.
