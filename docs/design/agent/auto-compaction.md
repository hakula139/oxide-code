# Auto-Compaction

Automatic context compression builds on manual `/compact`: when the latest observed token usage approaches the active model's context window, oxide-code summarizes the current transcript, persists the normal compact boundary, resets the file tracker, and continues from the synthetic summary.

Companion docs: [research/agent/auto-compaction.md](../../research/agent/auto-compaction.md), [slash/compact.md](../slash/compact.md), [session/persistence.md](../session/persistence.md).

## Scope

Auto-compaction is **default on** and can be disabled independently from manual `/compact`. The trigger runs before recording a new user prompt when the previous completed turn left usage over threshold. Tool results are compacted only after the assistant has consumed them and returned a final response.

It does not interrupt an in-flight stream or tool call. A prompt that arrives during compaction stays queued and drains afterward, unless the compaction fails, in which case the queue waits for the user.

## Token Signal

The trigger reads the usage of the previous turn's last round as one total:

```text
input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens
```

`message_start` reports the input and cache counts, and `message_delta` usually reports only `output_tokens`, so the stream keeps the latest non-zero value for each field. The status-line context segment shows the same total without `output_tokens`. Missing usage means "do not auto-compact".

## Threshold

Every supported model in `model.rs` has a `1_000_000`-token context window. Unknown model IDs have no window, so auto-compaction stays disabled for them.

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

`Engine` owns the automatic trigger because it can compact before a new prompt is recorded. The agent turn reports the latest usage signal after each completed turn.

1. `agent_turn` streams a complete turn, persists the transcript tail, and returns the latest token usage in its `TurnReport`.
2. `Engine` stores that usage as the pending automatic trigger signal.
3. When the next prompt arrives, `Engine::auto_compact` checks the stored usage before `submit_prompt` records the prompt.
4. If the total crosses the threshold, it calls the same compact driver used by `/compact`. A transcript below the `/compact` size floor is skipped without counting a failure, since the system prompt, instructions, and tools then fill most of the context and a summary would not shrink the request.
5. `auto_compact_if_needed` emits `AutoCompactionStarted` so the TUI can show compaction status while the summarizer runs, and the summary streams into the chat as it does for `/compact`.
6. On success, `compact_boundary` persists the compact boundary, clears the file tracker, replaces `messages` with the synthetic post-compact message, and emits `SessionCompacted`.
7. On failure, the streamed summary is discarded, `auto_compact_if_needed` increments the failure counter, and `submit_prompt` ends with `TurnFailed` without recording or sending the new prompt.

The failure counter lives on `Engine`. Three consecutive automatic failures pause further automatic attempts until a successful compaction, a model or effort change, `/clear`, `/archive`, or `/resume` resets the counter. Manual `/compact` does not consult it. The failure that pauses it emits `AutoCompactionPaused` with the failure count, and a reset of the paused counter emits `AutoCompactionResumed`, which the TUI follows to show the pause in `/status`.

## User Experience

Manual and automatic compaction use the same visible `CompactedBlock`. When an automatic attempt fails, the prompt is not sent, and chat shows why. A summarizer failure reads `Auto-compaction failed: ...`, and a failed boundary write shows a `Session write failed: ...` notice followed by `Auto-compaction was not saved`. Once session persistence has stopped, the boundary commits in memory alone and the compaction succeeds, as [Write-Error Surfacing](../session/persistence.md#write-error-surfacing) describes. The TUI puts the prompt back in the input, or at the head of the queue when other prompts are waiting or the input holds a draft, and leaves the queue for the user to start. Sending the prompt again retries compaction. During the pause, the next send goes out without compaction, and `/status` shows that auto-compaction is paused and points to `/compact`.

During TUI auto-compaction, the status bar shows the `Compacting` state. Automatic `SessionCompacted` events keep the TUI busy until the queued prompt drains or the prompt submission finishes. In the bare REPL, `driver::stdio::StdioSink` renders `SessionCompacted` as a stderr line such as `Auto-compacted 12 messages into summary`.

## Design Decisions

1. **Default-on.** Running out of context is worse than a well-marked summary boundary. A separate opt-out preserves user control.

2. **Response usage as the token signal.** The stream already carries usage. A count-tokens request would add latency and still be approximate once dynamic system context and tool definitions are included.

3. **Boundary-only compaction.** Compaction runs between persisting a coherent transcript unit and starting the next prompt. This avoids partial tool loops and makes session replay identical to manual `/compact`.

4. **Same summarizer as `/compact`.** Auto-compaction has no model setting of its own. The summarizer forks the conversation's request through the agent loop's stream path, which already handles auth, model, effort, betas, prompt caching, and first-party gateway constraints.

5. **Same persistence boundary as `/compact`.** Automatic and manual compaction write the same boundary, so sessions keep one format.

6. **Failure circuit breaker.** A compact request that is too large or malformed can be unrecoverable, so the pause keeps the engine from sending it with every prompt.

7. **No automatic continue prompt.** If the user queued input, it drains after compaction. Otherwise the assistant waits. Synthetic "continue" prompts make the agent act without fresh user intent.

8. **A failed attempt fails its prompt.** Sending the prompt against the unchanged transcript would put a request over the threshold on the wire, and the context would keep growing until it overflowed the window, which is the case auto-compaction exists for. Failing the prompt reports the failure before anything is sent, and the user can still continue during the pause without compaction. The TUI hands the unsent prompt back and holds the queue, so the prompts queued behind it do not each start another summary request on their own.

## Related Issues

Mid-turn compaction is tracked in [#184](https://github.com/hakula139/oxide-code/issues/184), and a `pre_compact` hook in [#168](https://github.com/hakula139/oxide-code/issues/168).
