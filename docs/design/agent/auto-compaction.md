# Auto-Compaction

Automatic context compression builds on manual `/compact`: when the latest observed token usage approaches the active model's context window, oxide-code summarizes the current transcript, persists the normal compact boundary, resets the file tracker, and continues from the synthetic summary.

Companion docs: [research/agent/auto-compaction.md](../../research/agent/auto-compaction.md), [slash/compact.md](../slash/compact.md), [session/persistence.md](../session/persistence.md).

## Scope

Auto-compaction is **default on** and can be disabled independently from manual `/compact`. The first implementation runs at safe boundaries:

- after a complete text-only assistant turn;
- after a tool round is persisted, before the next sampling request;
- before starting a new user prompt if the previous turn left usage over threshold.

It does not interrupt an in-flight stream or tool call. If a queued prompt exists, the prompt remains queued during compaction and drains afterward through the existing prompt-queue path.

## Token Signal

The agent loop records the maximum observed token usage from each stream:

- `message_start.message.usage.input_tokens + output_tokens`;
- `message_delta.usage.input_tokens + output_tokens`.

Anthropic's delta usage often carries only output tokens, so stream processing keeps the latest non-zero input and output values separately and computes `total = input + output`. Treat this value only as the auto-compaction trigger signal; it is unsuitable for billing telemetry. Missing usage means "do not auto-compact".

## Threshold

Each model has a known context window in `model.rs`:

- normal Claude context: `200_000`;
- `[1m]` models with the 1M beta: `1_000_000`;
- unknown models: no window, so auto-compaction is disabled.

The threshold is:

```text
effective_window = context_window - min(max_tokens, 20_000)
threshold = effective_window - 13_000
```

The 20k summary reserve mirrors Claude Code's p99 summary-output headroom and keeps a compact request from firing at the hard limit. The 13k buffer leaves room for the next prompt, dynamic instructions, and small tool-schema drift. If the subtraction would underflow, auto-compaction stays disabled.

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

| Variable                                 | Effect                                      |
| ---------------------------------------- | ------------------------------------------- |
| `OX_COMPACTION_AUTO_ENABLED`             | Overrides `client.compaction.auto_enabled`  |
| `OX_COMPACTION_AUTO_THRESHOLD_TOKENS`    | Absolute automatic trigger threshold        |
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`   | Percent of the model context window         |

Manual `/compact` remains available. The config controls only whether automatic compaction triggers and where that trigger fires. Token and percent thresholds are mutually exclusive so the resolved trigger stays obvious.

## Trigger Flow

`agent_turn` owns the automatic trigger because it has the live transcript, token usage, session handle, file tracker, sink, and user-action receiver.

1. Stream a model response and update the latest token usage in `StreamOutcome`.
2. Persist the assistant message and any tool-result message for the round.
3. If auto-compaction is enabled and the latest total crosses the threshold, call the same compact driver used by `/compact`.
4. On success, replace `messages` with the synthetic post-compact message and emit `SessionCompacted`.
5. On failure, increment the auto-compaction failure counter and continue without changing the transcript.

The failure counter is per agent-loop task. Three consecutive automatic failures disable further automatic attempts for the current session. Manual `/compact` does not consult this counter and resets it on success.

## User Experience

Manual and automatic compaction use the same visible `CompactedBlock`. Automatic compaction does not need a separate chat error on failure; repeated automatic failure is a background recovery problem, and the user's next regular request should proceed. The error still lands in logs.

During TUI auto-compaction, the status bar uses the existing `Compacting` state. In bare REPL / headless mode, `StdioSink` already renders `SessionCompacted` as a stderr boundary line.

## Design Decisions

1. **Default-on.** Running out of context is worse than a well-marked summary boundary. A separate opt-out preserves user control.

2. **Response usage over preflight counting.** The stream already carries usage. A count-tokens request would add latency and still be approximate once dynamic system context and tool definitions are included.

3. **Boundary-only compaction.** The first version compacts only after a coherent transcript unit is persisted. This avoids partial tool loops and makes session replay identical to manual `/compact`.

4. **Same summarizer as `/compact`.** No separate compaction model knob yet. The current `Client::stream_message` path already handles auth, model, effort, betas, prompt caching, and first-party gateway constraints.

5. **Same persistence boundary as `/compact`.** Auto-compaction should not create a second session format. `Entry::Compact` can later gain a trigger field if the UI needs to distinguish manual from automatic in history.

6. **Failure circuit breaker.** A too-large or malformed compact request can be unrecoverable. After 3 consecutive automatic failures, the loop stops trying until the session changes through manual compaction, `/clear`, or `/resume`.

7. **No automatic continue prompt.** If the user queued input, it drains after compaction. Otherwise the assistant waits. Synthetic "continue" prompts make the agent act without fresh user intent.

## Deferred

- Mid-turn compaction while a model response still needs tool follow-up.
- Microcompact / prune for old tool-result bodies.
- Anchored re-compaction that updates a previous summary in place.
- Separate compaction model.
- Token / cost status-bar redesign.
- Hook integration.
