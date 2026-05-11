# Context Compression / `/compact` (Reference)

Research on how `/compact` (context compression via summarization) is implemented across Claude Code, OpenAI Codex, and opencode. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode) (v1.3.15).

Companions: [commands.md](commands.md). Storage-layer notes (JSONL entry shape, resume semantics) live in [session/persistence.md](../session/persistence.md); this file focuses on the _summarization_ surface — trigger, prompt, replacement strategy, auto-fire policy.

## Claude Code (TypeScript + Ink)

`/compact` is a `local`-type command: `Clear conversation history but keep a summary in context. Optional: /compact [instructions for summarization]` (`src/commands/compact/index.ts:4-13`). Custom args become the `customInstructions` argument to the summarizer (`src/commands/compact/compact.ts:40-107`). A separate **rewind UI** ([`MessageSelector`](https://github.com/hakula139/claude-code/blob/main/src/components/MessageSelector.tsx)) drives **partial compact** in two directions — `summarize` (from a selected anchor onward) and `summarize_up_to` (everything before the anchor) — sharing the same summarizer stack (`src/components/MessageSelector.tsx:31-33,189-197`).

The compaction prompt itself is the most elaborate of the three. **`BASE_COMPACT_PROMPT`** in `src/services/compact/prompt.ts:61-143` asks for an `<analysis>...</analysis>` followed by a `<summary>...</summary>` block with **nine numbered sections**: Primary Request and Intent, Key Technical Concepts, Files and Code Sections, Errors and Fixes, Problem Solving, **All user messages**, Pending Tasks, Current Work, and Optional Next Step (with verbatim quotes from recent messages to anchor "where we left off"). A `NO_TOOLS_PREAMBLE` (`prompt.ts:19-26`) hard-bans tool calls during the compaction turn — _"Tool calls will be REJECTED and will waste your only turn — you will fail the task."_ Custom user instructions are appended verbatim under an `Additional Instructions:` header before the trailer (`prompt.ts:269-302`).

Summarization runs on the **same model as the main loop** (`compact.ts:1308-1314`, streaming via `queryModelWithStreaming`). Two paths exist: a **fork-cache-sharing** path that reuses the parent thread's full system prompt and `cacheSafeParams` (cheaper because the prefix cache hits), and a **fallback** path with a minimal system prompt — _"You are a helpful AI assistant tasked with summarizing conversations."_ — and the rubric in the user message (`compact.ts:1302-1304`).

The summary returns as plain text containing `<analysis>` + `<summary>` blocks; `formatCompactSummary` strips the analysis and rewrites the summary into a flat `Summary:` block (`prompt.ts:311-334`). `getCompactUserSummaryMessage` wraps that in a **synthetic user message** — _"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation."_ (`prompt.ts:337-347`) — flagged `isCompactSummary: true, isVisibleInTranscriptOnly: true`.

The replacement is then materialized as `buildPostCompactMessages` (`compact.ts:326-337`):

```text
[boundaryMarker, ...summaryMessages, ...messagesToKeep, ...attachments, ...hookResults]
```

The boundary marker is a `system / subtype: 'compact_boundary'` message with `compactMetadata: { trigger: 'manual' | 'auto', preTokens, userContext, messagesSummarized }` and a `logicalParentUuid` linking to the last pre-compact message uuid (`utils/messages.ts:4530-4554`). Resume loaders use this boundary to truncate at the latest compact when the file exceeds `SKIP_PRECOMPACT_THRESHOLD`, and `parentUuid` chains reset across the boundary so the post-compact tail is its own DAG.

After replacement Claude Code does substantial **bookkeeping**:

- `readFileState` cache is **cleared** then file attachments are recreated under a budget (`compact.ts:517-538`).
- Plan-mode files, MCP instruction deltas, and deferred tool listings are re-attached (`compact.ts:545-585`).
- `preCompactDiscoveredTools` is recorded on the boundary for tool-schema continuity (`compact.ts:603-611`).
- Pre-compact hooks (`PreCompact`) and post-compact hooks (`PostCompact`) plus `SessionStart` hooks fire around the boundary (`compact.ts:591-594`); user hook stdout merges into the prompt as additional instructions.
- The pre-compact segment is archived via `writeSessionTranscriptSegment` (fire-and-forget) (`compact.ts:713-717`).

**Auto-compact** (`src/services/compact/autoCompact.ts:62-90`) computes the threshold as `effective_context_window - AUTOCOMPACT_BUFFER_TOKENS` where the buffer is **13,000 tokens** and the effective window subtracts the model's reserved output cap (`min(maxOutputTokens, 20_000)`). The check fires at the **start of every `query()`** after `snip` and `microcompact` (`query.ts:412-468`). Token counting is `tokenCountWithEstimation(messages) - snipTokensFreed` — last assistant `usage.total_tokens` from the API plus an estimate of messages added since (`utils/tokens.ts:202-256`). A circuit breaker disables auto-compact after 3 consecutive failures (`autoCompact.ts:257-265,334-349`). Opt-out: `DISABLE_COMPACT` / `DISABLE_AUTO_COMPACT` env vars and `autoCompactEnabled` config (default `true`).

A **microcompact** stage runs _before_ full compact (`src/services/compact/microCompact.ts`): clears specific tool result bodies (Read, shell family, Grep, Glob, Edit, Write, web fetch / search) older than a TTL, replacing them with `[Old tool result content cleared]`. Adds a `microcompact_boundary` marker. This delays full compact without touching the user-visible transcript.

UX: spinner cycles `Running PreCompact hooks…` → `Compacting conversation` → `Running PostCompact hooks…` (`screens/REPL.tsx:2497-2512`); post-success the chat shows a dim `Compacted` line plus `(shortcut to see full summary)` (`commands/compact/compact.ts:230-247`). Pre-compact, the `TokenWarning` component renders `X% until auto-compact` or `X% context used`. Failure surfaces: not enough messages → `ERROR_MESSAGE_NOT_ENOUGH_MESSAGES`; user abort during summarization → "Compaction canceled."; prompt-too-long on the compact request → up to 3 retries trimming oldest API rounds (`compact.ts:227-291`); auto failures suppressed from the user (`addErrorNotificationIfNeeded` skipped for auto, `compact.ts:749-755`).

## OpenAI Codex (Rust + Ratatui)

`/compact` is `SlashCommand::Compact` — _"summarize conversation to prevent hitting the context limit"_ (`codex-rs/tui/src/slash_command.rs:11-83`). It does **not** support inline args (`:142-157`); typed text after `/compact` is ignored at the slash layer. The command is `available_during_task: false` (`:173-195`) — refused while another task is in flight. TUI dispatch resets the token-usage HUD, forces `set_task_running(true)`, and emits `AppEvent::Compact` (`chatwidget/slash_dispatch.rs:186-191`); the app server routes that to `AppCommand::Compact` → `thread_compact_start` (`thread_routing.rs:630-632`).

The compaction prompt is **terse** (`codex-rs/core/templates/compact/prompt.md`):

> You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.
>
> Include:
>
> - Current progress and key decisions made
> - Important context, constraints, or user preferences
> - What remains to be done (clear next steps)
> - Any critical data, examples, or references needed to continue
>
> Be concise, structured, and focused on helping the next LLM seamlessly continue the work.

A second template, `summary_prefix.md`, frames how the **next** turn sees the summary:

> Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:

Both load via `include_str!` as `SUMMARIZATION_PROMPT` and `SUMMARY_PREFIX` (`codex-rs/core/src/compact.rs:46-47`). Users can override via `Config.compact_prompt` (`config/mod.rs:470-471`).

Codex distinguishes **local** vs **remote** compaction. For OpenAI / Azure providers `ModelProviderInfo::supports_remote_compaction()` returns true and Codex POSTs to `responses/compact` (non-streaming, server-side history rewrite) (`model-provider-info/src/lib.rs:392-394`, `core/src/client.rs:425-428`). All other providers run **inline streaming summarization** through the same `model_client.stream` path as a regular turn — same model, same reasoning effort (`compact.rs:532-552`).

After the local stream completes, `replace_compacted_history` rebuilds the in-memory history (`compact.rs:259-284`):

1. Take the last assistant text message of the compaction turn as the **`summary_suffix`**.
2. Final summary body = `SUMMARY_PREFIX + "\n" + summary_suffix`.
3. Walk **backward** through the prior transcript collecting **user-text messages** under a `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` budget (`compact.rs:48,389-527`); rows that match `is_summary_message` (prefix equality with `SUMMARY_PREFIX`) are skipped so prior summaries don't get re-walked.
4. Assistant messages, tool calls, tool outputs, and reasoning blocks from the prior transcript are **dropped**.
5. The new history is the retained user backlog + the prefixed summary, all materialized as **`role: user`** `ResponseItem::Message`s.

`InitialContextInjection` controls whether the compaction reuses or clears the prior `TurnContextItem`. Manual `/compact` uses `DoNotInject`, which clears the reference so the next regular turn fully re-injects environment / instructions (`compact.rs:50-62,112`). Mid-turn auto-compact uses `BeforeLastUserMessage` so the summary slots in just before the user's pending input.

For the **remote** path the server returns the post-compaction history; Codex filters it through `should_keep_compacted_history_item` which drops `developer`, most non-user `user` wrappers, **`Reasoning`**, tool calls / outputs, and web-search calls — keeping only `Compaction` / `ContextCompaction` markers (`compact_remote.rs:277-316`).

Persistence: `RolloutItem::Compacted(CompactedItem { message, replacement_history? })` is appended to the rollout JSONL alongside an optional `RolloutItem::TurnContext` (`protocol/src/protocol.rs:2764-2790,2800-2803`). On resume the loader replays compacted history; the test suite `compact_resume_fork.rs` covers `compact → shutdown → resume → fork`.

**Auto-compact** is keyed off `ModelInfo::auto_compact_token_limit()` — defaults to **`(context_window * 9) / 10`** (90%), with optional `model_auto_compact_token_limit` config override clamped to that 90% ceiling (`protocol/src/openai_models.rs:295-333`, `models-manager/src/model_info.rs:38-39`). Two firing sites in `core/src/session/turn.rs`:

- **Pre-sampling**, before a new user turn enters `run_turn` (`turn.rs:154-167,721-755`). Condition: `sess.get_total_token_usage().await >= auto_compact_limit`. (Comment notes preemptive compaction for _pending_ size is not yet implemented.)
- **Mid-turn**, inside the sampling loop after `run_sampling_request` returns; if usage exceeds the limit and the turn still `needs_follow_up`, `run_auto_compact` runs with `CompactionPhase::MidTurn` (`turn.rs:474-511`).

A third path, `maybe_run_previous_model_inline_compact`, fires when the model is downshifted to one with a smaller window (`turn.rs:764-792`).

Token signal is `Session::get_total_token_usage` → `history.get_total_token_usage` — the last API response's `total_tokens` plus client-side estimates for items added since (`context_manager/history.rs:309-326`). UX during compaction is the same task-running spinner as a regular turn; on success the rebuilt thread renders a `ThreadItem::ContextCompaction` info line _"Context compacted"_ (`chatwidget.rs:6006-6007`). Inline compaction emits a post-success `EventMsg::Warning` carrying a long-thread cautionary message (`compact.rs:290-292`).

Failure: stream errors retry up to `stream_max_retries` with a `Reconnecting…` notice (`compact.rs:187-247`); `CodexErr::Interrupted` propagates immediately as Cancelled; `ContextWindowExceeded` during the compact request itself trims oldest history rows when `turn_input_len > 1` else surfaces an error and `set_total_tokens_full`. PreCompact hooks can `continue: false` to abort with `CodexErr::TurnAborted` (`compact.rs:141-148`). `steer_input` rejects mid-compaction queueing with `NonSteerableTurnKind::Compact` (`session/mod.rs:3028-3038`).

## opencode (TypeScript + Solid)

`/compact` is registered as a TUI / web command — alias `/summarize` — that calls `sdk.client.session.summarize({ sessionID, modelID, providerID })` (`packages/opencode/src/cli/cmd/tui/routes/session/index.tsx:538-563`, `packages/app/src/pages/session/use-session-commands.tsx:333-414`). The slash handler accepts no free-text args; the optional `auto: bool` field on `SummarizePayload` distinguishes manual from auto-fired calls (`packages/opencode/src/server/routes/instance/httpapi/groups/session.ts:61-65`). The HTTP handler runs `revert.cleanup`, then `SessionCompaction.create({ ..., auto })`, then `SessionPrompt.loop` (`handlers/session.ts:235-255`).

Compaction runs as its own **dedicated agent** named `compaction` — `mode: "primary"`, `native: true`, `hidden: true`, all tools `deny`-permissioned (`packages/opencode/src/agent/agent.ts:227-241`). Its system prompt (`packages/opencode/src/agent/prompt/compaction.txt`):

> You are an anchored context summarization assistant for coding sessions.
>
> Summarize only the conversation history you are given. The newest turns may be kept verbatim outside your summary, so focus on the older context that still matters for continuing the work.
>
> If the prompt includes a `<previous-summary>` block, treat it as the current anchored summary. Update it with the new history by preserving still-true details, removing stale details, and merging in new facts.
>
> Always follow the exact output structure requested by the user prompt. ...

The user-facing rubric is a Markdown template (`session/compaction.ts:43-135`) requesting these sections in order: Goal · Constraints & Preferences · Progress (Done / In Progress / Blocked) · Key Decisions · Next Steps · Critical Context · Relevant Files. Rules: keep every section even when empty, terse bullets, preserve exact paths / commands / error strings, never mention the summary process. The **anchored** pattern is opencode's signature: when a prior summary exists `buildPrompt` wraps it in `<previous-summary>...</previous-summary>` and asks the model to _update_ — preserve still-true, remove stale, merge new — rather than regenerate from zero.

Input messages are computed by `select` which finds the older prefix (head) and excludes the message ranges between previously-completed `compaction`+`summary` pairs so old summaries are not re-summarized as conversation. The head plus the rubric becomes the LLM call; tool outputs are truncated to 2,000 chars (`TOOL_OUTPUT_MAX_CHARS`) and media is stripped (`compaction.ts:410-463`). The model defaults to the user's currently-selected model unless `agent.compaction.model` is set in config.

Replacement keeps everything on the **same session**: an empty `user` row with a `type: "compaction"` part is appended, then an `assistant` row with `summary: true, mode: "compaction"` carrying the generated text (`compaction.ts:417-442,586-608`). `MessageV2.filterCompacted` then makes the active context behave like `[compaction marker, summary, optional verbatim tail]` while older turns remain in storage for replay / inspection (`message-v2.ts:1071-1121`). When the model issues a downstream chat request the compaction marker on the user row is rewritten to the literal _"What did we do so far?"_ (`message-v2.ts:793-798`). After **auto** compaction success the `experimental.compaction.autocontinue` plugin can synthesize a `synthetic: true` user message — _"Continue if you have next steps, or stop and ask for clarification..."_ — to immediately drive the next turn without user input (`compaction.ts:512-561`). The compaction part also stores `tail_start_id` so forks can find the boundary.

UX in the TUI: a top-bordered box captioned ` Compaction ` (or ` Auto Compaction `) appears above the user row, with the rendered summary markdown inside (`feature-plugins/system/session-v2.tsx:227-255`). Tip-view text mentions `/compact` as a hint. The Solid web app renders the same shape via the shared `compaction` part mapping.

**Auto-compact** decision is **boolean**: `isOverflow({ tokens, model })` returns true when `count >= usable(model)` where `count` is the assistant's last `tokens.total` and `usable = limit.input - reserved` (or `context - maxOutputTokens` when `limit.input` is absent) — `reserved` defaults to `min(20_000, maxOutputTokens)` (`overflow.ts:6-26`). Fires from two sites in `prompt.ts`: after a non-summary assistant turn finishes (`prompt.ts:1503-1510`), and when the processor returns a `"compact"` signal because a `ContextOverflowError` interrupted the stream (`prompt.ts:1633-1642`). On the second path `auto: true, overflow: true` is forwarded so the synthetic-continue copy mentions media-attachment overflow.

Config knobs (`config.ts:266-284,742-747`): `compaction.auto` (default true), `compaction.prune` (drop old tool outputs proactively), `compaction.tail_turns` (verbatim recent turns to keep, default 2), `compaction.preserve_recent_tokens` (token cap for the tail), `compaction.reserved` (buffer for the auto threshold). Env overrides: `OPENCODE_DISABLE_AUTOCOMPACT`, `OPENCODE_DISABLE_PRUNE`. The compaction agent has a separate model knob via `agent.compaction.model`.

A second pass, `SessionCompaction.prune`, walks backward and stamps `part.state.time.compacted` on old completed tool outputs to wipe their bodies before they hit any compaction LLM call — protected by `PRUNE_MINIMUM` / `PRUNE_PROTECT` constants (`compaction.ts:302-348`). This is opencode's analogue of Claude Code's microcompact.

Failure: still-too-large after stripping returns `"compact"` again with `ContextOverflowError` _"Session too large to compact - context exceeds model limit even after stripping media"_ (`compaction.ts:465-474`). Tool calls during a `summary: true` assistant message throw (`processor.ts:286-289,334-336`).

## Comparison

| Aspect              | Claude Code                                   | Codex (Rust)                                   | opencode                                |
| ------------------- | --------------------------------------------- | ---------------------------------------------- | --------------------------------------- |
| Slash trigger       | `/compact [instructions]`                     | `/compact` (no args)                           | `/compact`, alias `/summarize`          |
| Custom instructions | optional free-text args                       | not supported                                  | not supported                           |
| Auto-compact        | `window - 13k`, default on, env opt-out       | 90% of window, default on, config clamp        | usage ≥ usable budget, default on       |
| Auto check site     | start of every `query()`                      | pre-sampling + mid-turn loop                   | post-finished-turn + mid-stream         |
| Token signal        | last API usage + estimate of tail             | last API total + client estimate               | last assistant `tokens.total`           |
| Prompt shape        | 9-section rubric, `<analysis>`+`<summary>`    | 7-line concise paragraph                       | Markdown template, anchored             |
| Anchored re-compact | no — boundary marker chains forward only      | no — `is_summary_message` skip on rewalk       | yes — `<previous-summary>` block        |
| Summarizer model    | same as main loop                             | same as main loop                              | configurable per `agent.compaction`     |
| Streaming           | yes (fork or fallback)                        | yes (local) / no (remote OpenAI / Azure)       | yes                                     |
| Replacement shape   | boundary + synthetic user msg + kept tail     | retained user msgs + summary as user msg       | compaction marker + summary assistant   |
| Old transcript      | archived to segment file; loader truncates    | replaced in memory; rollout keeps record       | retained in DB; `filterCompacted` hides |
| Persistence record  | `system / compact_boundary` JSONL line        | `RolloutItem::Compacted` + `TurnContextItem`   | `compaction` part + `summary: true` row |
| Resume sees         | post-boundary tail; full body via transcript  | rebuilt history; tests cover compact+resume    | active = filtered; full = stored        |
| Tools / reasoning   | dropped from summary input via microcompact   | dropped from rebuilt history                   | dropped via `tools: {}` + truncation    |
| Refuse mid-tool     | yes (auto fires only at `query()` boundary)   | no — mid-turn auto path is explicit            | no — overflow mid-stream triggers it    |
| Pre-compact pass    | microcompact (clears old tool result bodies)  | none                                           | `prune` (stamps old tool outputs)       |
| Post-compact UI     | dim "Compacted" line + token warning bar      | "Context compacted" info line + warning event  | `Compaction` boxed pane with summary    |
| Synthetic continue  | no — user prompts again                       | next turn drives normally                      | optional plugin auto-continue           |
| Hooks               | PreCompact / PostCompact / SessionStart       | PreCompact / PostCompact (can abort)           | plugin `experimental.session.compacting`|
| Custom instructions | appended after rubric                         | (none)                                         | plugin can replace prompt entirely      |
| Failure on retry    | up to 3 retries trimming oldest API rounds    | stream retry + ContextWindowExceeded trim      | returns ContextOverflowError to user    |
| Cancel mid-compact  | "Compaction canceled."                        | propagates as Interrupted                      | participates in session cancel          |

## Patterns Worth Borrowing for oxide-code

1. **Manual-first, auto later.** All three CLIs ship both manual and automatic paths, but the manual `/compact` is the simpler primitive — it doesn't need a token-budget oracle, doesn't fire mid-stream, and the user can always recover by retyping. Land manual cleanly, then layer auto on top once the threshold math has a real test bed. Roadmap entry "Context Compression" already separates these.

2. **Optional free-text custom instructions on `/compact`.** Claude Code's `/compact <instructions>` lets the user steer the summary toward what they care about (e.g. _"focus on the build error and how we fixed it"_). Cheap to add, genuinely useful, matches oxide-code's existing typed-arg shape (`/rename <title>`, `/model <id>`).

3. **Synthetic user message carrying the summary.** All three CLIs converge on materializing the compaction result as a `role: user` message rather than `system` or `assistant`. This sidesteps two API constraints: assistant messages can't lead a turn, and most providers cap or special-case `system` to one block at the prefix. A user-role wrapper around the summary keeps the next turn shape exactly like a fresh first prompt.

4. **Boundary marker as a distinct entry type.** Claude Code's `compact_boundary` system message and Codex's `RolloutItem::Compacted` both let resume loaders find the cut point in O(line). Resume can choose between _full transcript_ and _post-boundary only_; without this you can't tell what was compacted vs. authored.

5. **Drop tool calls / tool results / reasoning blocks from the summarizer input.** The Codex pattern is the strictest — only user texts plus the new summary survive — and the simplest to reason about. Claude Code's microcompact does this in two passes; opencode does it via `tools: {}` plus truncation. For v1, strip everything non-conversational and pass the resulting transcript to the summarizer.

6. **Same model + streaming via the existing client.** No reason to introduce a separate code path for the summarization request. Reuse the live `Client::stream`, the live model selection, the live thinking/effort settings. opencode's "compaction agent has its own config knob" is a nice extension once a user complains, but `agent.compaction.model` doesn't need to ship in v1.

7. **Hard ban tool calls during the compaction turn.** Claude Code's `NO_TOOLS_PREAMBLE` is forceful prose that materially changes behavior; opencode achieves the same via agent permissions; Codex achieves it because the rebuild path drops anything that isn't text anyway. Cleanest for oxide-code: pass an empty tool registry to the summarization request so the model can't even attempt one.

8. **Summary materialization framing as `summary_prefix`.** Codex's `summary_prefix.md` ("_Another language model started to solve this problem..._") and Claude Code's _"This session is being continued..."_ both prep the next-turn model to _use_ the summary instead of re-asking the user. The phrasing matters: without a prefix the next turn often redundantly asks "what would you like me to do?".

9. **Boundary marker carries `pre_tokens` and `messages_summarized`.** Claude Code records both. Useful for the post-compact UI line ("Compacted N messages, X tokens → Y tokens"), useful for telemetry, free at write time.

10. **Refuse mid-turn (manual `/compact`).** All three CLIs treat manual compaction as "wait until the current turn ends." oxide-code's existing `Mutating` / `ReadOnly` slash classification already encodes this; `/compact` should be `Mutating`.

11. **Reset the file tracker.** Claude Code clears `readFileState` on compact and rebuilds attachments; oxide-code's `roll` and `roll_into` already do exactly this. The compaction path should reuse the same primitive.

12. **Surface a post-compact system message in the chat.** All three CLIs leave a visible artifact ("Compacted", "Context compacted", boxed `Compaction` pane). oxide-code already has `SystemMessageBlock` with a left-bar accent — perfect fit for a `Compacted X messages → 1 summary` line plus the rendered summary markdown.

## Patterns to Reject

1. **9-section structured rubric (Claude Code).** The Anthropic team is summarizing for _resumption across an indefinite gap including across sessions and across humans_. oxide-code's compaction is in-process and single-user — the rubric's _"All user messages: List ALL user messages that are not tool results"_ and the demand for verbatim quotes from recent messages bloat the summary 3-5×. A short directive ("focus on intent, decisions, code paths, next step, and any user constraints") gets the same outcome at one-third the size. Codex's prompt is the right reference shape.

2. **Anchored `<previous-summary>` re-compaction (opencode).** Genuinely clever for very long sessions, but it adds two complications: the summarizer must distinguish "still true" from "stale" without re-reading the (gone) original, and the summary slowly degrades through repeated rewrites. Defer until users hit the second-compaction case in practice.

3. **Auto-compact mid-turn (Codex `MidTurn`).** Codex fires auto-compact between sampling rounds inside an active turn. The complexity-to-payoff ratio is poor: it requires hot-path token counting, a way to pause the in-flight reply, and a careful resume after the rebuilt history. Manual `/compact` between turns + auto-fire only at turn boundaries is enough.

4. **Microcompact / prune as a separate stage.** Claude Code's microcompact and opencode's prune both _delete tool result bodies_ in place to delay the full compact. Useful at scale; expensive to ship correctly without a real metric (which tool results to clear, when, with what placeholder, how it interacts with file-tracker state). Defer until the auto-compact telemetry exists.

5. **Remote provider-side compaction (Codex).** OpenAI's `responses/compact` endpoint is genuinely useful but Anthropic doesn't expose an analogue. N/A.

6. **Anchored summary "agent" with its own permission set (opencode).** Useful in opencode's hosted multi-agent model. oxide-code already has a single `Client` and a single tool registry — passing an empty registry achieves the same lockdown without a new abstraction.

7. **Synthetic auto-continue user message (opencode).** Plugin-driven; quietly inserts a fake "Continue if you have next steps..." after auto-compact. oxide-code's compaction should land the summary and then _wait_ — the user types the next prompt. Surprises beat conveniences here.

8. **Partial / range compaction (Claude Code).** "Summarize from here" / "Summarize up to here" needs a message-selector UI, a dual-direction summarizer, and a splice operation in the transcript. Land full-transcript compaction first; partial is its own feature.

9. **Hooks (PreCompact / PostCompact).** All three CLIs ship hooks. oxide-code doesn't have a hook surface yet; introducing one for compaction would commit the project to that abstraction. Defer with the rest of "user-extensible workflow skills" on the roadmap.

10. **Pre-flight `count_tokens` API call.** Tempting for the auto-compact threshold check, but every reference CLI uses _last assistant `usage.total_tokens` + tail estimate_ and accepts the ~2-5% drift. Anthropic's `count_tokens` endpoint adds a round-trip per turn for marginal accuracy. Use the existing `usage` field on `MessageStop` events when auto-compact eventually ships.

11. **Two-stage warning UI (Claude Code's "X% until auto-compact").** Cute but requires the auto-compact threshold + a percent-of-window calculation in the status line. Tied up with the deferred status-bar redesign on the roadmap. v1 manual `/compact` doesn't need it.
