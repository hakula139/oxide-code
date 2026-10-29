# Compaction

Context compression replaces the visible transcript with a summary so a long session keeps working. `/compact [instructions]` runs it on demand, and automatic compaction runs it when the estimated size of the next request nears the active model's context window. Both share one summarizer, one persisted boundary, and one failure pause.

## Triggers

Manual compaction runs only when asked for. `/compact` accepts optional instructions that steer the summary, and the slash registry marks it idle-only, so a turn in flight finishes first. Auto-compaction is default on and can be disabled independently, and manual `/compact` ignores that setting.

Before recording a new prompt, the engine runs automatic compaction when the previous turn left its estimate over the threshold, whether that turn completed, failed, or was cancelled. Inside a turn, it runs at a round boundary where the next request would cross the threshold, and the turn then continues from the summary. It never interrupts an in-flight stream or tool call. A prompt that arrives during compaction stays queued and drains afterward, unless the compaction fails, in which case the queue waits for the user.

## Token Signal

The trigger estimates the size of the next request from the latest request of the turn:

```text
input_tokens + cache_creation_input_tokens + cache_read_input_tokens + estimate(messages since that request)
```

`message_start` reports the input and cache counts, and the stream keeps the latest non-zero value for each field. The status-line context segment shows the same input total. The messages appended since that request, its reply, tool results, queued prompts, and any interruption marker, have no reported usage yet, so the trigger estimates them at four bytes of JSON per token, as Claude Code's `tokenCountWithEstimation` does.

A fresh, resumed, or compacted transcript has no request to measure from. The trigger estimates the whole next request at the same four bytes per token: the system prompt, the user context, the tool definitions, and the transcript. It also uses this estimate when a turn reports no usage, including a context-limit rejection, so retained prompts still count toward compaction before the next submission. The status-line context segment keeps the latest reported input total. A resumed session over the threshold therefore compacts before its first prompt.

## Threshold

Every supported model in `model.rs` has a `1_000_000`-token context window. Unknown model IDs have no window, so default and percentage-based auto-compaction stay disabled for them. Explicit token thresholds still apply.

The threshold is:

```text
effective_window = context_window - min(max_tokens, 20_000)
threshold = effective_window - 13_000
```

The 20k summary reserve mirrors Claude Code's p99 summary-output headroom and keeps a compact request from firing at the hard limit. The 13k buffer leaves room for the next prompt, dynamic instructions, and small tool-schema drift. With the default `max_tokens`, supported models compact at `967_000` tokens. If the subtraction would underflow, auto-compaction stays disabled.

## Configuration

[`[client.compaction]`](../guide/configuration.md#clientcompaction-context-compression) lists the settings and how explicit thresholds snap into a usable range. They control only whether and where automatic compaction triggers, and manual `/compact` ignores them. Token and percent thresholds are mutually exclusive, so the resolved trigger stays obvious, and the `50_000` floor exists because a lower threshold would summarize again and again long before context pressure exists.

## Summarizer

`request_summary` forks the live conversation. It sends the conversation's own system prompt, user context, tools, and transcript, appends the rubric as the last user message, and reads the reply through the agent loop's `collect_response`. The rubric, `SUMMARIZATION_USER_RUBRIC`, asks for a terse list of the intent, decisions, code paths touched, current state, and next step, and ends by asking for text only. Custom instructions follow it under an additional-instructions heading. Every request it sends records its usage, even when the summary fails or the caller drops the future.

Its `SummaryStream` sink forwards the reply's text as `Event::SummaryToken` for manual and automatic compaction alike, passes retry notices and usage limits through, and drops its thinking. A retry, a failure, or a dropped future emits `Event::SummaryDiscarded` when any text has streamed.

A request the API rejects with HTTP 400 `prompt is too long`, or a reply that stops on `model_context_window_exceeded`, gets up to three retries. Each retry copies the transcript without thinking blocks and drops its oldest API rounds until it frees the overflow the error reports plus about 20k tokens of output room for the summary. The summarizer rejects a reply cut short by `max_tokens`, a refusal, a tool call, a pause or an unknown stop reason that ends it early, a truncated stream, or an overflow that outlasts the retries or leaves only the newest round. It also refuses an empty transcript, one under about 1,000 tokens, estimated at four bytes of JSON per token, and a whitespace-only summary. Text, tool calls, and tool results all count toward that floor, while the system prompt, instructions, and tool definitions do not, since a summary cannot shrink them.

## Boundary

`session/handle` exposes `compact(boundary, synthetic_message) -> CompactOutcome`, where `CompactBoundary` carries the summary, instructions, task list, and the session context the rebuilt prompt starts from. The actor stores the synthetic post-compact message as one message row with no parent, which becomes the session's head. The message is a `role: user` carrying `SUMMARY_PREFIX + summary`, plus reminders of the task list and the open background shells when there are any, and after a mid-turn compaction a reminder to resume the task. Its row's `compact` column holds the `summary`, `pre_message_count`, optional `instructions`, and the `tasks` (omitted when empty), and its `context` column holds that session context. The rows before it stay in the store, off the new chain.

On resume, the chain walk from the head stops at the synthetic root because it has no parent, so the loader reads only the post-compact tail. The root's `CompactInfo` lets the TUI render the boundary block while the model transcript keeps the synthetic root, and its context is the newest one on the chain. `session::sanitize` treats the synthetic continuation as an ordinary `role: user` text message.

`Engine::run_compaction` is the pipeline both paths run. On success, it reloads the instruction files and hands the summary, the session's task list, and the refreshed session context to `Engine::replace_with_summary`, which builds the synthetic continuation and passes it to `LiveTranscript::compact`. That method saves the boundary, then clears the file tracker, replaces the in-memory messages, and adopts the refreshed prompt. The engine emits `Event::SessionCompacted { summary, pre_count, instructions, trigger }`, shows any instruction warning the rebuilt prompt newly raises, and ends the compaction with `SessionReady`. A summarizer failure emits `Event::TurnFailed` and leaves the transcript unchanged. A failed boundary write stops session persistence like any failed write, but the summary still replaces the transcript in memory, and the failure is reported after `SessionCompacted`. Only a session writer that is gone discards the summary, with `Event::SummaryDiscarded`, and fails the compaction with the transcript unchanged. Cancellation emits `Event::Cancelled` like a regular turn, and a quit ends the compaction without a terminal event.

Each summary request is recorded as a `usage` entry, and its estimated cost joins the session total whether the summary succeeds, fails, or is cancelled. An `Event::UsageUpdated` then shows the new total, between `SessionCompacted` and `SessionReady` on success, and before the terminal event when a failed or cancelled summary request reported usage.

## Trigger Flow

`Engine` owns the automatic trigger because it can compact before a new prompt is recorded or between the segments of a turn. Each run of `agent_turn` reports its usage and the estimate above.

### Before a Prompt

1. `agent_turn` streams a turn, persists the transcript tail, and returns the latest token usage in its `TurnReport`, including when the turn fails or is cancelled.
2. `Engine::account_usage` stores the report's estimate of the next request as the pending automatic trigger signal.
3. When the next prompt arrives, `Engine::auto_compact` checks the stored estimate before `submit_prompt` records the prompt.
4. If the estimate crosses the threshold, it runs `Engine::run_compaction`. A transcript below the size floor is skipped without counting a failure, since the system prompt, instructions, and tools then fill most of the context and a summary would not shrink the request.
5. `Engine::run_auto_compaction` emits `AutoCompactionStarted` so the TUI can show compaction status while the summarizer runs, and the summary streams into the chat as it does for `/compact`.
6. On success, the pipeline replaces the transcript with the summary as described above and emits `SessionCompacted`.
7. On failure, the streamed summary is discarded, `AutoCompactFailures::fail` increments the failure counter, and `submit_prompt` emits `PromptUnsent`, then ends with `TurnFailed` without recording or sending the new prompt.

### Inside a Turn

1. After a tool round records its results and any queued prompts, `agent_turn` computes the estimate above. When it crosses the threshold and `max_tool_rounds` leaves another round, the turn stops with `TurnStop::Compact` before its next request, so the transcript holds no unresolved tool call.
2. `Engine::submit_prompt` bills the rounds so far, adopts their task list, and calls `Engine::compact_mid_turn`, which runs the same pipeline with the `mid_turn` trigger.
3. On success, the summary message ends with a reminder that tells the model to resume its task, and the prompts queued during the summary are recorded after it. `agent_turn` then runs again from the next round, and the rounds before the compaction still count toward `max_tool_rounds`.
4. On failure, the failure counts toward the pause, and the turn ends with `TurnFailed` and a hint to run `/compact`, then send a message to continue. A cancel ends the turn on the interruption marker, as at any other round boundary.

A refusal later in the turn rolls back only to the summary, which still holds the prompt, so its notice points to `/clear` in case the model keeps declining.

## Failure Reset

The failure counter lives on the engine's `UsageLedger`, which starts each session at zero. Three consecutive automatic failures pause further automatic attempts until a successful compaction, a model or effort change, `/clear`, `/archive`, or `/resume` resets the counter. Manual `/compact` does not consult it. The failure that pauses it emits `AutoCompactionPaused` with the failure count, and a reset of the paused counter emits `AutoCompactionResumed`, which the TUI follows to show the pause in `/status`.

## Frontend Behavior

Manual and automatic compaction use the same visible `CompactedBlock`. While the summary streams, the TUI shows it below the chat under a `Summarizing the conversation...` header through `CompactedBlock::render_streaming`, laid out like the block it becomes, and `Event::SummaryDiscarded` removes it. `App::apply_session_compacted` clears the chat, pushes a single `CompactedBlock`, keeps queued prompts, and clears the modal stack. The block has an accent-bar surface and no footer. It fills its `Compacted N messages into a summary` header from `pre_count`, adding a `(focus: ...)` suffix when `instructions` were given, and renders `summary` as Markdown below it. Resume renders the same boundary block and skips the synthetic root in visible chat.

When the attempt ahead of a prompt fails, the prompt is not sent, and chat shows why. A summarizer failure reads `Auto-compaction failed: ...`, and a session writer that is gone reads `Auto-compaction was not saved`. On the `PromptUnsent` event that precedes the failure, the TUI hands the prompt back as [**The TUI owns the queue**](cancellation.md#design-decisions) describes. Sending the prompt again retries compaction. During the pause, the next send goes out without compaction, and `/status` shows that auto-compaction is paused and points to `/compact`. A failed boundary write does not fail the attempt, so the prompt goes out after the boundary block and a `Session write failed: ...` notice.

During a manual or automatic compaction, the TUI status bar shows the `Compacting` state and holds prompts submitted meanwhile, so a failed `/compact` or a failed mid-turn attempt leaves them waiting for the user. Automatic `SessionCompacted` events keep the TUI busy until the queued prompt drains or the prompt submission finishes. After a mid-turn compaction, the reply continues below the boundary block, and the summary stands in for the prompt and the prompts the turn drained before it, so a later rollback hands back only the prompts drained after it. At `Event::SessionReady`, a manual compact returns to idle and submits the queue head, so the prompt renders below the notices the rebuilt prompt raises, or only returns to idle after a cancel that lost the race to the swap.

In the bare REPL and headless runs, `driver::stdio::StdioSink` writes `Compacting the conversation to free up context` to stderr on `AutoCompactionStarted`, then renders `SessionCompacted` as a stderr line such as `Compacted 12 messages into a summary`, the header the TUI block shows.

## Design Decisions

1. **One path for manual and automatic compaction.** Both call `Engine::run_compaction`, so the summarizer, the boundary, and the failure handling have one implementation. Setting `client.compaction.auto_enabled = false` turns off only the automatic trigger.

2. **Auto-compaction is default on, with an opt-out.** Running out of context is worse than a well-marked summary boundary, and running out near the end of a long task costs the work done so far. A separate opt-out preserves user control, and manual `/compact` stays available either way.

3. **Response usage as the token signal.** The stream already carries usage, and only the messages appended since the latest request need an estimate. A count-tokens request would add latency and still be approximate once dynamic system context and tool definitions are included. Claude Code and opencode likewise read the last usage and estimate the tail.

4. **Reserve output headroom in the threshold.** Claude Code and opencode both compact below the advertised context limit, and the summarizer needs room to produce its summary. The 20k reserve and 13k buffer follow that floor.

5. **Boundary-only compaction.** Compaction runs between persisting a coherent transcript unit and starting the next prompt or the next round of a turn, once every call of the round has its result recorded. It never interrupts a stream or a running tool, so the summarized transcript holds no unresolved call, and session replay matches manual `/compact`. The round boundary is a compaction site because a long tool loop would otherwise fill the context window before its turn ends.

6. **Refuse manual compaction mid-turn.** Claude Code, Codex, and opencode treat manual compaction as a between-turns operation. The slash dispatcher refuses an idle-only `/compact` while a turn is in flight, and the user retries after the reply finishes.

7. **Same request path as a turn.** The summarization request goes through the same `Wire` and `collect_response` as a turn, so it reuses the live model, effort and thinking settings, credentials, betas, and cache handling. `request_summary` checks the stop reason itself, so a summary cut off by `max_tokens` or stopped early fails before it can replace the transcript.

8. **Send the conversation's own request prefix.** The system prompt, tools, and message prefix match what the conversation sends, so the summarizer reads the conversation's prompt cache and pays cache-read rates for most of its input. Codex forks the same way.

   - **Preserve the prefix.** Editing the history first would miss that cache. On models that bind thinking blocks to the conversation prefix, such as Opus 5.5, it would also invalidate the thinking blocks the request replays, which enforced accounts reject with a 400.
   - **Keep the tools and ask for text only.** The tools array is part of the cached prefix. The rubric asks the model not to call any tool, and a reply that stops on `tool_use` fails the compaction.
   - **Replace history after success.** The summary replaces the whole history, so later requests replay no earlier thinking.
   - **Trim on overflow.** Retries drop the oldest API rounds and strip every thinking block, since a new turn may omit earlier thinking but must not replay it after a changed prefix. A retry whose kept rounds end on a tool result closes that turn with a short assistant message before the rubric.
   - **Reserve summary room.** Claude Code's `truncateHeadForPTLRetry` frees just the reported overflow. oxide-code frees about 20k tokens beyond it so the retry leaves the summary room to finish, and marks no cache breakpoint on a transcript no later request reads.

9. **Synthetic post-compact user message with no parent.** Claude Code, Codex, and opencode all materialize the summary as a `role: user` message, since an assistant message cannot lead a turn and system blocks are special-cased at the prefix. Storing the synthetic head with no parent lets the chain walk stop at the boundary with no special case. The message body is `SUMMARY_PREFIX + "\n\n" + summary`, where `SUMMARY_PREFIX` is a re-entry framing taken from the Codex template that tells the next-turn model to use the summary and continue without re-asking what to do.

10. **The synthetic message as the stored boundary.** The synthetic message is stored as a chain root, with no parent, carrying the `CompactInfo` and the refreshed prompt context, and the session's head moves onto it. The loader's chain walk stops at the root, keeps `CompactInfo` for resume display, and reads only the new tail, while the summarized rows stay for export. Claude Code's boundary entry and Codex's `RolloutItem::Compacted` serve the same purpose, and `pre_message_count` fills the post-compact header without a second field.

11. **Preserve session identity.** Claude Code, Codex, and opencode converge on this. `/clear` rolls because intent resets, while `/compact` preserves because intent is retained and context is compressed. The session ID, its stored rows, project, and title all carry through unchanged.

12. **Reset the file tracker.** Edit and Write require a Read in the visible transcript, and the visible transcript is now the summary, so the previous reads are out of scope. The reset matches the post-`/clear` behavior and forces a fresh Read before any Edit.

13. **Keep queued prompts.** A session switch drops the prompts queued before it because they belong to the conversation that ended. `/compact` preserves the user's intent, so queued prompts still make sense after it.

14. **Stream the summary as it generates.** A long session's summary can take tens of seconds, and streaming it shows progress and lets the user judge it early. The engine streams the text and decides its fate, so every frontend sees one protocol. A discard precedes the event that reports a failure or cancellation, and a retry discards before streaming again. The stdio frontends skip the stream and keep their one-line boundary notice.

15. **A failed attempt fails its prompt.** Sending the prompt against the unchanged transcript would put a request over the threshold on the wire, and the context would keep growing until it overflowed, which is the case auto-compaction exists for. The TUI hands the unsent prompt back and holds the queue, so the prompts behind it do not each start another summary request. A failed mid-turn attempt ends its turn for the same reason.

16. **A continuation only inside a turn.** After a compaction between prompts, queued input drains, and otherwise the assistant waits, since a synthetic continue prompt would make the agent act without fresh user intent. A mid-turn compaction resumes work the user already asked for, so its summary asks the model to resume the task. Claude Code and opencode add a continuation only where a turn is already running.

17. **Circuit-break automatic failures.** A compact request that is too large or malformed can be unrecoverable, so the pause keeps the engine from sending it with every prompt. Claude Code stops after three consecutive failures as well.

18. **Free-text instructions, appended verbatim.** `/compact <instructions>` lets the user steer the summary toward what they care about, using the same typed-argument form as `/rename <title>` and `/model <id>`. Claude Code accepts the same argument and appends it under an additional-instructions heading after its rubric.

19. **Refuse a short transcript and an empty summary.** Below about 1,000 tokens the summary is about as long as the transcript, so the attempt fails and leaves the session untouched. A whitespace-only reply fails the same way, since retrying costs less than committing a useless summary.

20. **`CompactedBlock` is one chat block.** The accent-bar surface gives compaction a visual identity while keeping the header and summary one conceptual unit.

## Out of Scope

- **Anchored re-compaction (`<previous-summary>` block).** opencode updates a previous summary in place, which pays off only when one session is compacted many times. Each compaction summarizes the transcript since the last boundary, which already starts from the previous summary, and repeated lossy rewrites need usage data before they earn their complexity.

- **Microcompact and prune (in-place tool-result deletion).** Claude Code's microcompact and opencode's prune clear old tool-output bodies before a full summary. They are a separate retention policy with their own UI and persistence, and whole-transcript summaries at turn and round boundaries cover the same need with one mechanism.

- **Partial and range compaction.** Claude Code's message-selector rewind adds a selector UI, a dual-direction summarizer, and a transcript splice for a case `/compact` already covers.

- **Provider-specific remote compaction.** Codex's OpenAI and Azure paths rewrite history server-side. oxide-code talks to the Anthropic Messages API, where the normal stream already works.

- **A pre-flight `count_tokens` request.** Every reference implementation reads the last usage plus an estimate and accepts a few percent of drift, which the extra round trip does not repay.

- **Compaction hooks.** Hooks belong to a general surface tracked in [#157](https://github.com/hakula139/oxide-code/issues/157), with command hooks in [#159](https://github.com/hakula139/oxide-code/issues/159) and the `pre_compact` event in [#168](https://github.com/hakula139/oxide-code/issues/168).

## Sources

- `crates/ox-core/src/client/anthropic/transport.rs`: `prompt_too_long_message`, which types an HTTP 400 `prompt is too long` as `PromptTooLong`.
- `crates/ox-core/src/config/compaction.rs`: `CompactionConfig`, which resolves the threshold and its snapping.
- `crates/ox-core/src/engine.rs`: `Engine::submit_prompt`, which compacts between the segments of a turn.
- `crates/ox-core/src/engine/compaction.rs`: `Engine::compact`, `Engine::run_compaction`, `Engine::replace_with_summary`, `request_summary`, the `SummaryStream` sink, the context-window retries, `AutoCompactFailures`, and `request_tokens`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::compact`, which saves the boundary and swaps the transcript to the synthetic continuation.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, `TurnStop::Compact`, `collect_response`, and `TurnReport::context_estimate`.
- `crates/ox-core/src/engine/usage.rs`: `UsageLedger`, which accounts the session cost and context usage, and `Engine::account_usage` and `Engine::reset_context_usage`, which feed it.
- `crates/ox-core/src/protocol.rs`: `Op::Compact`, the `SessionCompacted`, `SummaryToken`, `SummaryDiscarded`, `AutoCompactionStarted`, and `AutoCompactionPaused` events, and `CompactTrigger`.
- `crates/ox-core/src/protocol/notice.rs`: `compacted`, the boundary header every frontend shows.
- `crates/ox-core/src/session/actor.rs`: the `SessionCmd::Compact` handler.
- `crates/ox-core/src/session/handle.rs`: `SessionHandle::compact` and `CompactOutcome`.
- `crates/ox-core/src/session/history.rs`: `TranscriptWalker::push_message`, which skips the generated summary block when the compaction boundary already displays it.
- `crates/ox-core/src/session/post_compact.rs`: `SUMMARY_PREFIX` and `synthesize_post_compact_message`.
- `crates/ox-core/src/session/state.rs`: `SessionState::compact`, which queues the synthetic root.
- `crates/ox-core/src/session/store.rs`: `CompactInfo`, the record a compaction root stores.
- `crates/ox-core/src/session/store/load.rs`: the chain walk that stops at the root.
- `crates/ox-tui/src/app/events.rs`: `apply_session_compacted` and the auto-compaction event handling.
- `crates/ox-tui/src/app/queue.rs`: `PromptQueue::fail_turn`, which hands back an unsent prompt.
- `crates/ox-tui/src/components/chat/blocks/compacted.rs`: `CompactedBlock`.
- `crates/ox-tui/src/slash/compact.rs`: `CompactCmd`.
- `crates/ox/src/driver/stdio.rs`: `StdioSink`, which prints the compaction line on stderr.
