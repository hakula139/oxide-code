# Auto-Compaction (Reference)

Research on automatic context compaction across Claude Code, OpenAI Codex, and opencode. Companion to [slash/compact.md](../slash/compact.md), which covers manual compaction and replacement strategy.

## Claude Code

Claude Code runs automatic compaction proactively before the model call. The query loop applies snip / microcompact / context-collapse transforms first, then calls `autoCompactIfNeeded` with the transformed messages. A successful compact replaces the message set for the rest of the same turn.

Threshold math is token-buffer based:

- `effectiveWindow = contextWindow - min(modelMaxOutputTokens, 20_000)`.
- `autoCompactThreshold = effectiveWindow - 13_000`.
- Warning and error indicators use `threshold - 20_000`.
- Manual blocking limit uses `effectiveWindow - 3_000`.

The token signal is `tokenCountWithEstimation(messages)`, which uses the last API usage plus estimates for unsampled tail content. Auto-compaction defaults on, can be disabled by global `autoCompactEnabled`, and is also gated by `DISABLE_COMPACT` / `DISABLE_AUTO_COMPACT`. `DISABLE_COMPACT` disables manual and automatic compaction; `DISABLE_AUTO_COMPACT` leaves `/compact` available.

Failures are deliberately quiet. Auto-compaction first tries session-memory compaction, falls back to the full summarizer, and stops retrying after 3 consecutive failures. The circuit breaker is important because an over-limit session can otherwise retry a doomed compact request every turn.

Claude Code also has pre-stages that oxide-code should not copy yet:

- **Microcompact** clears old tool-result bodies before a full summary pass.
- **Session-memory compaction** prunes memory-specific slices.
- **Context-collapse** can own the headroom problem in feature-gated builds, so proactive auto-compact is suppressed when it is active.

User-facing behavior is minimal: token warnings mention "until auto-compact" when enabled, and a compact boundary renders after success. Automatic failures are logged rather than surfaced in chat.

Key files:

- `claude-code/src/services/compact/autoCompact.ts`: threshold math, opt-out flags, circuit breaker.
- `claude-code/src/query.ts`: pre-query placement.
- `claude-code/src/components/Settings/Config.tsx`: `autoCompactEnabled` setting.
- `claude-code/src/utils/context.ts`: context-window detection.

## OpenAI Codex

Codex drives auto-compaction from model metadata. `ModelInfo::auto_compact_token_limit()` defaults to 90% of the resolved context window, or to a configured limit clamped to that 90% ceiling. If no context window or explicit limit is known, the runtime uses `i64::MAX`, effectively disabling auto-compact.

Triggers:

- **Pre-turn**: before recording the new user input, if current total usage is already over the limit.
- **Mid-turn**: after a sampling response, only when usage is over the limit and the model needs a follow-up or pending input exists.
- **Model downshift**: when switching to a smaller context-window model and the current token use exceeds the new model's limit.

The token signal is `Session::get_total_token_usage()`, which combines cached last API token usage with estimates after the last model-generated item. Local compaction streams a normal model request. OpenAI / Azure providers use a remote compaction path, and a newer feature-gated path expects a `context_compaction` response item.

Codex exposes configuration for `model_context_window`, `model_auto_compact_token_limit`, and `compact_prompt`. The auto limit is absolute, not a percentage, then clamped by model metadata. Hooks can run before and after manual or automatic compaction.

Key files:

- `codex-rs/protocol/src/openai_models.rs`: 90% default and configured-limit clamp.
- `codex-rs/core/src/session/turn.rs`: pre-turn, mid-turn, and model-downshift triggers.
- `codex-rs/core/src/compact.rs`: inline summarization and history replacement.
- `codex-rs/config/src/config_toml.rs`: config surface.

## opencode

opencode performs local app-level compaction through a hidden `compaction` agent. The compaction agent is tool-denied and receives prior context plus a strict Markdown summary template. It does not rely on provider-side automatic summarization.

Threshold math is based on usable input context:

- Default reserved buffer is `20_000`.
- If the provider exposes `model.limit.input`, usable tokens are `input - reserved`.
- Otherwise usable tokens are `context - maxOutputTokens(model)`.
- Auto-overflow is disabled when `compaction.auto === false` or model context is `0`.

The overflow count prefers provider `tokens.total`; when absent, it falls back to `input + output + cache.read + cache.write`. opencode also reacts to provider context overflow errors by scheduling compaction.

Compaction preserves a recent tail. Defaults are 2 user turns and a recent-token budget of 25% of usable context, clamped to 2,000-8,000 tokens unless configured. Old tool-output pruning is a separate pass: it can wipe older completed tool outputs once enough tokens are reclaimable after protecting recent results.

Config supports `compaction.auto`, `compaction.prune`, `compaction.tail_turns`, `compaction.preserve_recent_tokens`, and `compaction.reserved`. Env flags `OPENCODE_DISABLE_AUTOCOMPACT` and `OPENCODE_DISABLE_PRUNE` override config.

Key files:

- `packages/opencode/src/session/overflow.ts`: usable-context threshold.
- `packages/opencode/src/session/prompt.ts`: post-assistant and overflow-triggered compaction scheduling.
- `packages/opencode/src/session/compaction.ts`: prompt, tail preservation, pruning.
- `packages/opencode/src/agent/agent.ts`: hidden tool-denied compaction agent.

## Patterns Worth Borrowing

1. **Default-on with explicit opt-out.** All three systems treat auto-compaction as normal context hygiene, while still giving users an escape hatch.

2. **Use observed response usage.** Response usage is already available on the hot path. Pre-flight token-count calls add latency and still need estimates for dynamic system / tool content.

3. **Reserve output headroom.** Claude Code and opencode both avoid compacting exactly at the model's advertised context limit. The compact request itself needs room to produce the summary.

4. **Run at turn boundaries first.** Pre-turn or post-round compaction is much simpler than interrupting an in-flight response. Mid-turn compaction is useful only once the loop can resume safely after history replacement.

5. **Circuit-break automatic failures.** Automatic failures should not spam chat or repeatedly hit the API when the session is too large to summarize.

6. **Keep manual `/compact` independent.** Auto opt-out should not disable manual compaction unless the user explicitly disables all compaction.

## Patterns to Defer

1. **Mid-turn compaction.** Requires pausing a tool loop or assistant continuation, replacing history, and resuming the same logical turn. The first oxide-code version should compact after a complete round and before the next user-visible continuation.

2. **Microcompact / prune.** Clearing old tool outputs can save tokens, but it is a separate retention policy with its own UI and persistence implications.

3. **Anchored summary rewrites.** opencode's `<previous-summary>` pattern helps repeated compactions, but repeated lossy rewrite quality needs real usage data before adding complexity.

4. **Provider-specific remote compaction.** oxide-code talks to Anthropic Messages today, and the current manual compaction path already works through the normal stream.

5. **Automatic continue prompts.** opencode can synthesize a "Continue..." prompt after auto-compaction. oxide-code should wait for the user unless a queued prompt already exists.

6. **Hooks.** PreCompact / PostCompact hooks belong with a broader hook or workflow-skill system.
