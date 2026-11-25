# Compaction

Context compression replaces the visible transcript with a summary so long sessions keep working. [Compaction](../guide/slash-commands.md#compaction) covers user-facing behavior and `/compact`, and [`[client.compaction]`](../guide/configuration.md#clientcompaction-context-compression) lists configuration keys. Manual and automatic compaction share one summarizer, one persisted boundary, and one failure pause.

## Triggers

`/compact` is idle-only, so in-flight turns finish first. Automatic compaction runs without interrupting an in-flight stream or tool call at two boundaries:

- **Before a prompt.** Before recording a new prompt when the previous turn left its estimate over threshold (whether it completed, failed, or was cancelled).
- **Inside a turn.** At a round boundary when the next request would cross the threshold, after which the turn continues from the summary.

Prompts arriving during compaction queue and drain afterward, unless compaction fails, in which case the queue waits for the user.

## Token Signal

The trigger estimates the next request from the turn's latest request:

```text
input_tokens + cache_creation_input_tokens + cache_read_input_tokens + estimate(messages since that request)
```

The stream retains the latest non-zero input and cache counts from `message_start` (also shown in the status line), and estimates messages appended since at four bytes of JSON per token, as Claude Code does.

Fresh, resumed, or compacted transcripts, as well as turns that reported no usage (such as after a context-limit rejection), estimate the whole next request (system prompt, user context, tool definitions, transcript) at the same rate, so retained prompts still trigger compaction and resumed sessions over threshold compact before their first prompt. The status line keeps only reported totals.

## Threshold

Every catalog model in `model.rs` has a `1_000_000`-token window. Unknown model IDs disable default and percentage auto-compaction while still honoring explicit token thresholds.

```text
effective_window = context_window - min(max_tokens, 20_000)
threshold = effective_window - 13_000
```

The 20k summary reserve matches Claude Code's p99 summary-output headroom, and the 13k buffer leaves room for the next prompt, dynamic instructions, and minor tool-schema drift. With default `max_tokens`, catalog models compact at `967_000` tokens. Underflow disables auto-compaction. [`[client.compaction]`](../guide/configuration.md#clientcompaction-context-compression) applies only to automatic compaction, token and percent thresholds are mutually exclusive, and the `50_000` floor avoids premature summaries.

## Summarizer

`request_summary` forks the live conversation (same system prompt, user context, tools, and transcript), appends `SUMMARIZATION_USER_RUBRIC` (asking for a terse list of intent, decisions, code paths touched, current state, next step, and text-only output, followed by any custom instructions under an additional-instructions heading), and reads the reply through `collect_response`. Every request records usage even on failure or cancellation.

`SummaryStream` emits `Event::SummaryToken`, forwards retry notices and usage limits, and drops thinking. Any retry, failure, or dropped future emits `Event::SummaryDiscarded` if text already streamed.

HTTP 400 `prompt is too long` and `model_context_window_exceeded` stops get up to three retries. Each retry strips thinking blocks and drops the oldest API rounds to free the reported overflow plus ~20k tokens of summary headroom.

Summarization fails if the reply ends on `max_tokens`, refusal, `tool_use`, pause, unknown stop reason, truncated stream, whitespace-only text, or overflow exhausting retries or leaving only the newest round. It also refuses transcripts under ~1,000 tokens (counting text, tool calls, and tool results, excluding system prompt, instructions, and tool definitions since summarization cannot shrink them).

## Boundary

`SessionHandle::compact(boundary, synthetic_message) -> CompactOutcome` (in `session/handle.rs`) stores one parentless message row as the new `head_id`:

- **Message.** A `role: user` row holding `SUMMARY_PREFIX`, a blank line, the summary, optional `<system-reminder>` blocks for tasks and open background shells, and after mid-turn compaction a resume reminder.
- **`compact` column.** `summary`, `pre_message_count`, optional `instructions`, and `tasks` (omitted when empty).
- **`context` column.** Refreshed session context.

Pre-compact rows remain in the store off the active chain. Resume's chain walk stops at the parentless root, `CompactInfo` drives the TUI boundary block, and `session::sanitize` treats the root as ordinary user text.

`Engine::run_compaction` drives both paths: on success, it reloads instruction files, builds the synthetic continuation, calls `LiveTranscript::compact` (saving the boundary, clearing the file tracker, replacing in-memory messages, adopting the refreshed prompt), emits `Event::SessionCompacted` plus any new instruction warnings, and ends manual compaction with `OpEnded`.

- **Summarizer failure.** Emits `Event::TurnFailed` and leaves the transcript unchanged.
- **Boundary write failure.** Stops session persistence like any write error, applies the summary in memory, and reports the error after `SessionCompacted` so the chat clear does not hide it.
- **Cancel or quit.** Cancel ends with `OpEnd::Cancelled`, and quit ends without `OpEnded`.

Every summary request adds a `usage` row to the session total regardless of outcome, emitting `Event::UsageUpdated` after `SessionCompacted` on success or before op end when a failed or cancelled request reported usage.

## Trigger Flow

`Engine` owns the automatic trigger. `agent_turn` returns latest usage and the next-request estimate in `TurnReport` (even on failure or cancel), which `Engine::account_usage` stores as the pending signal.

### Before a Prompt

1. `Engine::auto_compact` checks the stored estimate before `submit_prompt` records the prompt.
2. Over threshold, `Engine::run_auto_compaction` emits `AutoCompactionStarted` and runs the pipeline, streaming the summary into chat. Transcripts below the ~1,000-token floor skip silently without counting a failure.
3. On failure, streamed summary text is discarded, `AutoCompactFailures::fail` increments, and `submit_prompt` emits `PromptUnsent` then `TurnFailed` without recording or sending the prompt.

### Inside a Turn

1. After recording a tool round and any queued prompts, `agent_turn` stops with `TurnStop::Compact` if the estimate crosses threshold and `max_tool_rounds` allows another round, leaving no unresolved tool call.
2. `Engine::submit_prompt` bills completed rounds, adopts their task list, and calls `Engine::compact_mid_turn` (`mid_turn` trigger).
3. On success, prompts queued during summarization record after the summary and `agent_turn` continues from the next round (prior rounds still count toward `max_tool_rounds`).
4. On failure, the failure counter increments and the turn ends with `TurnFailed` pointing to `/compact`. Cancellation ends on the interruption marker like any round boundary.

A later refusal rolls back only to the summary (which retains the prompt), pointing to `/clear` if the model keeps refusing.

## Failure Reset

`UsageLedger` starts each session's failure counter at zero. Three consecutive automatic failures pause auto-compaction (emitting `AutoCompactionPaused`) until a successful compaction, model or effort change, `/clear`, `/archive`, or `/resume` resets it (emitting `AutoCompactionResumed` when paused, reflected in `/status`). Manual `/compact` ignores the counter.

## Frontend Behavior

Both paths render `CompactedBlock` (an accent-bar surface with header `Compacted N messages into a summary`, optional `(focus: ...)` suffix, Markdown summary, and no footer). Streaming renders via `CompactedBlock::render_streaming` under `Summarizing the conversation...` and clears on `SummaryDiscarded`. `App::apply_session_compacted` clears chat and the modal stack (keeping modals open on mid-turn compaction), pushes the block, and keeps queued prompts, and resume renders the same block while hiding the synthetic root.

On pre-prompt failure, chat shows `Auto-compaction failed: ...`, `PromptUnsent` hands the prompt back ([**The TUI owns the queue**](cancellation.md#design-decisions)), and resending retries compaction. While paused, prompts send normally and `/status` notes the pause. A boundary write failure still sends the prompt after the boundary block and `Session write failed: ...`.

During compaction, the status bar shows `Compacting` and holds new prompts. Automatic compaction keeps the TUI busy until the prompt finishes, mid-turn replies continue below the boundary block, and rollback returns only prompts drained after it. Manual `/compact` submits the queue head on `OpEnded` (after any prompt warnings), except after a late cancel that lost the race to the swap. Headless `driver::stdio::TextState` logs `Compacting the conversation to free up context` to stderr on `AutoCompactionStarted` and prints the header line on `SessionCompacted`.

## Design Decisions

1. **One path for manual and automatic compaction, default on with an opt-out.** Sharing `Engine::run_compaction` keeps summarization, persistence, and error handling identical. Auto-compaction defaults on to avoid losing late-session work, while `client.compaction.auto_enabled = false` disables only automatic triggers.

2. **Response usage as the token signal, with reserved output headroom.** Streams already report usage, leaving only appended messages to estimate, whereas a count-tokens call adds latency and still approximates dynamic context. Claude Code and opencode likewise combine last usage, tail estimation, and reserved headroom (20k reserve, 13k buffer).

3. **Boundary-only compaction.** Compacting only at coherent persisted boundaries (pre-prompt, round boundary, or idle manual `/compact`, as in Claude Code, Codex, and opencode) avoids interrupting streams or tools and keeps replay identical to manual `/compact`. Claude Code's microcompact and opencode's prune would add a second retention policy, UI, and persistence layer.

4. **Same request path and cached prefix as a turn.** Reusing `Wire` and `collect_response` preserves model, effort, thinking, credentials, betas, and prompt-cache reads (as Codex does) over the standard Messages stream:

   - **Preserve the prefix.** Mutating history first misses cache and breaks prefix-bound thinking blocks (such as Opus 5.5), which enforced accounts reject with 400.
   - **Keep the tools and ask for text only.** Tools belong to the cached prefix, the rubric forbids tool calls, and `tool_use` stops fail compaction.
   - **Replace history after success.** Replacing history avoids replaying pre-compact thinking, and later compactions summarize from this root, avoiding opencode's second anchored re-compaction path.
   - **Trim on overflow.** Retries drop oldest API rounds and strip all thinking blocks (since replaying thinking after a changed prefix is invalid), closing a trailing tool-result turn with a short assistant message before the rubric.
   - **Reserve summary room.** Whereas Claude Code's overflow retry frees only the reported overflow, `ox` frees about 20k extra tokens for the summary and sets no cache breakpoint on the unextended trimmed transcript.

5. **Synthetic parentless user message as the stored boundary.** Claude Code, Codex, and opencode all store summaries as `role: user` (assistant cannot open a turn, and system blocks are prefix-special). Dropping `parent_id` stops the resume walk naturally while retaining prior rows for export, and `SUMMARY_PREFIX` (adapted from Codex) prompts continuation.

6. **Preserve session identity and queued prompts, reset the file tracker.** Matching Claude Code, Codex, and opencode, compaction keeps session ID, rows, project, title, and queued prompts because user intent continues (unlike `/clear`), while resetting the file tracker to require fresh Reads before Edits.

7. **Stream the summary as it generates.** Long summaries take tens of seconds, so streaming gives early feedback across frontends while discarding cleanly on retries or errors and keeping headless text to one line.

8. **A failed attempt fails its prompt, and three consecutive failures pause auto-compaction.** Sending against an over-threshold transcript risks context overflow, so the TUI returns the unsent prompt and holds the queue (and mid-turn failures end the turn). Pausing after three consecutive failures, as Claude Code does, prevents retrying a doomed summary on every prompt.

9. **A continuation only inside a turn.** Between prompts, queued input drains or the assistant waits so it never acts without user intent, while mid-turn compaction resumes active work, matching Claude Code and opencode.

10. **Free-text instructions, and refuse short transcripts or empty summaries.** `/compact <instructions>` appends steering verbatim under an additional-instructions heading, matching Claude Code and `/rename` / `/model`. Refusing transcripts under ~1,000 tokens and whitespace-only replies prevents pointless replacements.

## Out of Scope

- **Partial and range compaction.** Claude Code's rewind-based range compaction adds a selector UI, dual-direction summarizer, and transcript splicing for a case `/compact` covers.
- **Compaction hooks.** Tracked under general hooks ([#157](https://github.com/hakulab/oxide-code/issues/157)), command hooks ([#159](https://github.com/hakulab/oxide-code/issues/159)), and `pre_compact` ([#168](https://github.com/hakulab/oxide-code/issues/168)).

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
