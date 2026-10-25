# Auto-Compaction

Automatic context compression builds on manual `/compact`: when the estimated size of the next request approaches the active model's context window, oxide-code summarizes the current transcript, persists the normal compact boundary, resets the file tracker, and continues from the synthetic summary.

Companion docs: [research/agent/auto-compaction.md](../../research/agent/auto-compaction.md), [slash/compact.md](../slash/compact.md), [session/persistence.md](../session/persistence.md).

## Scope

Auto-compaction is **default on** and can be disabled independently from manual `/compact`. The trigger runs before recording a new user prompt when the previous turn left its estimate over threshold, whether that turn completed, failed, or was cancelled. It also runs inside a turn, at the round boundary where the next request would cross the threshold, and the turn then continues from the summary.

It does not interrupt an in-flight stream or tool call. A prompt that arrives during compaction stays queued and drains afterward, unless the compaction fails, in which case the queue waits for the user.

## Token Signal

The trigger estimates the size of the next request from the latest request of the turn:

```text
input_tokens + cache_creation_input_tokens + cache_read_input_tokens + estimate(messages since that request)
```

`message_start` reports the input and cache counts, and the stream keeps the latest non-zero value for each field. The status-line context segment shows the same input total. The messages appended since that request, its reply, tool results, queued prompts, and any interruption marker, have no reported usage yet, so the trigger estimates them at four bytes of JSON per token, as Claude Code's `tokenCountWithEstimation` does.

A fresh, resumed, or compacted transcript has no request to measure from. The trigger estimates the whole next request at the same four bytes per token: the system prompt, the user context, the tool definitions, and the transcript. It also uses this estimate when a turn reports no usage, including a context-limit rejection, so retained prompts still count toward compaction before the next submission. The status-line context segment keeps the latest reported input total. A resumed session over the threshold therefore compacts before its first prompt.

When a turn reports usage, the checks inside the turn and before the next prompt use the same estimate.

## Threshold

Every supported model in `model.rs` has a `1_000_000`-token context window. Unknown model IDs have no window, so default and percentage-based auto-compaction stay disabled for them. Explicit token thresholds still apply.

The threshold is:

```text
effective_window = context_window - min(max_tokens, 20_000)
threshold = effective_window - 13_000
```

The 20k summary reserve mirrors Claude Code's p99 summary-output headroom and keeps a compact request from firing at the hard limit. The 13k buffer leaves room for the next prompt, dynamic instructions, and small tool-schema drift. With the default `max_tokens`, supported models compact at `967_000` tokens. If the subtraction would underflow, auto-compaction stays disabled.

## Configuration

Config surface:

```toml
[client.compaction]
auto_enabled = true
auto_threshold_tokens = 400000
# or:
auto_threshold_percent = 40
```

Environment:

| Variable                               | Effect                                     |
| -------------------------------------- | ------------------------------------------ |
| `OX_COMPACTION_AUTO_ENABLED`           | Overrides `client.compaction.auto_enabled` |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`  | Absolute automatic trigger threshold       |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT` | Percent of context, capped by safe trigger |

The config controls only whether and where automatic compaction triggers, and manual `/compact` ignores it. Token and percent thresholds are mutually exclusive, so the resolved trigger stays obvious.

Explicit thresholds snap into a usable range. Token values below `50_000` rise to that floor, and for models with known context windows, values above the model-derived safe trigger drop to it. Percent thresholds must be 1-100 and snap the same way once resolved to tokens. The floor exists because a lower threshold would summarize again and again long before context pressure exists.

## Trigger Flow

`Engine` owns the automatic trigger because it can compact before a new prompt is recorded or between the segments of a turn. Each run of `agent_turn` reports its usage and the estimate above.

### Before a Prompt

1. `agent_turn` streams a turn, persists the transcript tail, and returns the latest token usage in its `TurnReport`, including when the turn fails or is cancelled.
2. `Engine::account_usage` stores the report's estimate of the next request as the pending automatic trigger signal.
3. When the next prompt arrives, `Engine::auto_compact` checks the stored estimate before `submit_prompt` records the prompt.
4. If the estimate crosses the threshold, it runs `Engine::run_compaction`, the same pipeline `/compact` uses. A transcript below the `/compact` size floor is skipped without counting a failure, since the system prompt, instructions, and tools then fill most of the context and a summary would not shrink the request.
5. `Engine::run_auto_compaction` emits `AutoCompactionStarted` so the TUI can show compaction status while the summarizer runs, and the summary streams into the chat as it does for `/compact`.
6. On success, `Engine::replace_with_summary` persists the compact boundary, clears the file tracker, replaces `messages` with the synthetic post-compact message, and emits `SessionCompacted`.
7. On failure, the streamed summary is discarded, `AutoCompactFailures::fail` increments the failure counter, and `submit_prompt` emits `PromptUnsent`, then ends with `TurnFailed` without recording or sending the new prompt.

### Inside a Turn

1. After a tool round records its results and any queued prompts, `agent_turn` computes the estimate above. When it crosses the threshold and `max_tool_rounds` leaves another round, the turn stops with `TurnStop::Compact` before its next request, so the transcript holds no unresolved tool call.
2. `Engine::submit_prompt` bills the rounds so far, adopts their task list, and calls `Engine::compact_mid_turn`, which runs the same pipeline with the `mid_turn` trigger.
3. On success, the summary message ends with a reminder that tells the model to resume its task, and the prompts queued during the summary are recorded after it. `agent_turn` then runs again from the next round, and the rounds before the compaction still count toward `max_tool_rounds`.
4. On failure, the failure counts toward the pause, and the turn ends with `TurnFailed` and a hint to run `/compact`, then send a message to continue. A cancel ends the turn on the interruption marker, as at any other round boundary.

A refusal later in the turn rolls back only to the summary, which still holds the prompt, so its notice points to `/clear` in case the model keeps declining.

The failure counter lives on `Engine`. Three consecutive automatic failures pause further automatic attempts until a successful compaction, a model or effort change, `/clear`, `/archive`, or `/resume` resets the counter. Manual `/compact` does not consult it. The failure that pauses it emits `AutoCompactionPaused` with the failure count, and a reset of the paused counter emits `AutoCompactionResumed`, which the TUI follows to show the pause in `/status`.

## User Experience

Manual and automatic compaction use the same visible `CompactedBlock`. When the attempt ahead of a prompt fails, the prompt is not sent, and chat shows why. A summarizer failure reads `Auto-compaction failed: ...`, and a failed boundary write shows a `Session write failed: ...` notice followed by `Auto-compaction was not saved`. Once session persistence has stopped, the boundary commits in memory alone and the compaction succeeds, as [Write-Error Surfacing](../session/persistence.md#write-error-surfacing) describes. On the `PromptUnsent` event that precedes the failure, the TUI puts the prompt back in the input, or at the head of the queue when other prompts are waiting or the input holds a draft, and leaves the queue for the user to start. Sending the prompt again retries compaction. During the pause, the next send goes out without compaction, and `/status` shows that auto-compaction is paused and points to `/compact`.

During TUI auto-compaction, the status bar shows the `Compacting` state, and the TUI holds prompts submitted meanwhile, so a failed mid-turn attempt leaves them waiting for the user. Automatic `SessionCompacted` events keep the TUI busy until the queued prompt drains or the prompt submission finishes. After a mid-turn compaction, the reply continues below the boundary block, and the summary stands in for the prompt and the prompts the turn drained before it, so a later rollback hands back only the prompts drained after it. In the bare REPL and headless runs, `driver::stdio::StdioSink` writes `Compacting the conversation to free up context` to stderr on `AutoCompactionStarted`, then renders `SessionCompacted` as a stderr line such as `Compacted 12 messages into a summary`, the header the TUI block shows.

## Design Decisions

1. **Default-on.** Running out of context is worse than a well-marked summary boundary. A separate opt-out preserves user control.

2. **Response usage as the token signal.** The stream already carries usage, and only the messages appended since the latest request need an estimate. A count-tokens request would add latency and still be approximate once dynamic system context and tool definitions are included. Claude Code also reads the last usage stored in a resumed transcript, but a session file records usage apart from the messages, so a fresh transcript estimates the whole request instead.

3. **Boundary-only compaction.** Compaction runs between persisting a coherent transcript unit and starting the next prompt or the next round of a turn, once every call of the round has its result recorded. It never interrupts a stream or a running tool, so the summarized transcript holds no unresolved call, and session replay matches manual `/compact`. The round boundary is a compaction site because a long tool loop would otherwise fill the context window before its turn ends.

4. **Same summarizer as `/compact`.** Auto-compaction has no model setting of its own. The summarizer forks the conversation's request through the agent loop's stream path, which already handles auth, model, effort, betas, prompt caching, and first-party gateway constraints.

5. **Same persistence boundary as `/compact`.** Automatic and manual compaction write the same boundary, so sessions keep one format.

6. **Failure circuit breaker.** A compact request that is too large or malformed can be unrecoverable, so the pause keeps the engine from sending it with every prompt.

7. **A continuation only inside a turn.** After a compaction between prompts, queued input drains, and otherwise the assistant waits, since a synthetic "continue" prompt would make the agent act without fresh user intent. A mid-turn compaction resumes work the user already asked for, so its summary message tells the model to resume the task directly, adapting Claude Code's continuation after an automatic compaction.

8. **A failed attempt fails its prompt.** Sending the prompt against the unchanged transcript would put a request over the threshold on the wire, and the context would keep growing until it overflowed the window, which is the case auto-compaction exists for. Failing the prompt reports the failure before anything is sent, and the user can still continue during the pause without compaction. The TUI hands the unsent prompt back and holds the queue, so the prompts queued behind it do not each start another summary request on their own. A failed mid-turn attempt ends its turn for the same reason, since the turn's next request would cross the threshold, and the TUI holds the queue there too.

## Related Issues

A `pre_compact` hook is tracked in [#168](https://github.com/hakula139/oxide-code/issues/168).

## Sources

- `crates/ox-core/src/config/compaction.rs`: `CompactionConfig`, which resolves the threshold and its snapping.
- `crates/ox-core/src/engine.rs`: `Engine::submit_prompt`, which compacts between the segments of a turn.
- `crates/ox-core/src/engine/compaction.rs`: `Engine::auto_compact`, `Engine::compact_mid_turn`, `Engine::run_auto_compaction`, `Engine::run_compaction`, `Engine::replace_with_summary`, `AutoCompactFailures`, the failure counter and its pause, and `request_tokens`, the estimate for a fresh transcript.
- `crates/ox-core/src/engine/turn.rs`: `agent_turn`, which stops with `TurnStop::Compact` at a round boundary that reaches the threshold, and `TurnReport::context_estimate`, the trigger signal.
- `crates/ox-core/src/engine/usage.rs`: `Engine::account_usage`, which stores the trigger signal, and `Engine::reset_context_usage`, which estimates a fresh transcript.
- `crates/ox-core/src/protocol.rs`: `Event::AutoCompactionStarted`, `Event::AutoCompactionPaused`, `Event::AutoCompactionResumed`, `Event::PromptUnsent`, and `CompactTrigger`.
- `crates/ox-core/src/session/post_compact.rs`: the summary message and its mid-turn continuation.
- `crates/ox-tui/src/app/events.rs`: the TUI's handling of the auto-compaction events.
- `crates/ox-tui/src/app/queue.rs`: `PromptQueue::fail_turn`, which hands back an unsent prompt and holds the queue after a failed automatic compaction.
