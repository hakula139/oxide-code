# Auto-Compaction

Automatic context compression builds on manual `/compact`: when the latest observed token usage approaches the active model's context window, oxide-code summarizes the current transcript, persists the normal compact boundary, resets the file tracker, and continues from the synthetic summary.

Companion docs: [research/agent/auto-compaction.md](../../research/agent/auto-compaction.md), [slash/compact.md](../slash/compact.md), [session/persistence.md](../session/persistence.md).

## Scope

Auto-compaction is **default on** and can be disabled independently from manual `/compact`. The trigger runs before recording a new user prompt when the previous completed turn left usage over threshold. Tool results are compacted only after the assistant has consumed them and returned a final response.

It does not interrupt an in-flight stream or tool call. If another prompt arrives while summarization is running, the prompt remains queued during compaction and drains afterward through the existing prompt-queue path.

## Token Signal

The agent loop records the maximum observed token usage from each stream:

- `message_start.message.usage.input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`;
- `message_delta.usage.input_tokens + cache_creation_input_tokens + cache_read_input_tokens + output_tokens`.

Anthropic's delta usage often carries only output tokens, so stream processing keeps the latest non-zero input, cache-creation input, cache-read input, and output values separately. The status-line context segment reuses the same cache-aware signal. Missing usage means "do not auto-compact".

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
| `OX_COMPACTION_AUTO_THRESHOLD_PERCENT`   | Percent of context, capped by safe trigger  |

Manual `/compact` remains available. The config controls only whether automatic compaction triggers and where that trigger fires. Token and percent thresholds are mutually exclusive so the resolved trigger stays obvious.

Explicit token thresholds must be at least `50_000` tokens and, for models with known context windows, no higher than the model-derived safe trigger. Percent thresholds must be 1-100, are capped by the same safe trigger after they resolve to tokens, and must still resolve to at least `50_000` tokens. Lower values create frequent summarization loops, extra latency, and avoidable summary loss long before context pressure exists.

## Trigger Flow

The main loop owns the automatic trigger because it can compact before a new prompt is recorded. The agent turn reports the latest usage signal after each completed turn.

1. `agent_turn` streams a complete turn, persists the transcript tail, and returns the latest token usage from `StreamOutcome`.
2. The main loop stores that usage as the pending automatic trigger signal.
3. When the next `SubmitPrompt` arrives, `auto_compact_before_prompt` checks the stored usage before recording the prompt.
4. If the total crosses the threshold, it calls the same compact driver used by `/compact`.
5. The agent loop emits `AutoCompactionStarted` so the TUI can show compaction status while the summarizer runs.
6. On success, `compact_boundary` persists the compact boundary, clears the file tracker, replaces `messages` with the synthetic post-compact message, and emits `SessionCompacted`.
7. On failure, the loop increments the auto-compaction failure counter and records the new prompt against the unchanged transcript.

The failure counter is per agent-loop task. Three consecutive automatic failures disable further automatic attempts for the current session. Manual `/compact` does not consult this counter and resets it on success.

## User Experience

Manual and automatic compaction use the same visible `CompactedBlock`. Automatic compaction does not need a separate chat error on failure; repeated automatic failure is a background recovery problem, and the user's next regular request should proceed. The error still lands in logs.

During TUI auto-compaction, the status bar uses the existing `Compacting` state. Automatic `SessionCompacted` events keep the TUI busy until the queued prompt drains or the prompt submission finishes. In bare REPL / headless mode, `StdioSink` already renders `SessionCompacted` as a stderr boundary line.

## Design Decisions

1. **Default-on.** Running out of context is worse than a well-marked summary boundary. A separate opt-out preserves user control.

2. **Response usage over preflight counting.** The stream already carries usage. A count-tokens request would add latency and still be approximate once dynamic system context and tool definitions are included.

3. **Boundary-only compaction.** The first version compacts after a coherent transcript unit is persisted and before the next prompt starts. This avoids partial tool loops and makes session replay identical to manual `/compact`.

4. **Same summarizer as `/compact`.** No separate compaction model knob yet. The current `Client::stream_message` path already handles auth, model, effort, betas, prompt caching, and first-party gateway constraints.

5. **Same persistence boundary as `/compact`.** Auto-compaction should not create a second session format. `Entry::Compact` can later gain a trigger field if the UI needs to distinguish manual from automatic in history.

6. **Failure circuit breaker.** A too-large or malformed compact request can be unrecoverable. After 3 consecutive automatic failures, the loop stops trying until the session changes through manual compaction, `/clear`, or `/resume`.

7. **No automatic continue prompt.** If the user queued input, it drains after compaction. Otherwise the assistant waits. Synthetic "continue" prompts make the agent act without fresh user intent.

## Deferred

- Mid-turn compaction while a model response still needs tool follow-up.
- Microcompact / prune for old tool-result bodies.
- Anchored re-compaction that updates a previous summary in place.
- Separate compaction model.
- Persisted cost restore after resume.
- Hook integration.
