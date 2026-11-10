# Compaction

Context compression replaces the visible transcript with a summary so a long session keeps working. `/compact [instructions]` runs it on demand, and automatic compaction runs it when the estimated size of the next request nears the active model's context window. Both share one summarizer, one persisted boundary, and one failure pause.

## Triggers

`/compact` accepts optional instructions that steer the summary, and the slash registry marks it idle-only, so a turn in flight finishes first. Auto-compaction is on by default and can be disabled, which manual `/compact` ignores.

Automatic compaction runs at two points, and never interrupts an in-flight stream or tool call:

- **Before a prompt.** Before recording a new prompt, the engine compacts when the previous turn left its estimate over the threshold, whether that turn completed, failed, or was cancelled.
- **Inside a turn.** At a round boundary where the next request would cross the threshold, the engine compacts and the turn continues from the summary.

A prompt that arrives during compaction stays queued and drains afterward, unless the compaction fails, in which case the queue waits for the user.

## Token Signal

The trigger estimates the size of the next request from the turn's latest request:

```text
input_tokens + cache_creation_input_tokens + cache_read_input_tokens + estimate(messages since that request)
```

`message_start` reports the input and cache counts, and the stream keeps the latest non-zero value of each, which the status-line context segment also shows. The messages appended since that request have no reported usage yet, so the trigger estimates them at four bytes of JSON per token, as Claude Code's `tokenCountWithEstimation` does.

A fresh, resumed, or compacted transcript has no request to measure from, so the trigger estimates the whole next request at the same rate: the system prompt, the user context, the tool definitions, and the transcript. It also uses this estimate when a turn reports no usage, such as after a context-limit rejection, so retained prompts still count toward compaction. The status-line segment keeps the latest reported total. A resumed session over the threshold therefore compacts before its first prompt.

## Threshold

Every supported model in `model.rs` has a `1_000_000`-token context window. Unknown model IDs have no window, so default and percentage-based auto-compaction stay disabled for them, while explicit token thresholds still apply.

```text
effective_window = context_window - min(max_tokens, 20_000)
threshold = effective_window - 13_000
```

The 20k summary reserve mirrors Claude Code's p99 summary-output headroom and keeps a compact request from firing at the hard limit. The 13k buffer leaves room for the next prompt, dynamic instructions, and small tool-schema drift. With the default `max_tokens`, supported models compact at `967_000` tokens. If the subtraction would underflow, auto-compaction stays disabled.

## Configuration

[`[client.compaction]`](../guide/configuration.md#clientcompaction-context-compression) lists the settings and how explicit thresholds snap into a usable range. They control only the automatic trigger. Token and percent thresholds are mutually exclusive, so the resolved trigger stays obvious, and the `50_000` floor exists because a lower threshold would summarize again and again long before context pressure exists.

## Summarizer

`request_summary` forks the live conversation: it sends the conversation's own system prompt, user context, tools, and transcript, appends the rubric `SUMMARIZATION_USER_RUBRIC` as the last user message, and reads the reply through the agent loop's `collect_response`. The rubric asks for a terse list of the intent, decisions, code paths touched, current state, and next step, and ends by asking for text only. Custom instructions follow it under an additional-instructions heading. Every request records its usage, even when the summary fails or the caller drops the future.

Its `SummaryStream` sink forwards the reply's text as `Event::SummaryToken`, passes retry notices and usage limits through, and drops thinking. A retry, a failure, or a dropped future emits `Event::SummaryDiscarded` once any text has streamed.

A request the API rejects with HTTP 400 `prompt is too long`, or a reply that stops on `model_context_window_exceeded`, gets up to three retries. Each retry copies the transcript without thinking blocks and drops its oldest API rounds until it frees the reported overflow plus about 20k tokens of output room for the summary.

The summarizer fails on any reply that does not end in a complete text summary: one cut short by `max_tokens`, a refusal, a tool call, a pause or unknown stop reason, a truncated stream, a whitespace-only summary, or an overflow that outlasts the retries or leaves only the newest round. It also refuses a transcript under about 1,000 tokens, an empty one included. Text, tool calls, and tool results count toward that floor, while the system prompt, instructions, and tool definitions do not, since a summary cannot shrink them.

## Boundary

`session/handle` exposes `compact(boundary, synthetic_message) -> CompactOutcome`, where `CompactBoundary` carries the summary, instructions, task list, and the session context the rebuilt prompt starts from. The session stores the synthetic post-compact message as one parentless message row, which becomes the head:

- **Message.** A `role: user` message holding `SUMMARY_PREFIX`, a blank line, and the summary, followed by reminders of the task list and the open background shells when there are any, and after a mid-turn compaction a reminder to resume the task.
- **`compact` column.** The `summary`, `pre_message_count`, optional `instructions`, and the `tasks`, omitted when empty.
- **`context` column.** The refreshed session context.

The rows before it stay in the store, off the new chain. On resume, the chain walk stops at the parentless root, so the loader reads only the post-compact tail. The root's `CompactInfo` lets the TUI render the boundary block while the model transcript keeps the synthetic root, and `session::sanitize` treats that root as an ordinary user text message.

`Engine::run_compaction` is the pipeline both paths run. On success, it reloads the instruction files and builds the synthetic continuation, and `LiveTranscript::compact` saves the boundary, clears the file tracker, replaces the in-memory messages, and adopts the refreshed prompt. The engine then emits `Event::SessionCompacted`, shows any instruction warning the rebuilt prompt newly raises, and ends a manual compaction with `OpEnded`.

- **Summarizer failure.** Emits `Event::TurnFailed` and leaves the transcript unchanged.
- **Boundary write failure.** Stops session persistence like any failed write, while the summary still replaces the transcript in memory, and the failure is reported after `SessionCompacted`.
- **Cancel or quit.** A cancelled compaction ends with `OpEnd::Cancelled` like a regular turn, and a quit ends it without an `OpEnded`.

Each summary request adds a `usage` entry whose estimated cost joins the session total whether the summary succeeds, fails, or is cancelled. An `Event::UsageUpdated` shows the new total after `SessionCompacted` on success, or before the operation ends when a failed or cancelled request reported usage.

## Trigger Flow

`Engine` owns the automatic trigger because it can compact before a new prompt is recorded or between the segments of a turn. Each run of `agent_turn` returns its latest usage and the estimate above in its `TurnReport`, including when the turn fails or is cancelled, and `Engine::account_usage` stores the estimate as the pending signal.

### Before a Prompt

1. When the next prompt arrives, `Engine::auto_compact` checks the stored estimate before `submit_prompt` records the prompt.
2. If the estimate crosses the threshold, `Engine::run_auto_compaction` emits `AutoCompactionStarted` and runs the pipeline, and the summary streams into the chat as it does for `/compact`. A transcript below the size floor is skipped without counting a failure, since the system prompt, instructions, and tools then fill most of the context.
3. On failure, the streamed summary is discarded, `AutoCompactFailures::fail` counts the failure, and `submit_prompt` emits `PromptUnsent` and then `TurnFailed` without recording or sending the prompt.

### Inside a Turn

1. After a tool round records its results and any queued prompts, `agent_turn` computes the estimate. When it crosses the threshold and `max_tool_rounds` leaves another round, the turn stops with `TurnStop::Compact` before its next request, so the transcript holds no unresolved tool call.
2. `Engine::submit_prompt` bills the rounds so far, adopts their task list, and calls `Engine::compact_mid_turn`, which runs the pipeline with the `mid_turn` trigger.
3. On success, the prompts queued during the summary are recorded after it, and `agent_turn` runs again from the next round. The rounds before the compaction still count toward `max_tool_rounds`.
4. On failure, the failure counts toward the pause, and the turn fails with `TurnFailed` and a hint to run `/compact` and then send a message to continue. A cancel ends the turn on the interruption marker, as at any other round boundary.

A refusal later in the turn rolls back only to the summary, which still holds the prompt, so its notice points to `/clear` in case the model keeps declining.

## Failure Reset

The failure counter lives on the engine's `UsageLedger` and starts each session at zero. Three consecutive automatic failures pause automatic attempts until a successful compaction, a model or effort change, `/clear`, `/archive`, or `/resume` resets the counter, which manual `/compact` does not consult. The pausing failure emits `AutoCompactionPaused` with the failure count, and a reset of the paused counter emits `AutoCompactionResumed`, which the TUI follows to show the pause in `/status`.

## Frontend Behavior

Manual and automatic compaction share one `CompactedBlock`, an accent-bar surface without a footer. Its `Compacted N messages into a summary` header comes from `pre_count`, with a `(focus: ...)` suffix when `instructions` were given, and the summary renders as Markdown below it. While the summary streams, `CompactedBlock::render_streaming` shows it below the chat under a `Summarizing the conversation...` header, laid out like the final block, and `Event::SummaryDiscarded` removes it. `App::apply_session_compacted` clears the chat and the modal stack, pushes the block, and keeps queued prompts. Resume renders the same block and hides the synthetic root.

When the attempt ahead of a prompt fails, chat shows `Auto-compaction failed: ...` with the summarizer's error, and on the preceding `PromptUnsent` the TUI hands the prompt back as [**The TUI owns the queue**](cancellation.md#design-decisions) describes. Sending it again retries compaction. During the pause, sends go out without compaction, and `/status` shows that auto-compaction is paused and points to `/compact`. A failed boundary write does not fail the attempt, so the prompt goes out after the boundary block and a `Session write failed: ...` notice.

During any compaction, the status bar shows `Compacting` and holds prompts submitted meanwhile, so a failed `/compact` or mid-turn attempt leaves them waiting for the user. Automatic `SessionCompacted` events keep the TUI busy until the queued prompt drains or the submission finishes. After a mid-turn compaction, the reply continues below the boundary block, and a later rollback hands back only the prompts drained after it. When a manual compact's `OpEnded` arrives, the TUI returns to idle and submits the queue head, so the prompt renders below the notices the rebuilt prompt raises, except after a cancel that lost the race to the swap, when it only returns to idle.

In headless runs, `driver::stdio::TextState` writes `Compacting the conversation to free up context` to stderr on `AutoCompactionStarted` and renders `SessionCompacted` as the block's header line, such as `Compacted 12 messages into a summary`.

## Design Decisions

1. **One path for manual and automatic compaction.** Both call `Engine::run_compaction`, so the summarizer, the boundary, and the failure handling have one implementation. Setting `client.compaction.auto_enabled = false` turns off only the automatic trigger.

2. **Auto-compaction is default on, with an opt-out.** Running out of context near the end of a long task costs the work done so far, which is worse than a well-marked summary boundary. The opt-out preserves user control, and manual `/compact` stays available either way.

3. **Response usage as the token signal.** The stream already carries usage, and only the messages appended since the latest request need an estimate. A count-tokens request would add latency and still be approximate once dynamic system context and tool definitions are included. Claude Code and opencode likewise read the last usage and estimate the tail.

4. **Reserve output headroom in the threshold.** Claude Code and opencode both compact below the advertised context limit, and the summarizer needs room to produce its summary. The 20k reserve and 13k buffer follow that floor.

5. **Boundary-only compaction.** Compaction runs after a coherent transcript unit is persisted, before the next prompt or the next round of a turn, once every call of the round has its result. It never interrupts a stream or a running tool, so the summarized transcript holds no unresolved call, and session replay matches manual `/compact`. The round boundary is a site because a long tool loop would otherwise fill the context window before its turn ends. Claude Code's microcompact and opencode's prune, which clear old tool-output bodies between summaries, would add a second retention policy with its own UI and persistence for a need these boundaries already cover.

6. **Refuse manual compaction mid-turn.** Claude Code, Codex, and opencode treat manual compaction as a between-turns operation. The slash dispatcher refuses an idle-only `/compact` while a turn is in flight, and the user retries after the reply finishes.

7. **Same request path as a turn.** The summarization request goes through the same `Wire` and `collect_response` as a turn, so it reuses the live model, effort and thinking settings, credentials, betas, and cache handling. `request_summary` checks the stop reason itself, so a summary cut off or stopped early fails before it can replace the transcript. Codex rewrites history server-side on its OpenAI and Azure paths, while this path needs only the Messages stream every supported endpoint already serves.

8. **Send the conversation's own request prefix.** The system prompt, tools, and message prefix match what the conversation sends, so the summarizer reads the conversation's prompt cache and pays cache-read rates for most of its input. Codex forks the same way.

   - **Preserve the prefix.** Editing the history first would miss that cache. On models that bind thinking blocks to the conversation prefix, such as Opus 5.5, it would also invalidate the thinking blocks the request replays, which enforced accounts reject with a 400.
   - **Keep the tools and ask for text only.** The tools array is part of the cached prefix. The rubric asks the model not to call any tool, and a reply that stops on `tool_use` fails the compaction.
   - **Replace history after success.** The summary replaces the whole history, so later requests replay no earlier thinking. The next compaction summarizes a transcript that already starts from this summary, so opencode's anchored re-compaction, which updates the previous summary in place, would add a second prompt path without new input.
   - **Trim on overflow.** Retries drop the oldest API rounds and strip every thinking block, since a new turn may omit earlier thinking but must not replay it after a changed prefix. A retry whose kept rounds end on a tool result closes that turn with a short assistant message before the rubric.
   - **Reserve summary room.** Claude Code's `truncateHeadForPTLRetry` frees just the reported overflow. oxide-code frees about 20k tokens beyond it so the retry leaves the summary room to finish, and marks no cache breakpoint on a transcript no later request reads.

9. **Synthetic post-compact user message with no parent.** Claude Code, Codex, and opencode all materialize the summary as a `role: user` message, since an assistant message cannot lead a turn and system blocks are special-cased at the prefix. Storing it with no parent lets the chain walk stop at the boundary with no special case. `SUMMARY_PREFIX` is a re-entry framing taken from the Codex template, which tells the next-turn model to use the summary and continue without re-asking what to do.

10. **The synthetic message as the stored boundary.** The synthetic root carries the `CompactInfo` and the refreshed prompt context, and the session's head moves onto it. The loader's chain walk stops at the root, keeps `CompactInfo` for resume display, and reads only the new tail, while the summarized rows stay for export. Claude Code's boundary entry and Codex's `RolloutItem::Compacted` serve the same purpose, and `pre_message_count` fills the post-compact header without a second field.

11. **Preserve session identity.** Claude Code, Codex, and opencode converge on this. `/clear` rolls because intent resets, while `/compact` keeps the session because intent is retained and only context is compressed. The session ID, its stored rows, project, and title all carry through unchanged.

12. **Reset the file tracker.** Edit and Write require a Read in the visible transcript, which is now the summary, so the previous reads are out of scope. The reset matches the post-`/clear` behavior and forces a fresh Read before any Edit.

13. **Keep queued prompts.** A session switch drops the prompts queued before it because they belong to the conversation that ended. `/compact` preserves the user's intent, so queued prompts still make sense after it.

14. **Stream the summary as it generates.** A long session's summary can take tens of seconds, and streaming it shows progress and lets the user judge it early. The engine streams the text and decides its fate, so every frontend sees one protocol. A discard precedes the event that reports a failure or cancellation, and a retry discards before streaming again. Headless text output omits the summary stream and keeps its one-line boundary notice.

15. **A failed attempt fails its prompt.** Sending the prompt against the unchanged transcript would put a request over the threshold on the wire, and the context would keep growing until it overflowed, the case auto-compaction exists for. The TUI hands the unsent prompt back and holds the queue, so the prompts behind it do not each start another summary request. A failed mid-turn attempt ends its turn for the same reason.

16. **A continuation only inside a turn.** After a compaction between prompts, queued input drains, and otherwise the assistant waits, since a synthetic continue prompt would make the agent act without fresh user intent. A mid-turn compaction resumes work the user already asked for, so its summary asks the model to resume the task. Claude Code and opencode add a continuation only where a turn is already running.

17. **Circuit-break automatic failures.** A compact request that is too large or malformed can be unrecoverable, so the pause keeps the engine from sending it with every prompt. Claude Code also stops after three consecutive failures.

18. **Free-text instructions, appended verbatim.** `/compact <instructions>` lets the user steer the summary, using the same typed-argument form as `/rename <title>` and `/model <id>`. Claude Code accepts the same argument and appends it under an additional-instructions heading after its rubric.

19. **Refuse a short transcript and an empty summary.** Below about 1,000 tokens the summary is about as long as the transcript, so the attempt fails and leaves the session untouched. A whitespace-only reply fails the same way, since retrying costs less than committing a useless summary.

20. **`CompactedBlock` is one chat block.** The accent-bar surface gives compaction a visual identity while keeping the header and summary one conceptual unit.

## Out of Scope

- **Partial and range compaction.** Claude Code's message-selector rewind adds a selector UI, a dual-direction summarizer, and a transcript splice for a case `/compact` already covers.
- **Compaction hooks.** Hooks belong to a general surface tracked in [#157](https://github.com/hakula139/oxide-code/issues/157), with command hooks in [#159](https://github.com/hakula139/oxide-code/issues/159) and the `pre_compact` event in [#168](https://github.com/hakula139/oxide-code/issues/168).

## Sources

- `crates/ox-core/src/config/compaction.rs`: `CompactionConfig`, which resolves the threshold and its snapping.
- `crates/ox-core/src/engine.rs`: `Engine::submit_prompt`, which compacts between the segments of a turn.
- `crates/ox-core/src/engine/compaction.rs`: `Engine::compact`, `Engine::run_compaction`, `Engine::replace_with_summary`, and `AutoCompactFailures`.
- `crates/ox-core/src/engine/compaction/summary.rs`: `request_summary`, the `SummaryStream` sink, and the context-window retries.
- `crates/ox-core/src/engine/estimate.rs`: `request_tokens`.
- `crates/ox-core/src/engine/history.rs`: `TranscriptWalker::push_message`, which skips the generated summary block when the compaction boundary already displays it.
- `crates/ox-core/src/engine/post_compact.rs`: `SUMMARY_PREFIX` and `synthesize_post_compact_message`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::compact`, which saves the boundary and swaps the transcript to the synthetic continuation.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, `TurnStop::Compact`, `collect_response`, and `TurnReport::context_estimate`.
- `crates/ox-core/src/engine/usage.rs`: `UsageLedger`, which accounts the session cost and context usage, and `Engine::account_usage` and `Engine::reset_context_usage`, which feed it.
- `crates/ox-core/src/protocol.rs`: `Op::Compact`, the `SessionCompacted`, `SummaryToken`, `SummaryDiscarded`, `AutoCompactionStarted`, and `AutoCompactionPaused` events, and `CompactTrigger`.
- `crates/ox-core/src/session/state.rs`: `SessionState::compact`, which queues the synthetic root.
- `crates/ox-core/src/session/store.rs`: `CompactInfo`, the record a compaction root stores.
- `crates/ox-core/src/session/store/load.rs`: the chain walk that stops at the root.
- `crates/ox-tui/src/components/chat/blocks/compacted.rs`: `CompactedBlock`.
