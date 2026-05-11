# Context Compression / `/compact`

Manual context compression triggered by `/compact [instructions]`. The agent loop streams a one-shot summarization request through the live model, replaces the in-memory transcript with a synthetic continuation, and persists a `Compact` JSONL boundary so resume only sees the post-compact tail.

Companions: [commands.md](commands.md), [session/persistence.md](../session/persistence.md). Underlying research: [research/slash/compact.md](../../research/slash/compact.md).

## Implementation

`slash/compact` hosts `CompactCmd`. Bare or whitespace-only args become `None`, and non-empty args trim into a `Some(instructions)` that the agent loop forwards verbatim into the summarization request. Both shapes echo the input line and forward `UserAction::Compact { instructions }`. The classifier is always `Mutating`, so the input is refused mid-turn (the in-flight reply is allowed to finish first).

`agent/compaction` is a small driver module. `compact_session` builds a stripped transcript (user-text-only, see below), composes a one-shot `Client` request with an empty tool registry and a dedicated minimal system prompt, drains the stream into a single `String`, then dispatches a `SessionCmd::Compact` over the actor channel. The driver is its own module rather than agent-loop code so the request shape is testable in isolation.

`session/handle` gains `compact(summary, pre_count, instructions, parent_anchor) -> CompactOutcome`. The actor writes one `Entry::Compact` followed by the synthetic post-compact `Entry::Message` (a `role: user` carrying `SUMMARY_PREFIX + summary`), with the synthetic message's `parent_uuid` deliberately set to `None`. `SessionState`'s `last_message_uuid` is reset to the synthetic message's id and `message_count` resets to `1`. The file tracker is reset to match `/clear`. Pre-compact `FileSnapshot` entries already on disk become inaccessible the same way pre-compact messages do, since `chain` walks back via `parent_uuid` and stops at the post-compact head.

The agent loop adds `apply_compact`: drive the streaming summarization, on success call `session.compact(...)`, replace the in-memory `Vec<Message>` with the synthetic continuation, and emit `AgentEvent::SessionCompacted { summary, pre_count, instructions }`. Failure paths (stream error, empty summary, too-few-messages guard, channel close) emit `AgentEvent::Error` and leave the session untouched. Cancellation routes through the existing cancel infrastructure and emits `AgentEvent::Cancelled` like a regular turn.

The TUI's `App::apply_session_compacted` clears the chat, replays the synthetic continuation as a single `CompactedBlock` (count header plus summary markdown body in a bordered surface), keeps queued prompts (since compact preserves intent), preserves the modal stack, and resumes idle.

## Design Decisions

1. **Manual-only in v1.** Auto-compact is the deferred half of the roadmap's "Context Compression" entry. Manual `/compact` ships first because it doesn't need a token-budget oracle, doesn't fire mid-stream, and the user can always retry. Threshold math, telemetry, and circuit breaker land with auto-compact in a follow-up PR.

2. **Optional free-text custom instructions.** `/compact <instructions>` lets the user steer the summary toward what they care about (`focus on the build error and how we fixed it`). The argument shape matches the existing `/rename <title>` / `/model <id>` typed-arg form, and adding it costs little. Claude Code shipped it because the rubric is generic enough that user-supplied focus noticeably improves recall.

3. **Same model plus streaming via the existing `Client`.** The summarization request reuses the live model selection, thinking / effort settings, and OAuth / API-key auth, since it's just another `Client::stream` call with a custom system prompt and empty tools. No separate code path is needed. opencode's `agent.compaction.model` config knob is genuinely useful but unnecessary for v1.

4. **Empty tool registry on the compaction request.** Passing `Vec::new()` for tools hard-bans tool calls at the API layer, rather than relying on prompt-only enforcement. Claude Code's `NO_TOOLS_PREAMBLE` is forceful prose but still relies on the model honoring it. The empty-registry path is simpler and unconditional.

5. **Dedicated minimal system prompt for the compaction request.** The regular system prompt mentions tools, environment, and instructions in a way that primes the model to act rather than summarize. The compaction system prompt is a single sentence: _"You are summarizing a conversation between an engineer and an AI coding assistant. Output ONLY the summary text. Do not call any tools."_ The rubric and any custom instructions ride in the user message.

6. **Strip non-conversational blocks from the summarizer input.** Drop `tool_use`, `tool_result`, and thinking blocks before sending the transcript to the summarizer. Codex's pattern is the strictest and the simplest. The assistant text the model already produced gives the summarizer enough to reconstruct what was decided. Tool inputs and outputs blow up request size for marginal recall gain, because file paths and decisions are already mentioned in the assistant text around the tool calls.

7. **Synthetic post-compact user message with `parent_uuid: None`.** Materializing the summary as a `role: user` `Message` is the converged answer across all three reference CLIs, since assistant messages can't lead a turn and system blocks are special-cased at the prefix. Setting `parent_uuid: None` on the synthetic head lets the existing `chain` walker stop naturally at the boundary, with no special-case in `chain.rs`. The synthetic message body is `SUMMARY_PREFIX + "\n\n" + summary`, where `SUMMARY_PREFIX` is a curated re-entry framing taken from the Codex template that tells the next-turn model to _use_ the summary rather than re-asking what to do.

8. **New `Entry::Compact` JSONL variant.** Carries `summary`, `pre_message_count`, optional `instructions`, and `timestamp`. Position: written immediately before the synthetic post-compact `Entry::Message`. Loader treats the boundary as a chain reset because the synthetic message's `parent_uuid` is already `None`. The `Compact` line is metadata for `--list` (so listings can show "compacted N → 1") and for future "view full pre-compact transcript" tooling. `Entry::Unknown` catch-all means older binaries skip it gracefully.

9. **Same session id, do not roll.** All three reference CLIs converged on this. `/clear` rolls (intent reset), while `/compact` preserves (intent retained, context compressed). The JSONL file, session id, project, and title all carry through unchanged. The chain reset is purely an in-memory or replay concern.

10. **Reset the file tracker.** `/compact` discards the read history because Edit and Write contracts depend on a Read having happened _in the visible transcript_. Since the visible transcript is now the summary, the previous `Read`s are no longer "in scope" from a user-visible standpoint. The reset forces a fresh `Read` before any `Edit`, matching the post-`/clear` behavior. The trade-off (extra Reads after compact) is the right side of the safety / convenience line.

11. **Keep queued prompts.** Unlike `/resume` (queued prompts belong to the source thread and get dropped), `/compact` _preserves_ the user's intent. Queued prompts continue to make sense after compaction, and dropping them would surprise the user. This mirrors `/clear`'s "same identity, fresh slate" behavior, even though the underlying mechanic is different.

12. **No live token streaming in v1.** Show the spinner during compaction, then emit `AgentEvent::SessionCompacted` once with the full summary at the end. Live-streaming the summary into a `StreamingCompactionBlock` is a v2 nicety because it requires an extra block variant, finalize-on-completion plumbing, and "what if the user cancels mid-stream" handling. Not worth it for the typical 5-15 second wait.

13. **Refuse mid-turn (`classify = Mutating`).** All three reference CLIs treat manual compaction as a between-turns operation. `Mutating` is the existing oxide-code lever for that. The slash registry refuses if a turn is in flight, and the user retries after the in-flight reply finishes.

14. **Refuse on too-few messages.** The driver requires at least 4 transcript messages (2 user + 2 assistant turns or equivalent), since below that the summary is more verbose than the transcript itself. The refusal surfaces as a system message (`Session is too short to compact`) rather than an error block, since this is a normal state.

15. **Refuse on empty summary.** If the model returns whitespace-only text (model errored quietly, content filter, etc.), surface as an `AgentEvent::Error` and leave the session untouched. Better to retry than to commit a useless summary.

16. **No `pre_tokens` field in v1.** Token tracking lives next to the auto-compact work. Manual compact doesn't need it, since `pre_message_count` is enough for the post-compact UI line.

17. **Custom instructions appended verbatim under an "Additional instructions" section in the user message.** Matches Claude Code's pattern: the rubric runs first, then custom instructions follow as steering for the same task.

18. **`CompactedBlock` is one chat block.** The top-bordered surface gives compaction a visual identity while keeping the header and summary one conceptual unit. The block can later grow a "view full pre-compact transcript" footer or a token-saved indicator without coordinating two block types.

19. **`echoes_input` returns true.** The user's `> /compact <instructions>` line stays in scrollback above the `CompactedBlock` so the operation is visible in history. Bare `/compact` echoes too, so the prompt line plus boundary block reads as a single coherent operation.

## Per-Component Notes

- **`CompactCmd`**: `name = "compact"`, no aliases. `description = "Compress conversation context into a summary"`. `argument_hint = "[instructions]"`. `classify` is always `Mutating`. `echoes_input` returns true. `execute` parses args via `args.trim()` and returns `SlashOutcome::Forward(UserAction::Compact { instructions })` with `instructions = (!s.is_empty()).then_some(s.to_owned())`.

- **`UserAction::Compact { instructions }`**: `Option<String>`. Empty or whitespace input becomes `None` at the slash boundary so the agent loop's `apply_compact` doesn't repeat the trim.

- **`AgentEvent::SessionCompacted { summary, pre_count, instructions }`**: Emitted post-success. `summary` is the rendered body, `pre_count` is for the count header, and `instructions` is forwarded for log / telemetry. App-only reaction. `StdioSink` ignores it (same convention as `SessionResumed`).

- **`agent::compaction::compact_session`**: Async fn taking `&Client`, `&[Message]`, `Option<&str>`, returns `Result<String>`. Composes the system prompt, strips the transcript to user-text plus assistant-text only, builds a one-shot `CreateMessageRequest` with `tools: Vec::new()`, drains the stream into a single `String`, returns the trimmed summary or an error.

- **`SUMMARIZATION_SYSTEM`**, **`SUMMARIZATION_USER_RUBRIC`**, **`SUMMARY_PREFIX`**: Three `&'static str` constants. System prompt is one paragraph. Rubric is the terse list (intent, decisions, code paths touched, current state, next step). Prefix is the next-turn framing prepended to the synthetic message.

- **`apply_compact` (agent loop)**: Drive `compact_session`, surface failure as `AgentEvent::Error`, on success call `session.compact(summary, pre_count, instructions)`, swap in-memory `Vec<Message>` with the synthetic continuation, emit `SessionCompacted`, and surface session-write failure via the existing sink helper.

- **`Entry::Compact`**: New variant on the externally-tagged `Entry` enum. Fields: `summary: String`, `pre_message_count: u32`, `instructions: Option<String>`, `timestamp: OffsetDateTime`. Tagged `"type": "compact"` (lowercase, snake_case). Rejected gracefully via `Entry::Unknown` for older binaries.

- **`SessionCmd::Compact`**: Actor command carrying the new state. Writes `Entry::Compact` and the synthetic post-compact `Entry::Message` in one batched flush, resets `last_message_uuid` to the synthetic message id, and resets `message_count` to `1`. Acks via `oneshot` like the rest of `SessionCmd`.

- **`session::handle::compact`**: Async API. Snapshots the file tracker, clears it, sends `SessionCmd::Compact`, awaits the ack, and returns `CompactOutcome { synthetic_message: Message, finalize_failure: Option<String> }`.

- **`CompactedBlock`**: Chat block with a top-bordered surface, a `Compacted N messages` header (themed `dim`), and the rendered summary markdown body. No footer. Reuses the existing markdown renderer.

- **`apply_session_compacted` (TUI)**: Clears the chat, replays the synthetic continuation as a single `CompactedBlock`, preserves queued prompts (shows count if non-zero via system message), preserves the modal stack, and resumes idle.

- **`chain::pick_chain`**: No change needed. Walking from the latest leaf back via `parent_uuid` naturally stops at the post-compact head because `parent_uuid: None`.

- **`session::sanitize`**: No change needed. The synthetic continuation is a normal `role: user` message with text content, and sanitize drops nothing.

- **`SessionInfo` / `--list`**: No change in v1. Future work surfaces "compacted N times" alongside `Msgs`. Free at write time once `Entry::Compact` lines exist, and a follow-up PR can add the column.

## Out of Scope / Deferred

- **Auto-compact.** Token-budget oracle, threshold math (95% of effective context window, `(window - max_output) - safety_buffer`), pre-turn check, single-turn circuit breaker, opt-out config knob (`compact.auto = false`). Roadmap-tracked. Lands after manual is shipped and exercised.

- **Live-streamed summary.** `StreamingCompactionBlock` finalizing into `CompactedBlock`, tokenwise rendering, mid-stream cancel handling. v2 polish.

- **Anchored re-compaction (`<previous-summary>` block).** opencode's signature pattern. Genuinely useful for ultra-long sessions, but defer until a user has compacted the same session twice.

- **Microcompact / prune (in-place tool-result body deletion).** Both Claude Code and opencode ship this because their auto-compact is more aggressive. Land after auto-compact.

- **Partial / range compaction.** Claude Code's "summarize from here" / "summarize up to here" via the message selector. Adds a UI surface, a dual-direction summarizer, and a transcript splice. Its own feature.

- **PreCompact / PostCompact hooks.** All three CLIs ship hooks, but oxide-code doesn't have a hook surface yet. Lands with the broader hooks abstraction.

- **`agent.compaction.model` config knob.** Letting users pin compaction to a cheap model (Haiku) while the main session runs Opus. Useful, but defer until users ask.

- **`pre_tokens` field on `Entry::Compact`.** Token-tracking infrastructure lands with auto-compact. Manual compact doesn't need it.

- **Pre-flight `count_tokens` API call.** All three reference CLIs use the API's `usage.total_tokens` from the previous response and accept the drift. When auto-compact ships, follow that convention.

- **`/cost` companion.** Token / cost telemetry surface. Status-bar redesign plus `/cost` listed under "Later" in the roadmap.

## Sources

- `crates/oxide-code/src/agent.rs`: agent-loop dispatch table. `apply_compact` lives here.
- `crates/oxide-code/src/agent/compaction.rs`: new module containing `compact_session` driver plus `SUMMARIZATION_SYSTEM` / `SUMMARIZATION_USER_RUBRIC` / `SUMMARY_PREFIX` constants.
- `crates/oxide-code/src/agent/event.rs`: `UserAction::Compact`, `AgentEvent::SessionCompacted`.
- `crates/oxide-code/src/main.rs`: `apply_compact` helper alongside `apply_resume`.
- `crates/oxide-code/src/session/actor.rs`: `SessionCmd::Compact` handler.
- `crates/oxide-code/src/session/entry.rs`: `Entry::Compact` variant.
- `crates/oxide-code/src/session/handle.rs`: `SessionHandle::compact`, `CompactOutcome`.
- `crates/oxide-code/src/session/state.rs`: `SessionState::compact` (chain reset).
- `crates/oxide-code/src/slash/compact.rs`: `CompactCmd`.
- `crates/oxide-code/src/slash/registry.rs`: `BUILT_INS` adds `&CompactCmd`.
- `crates/oxide-code/src/tui/app.rs`: `apply_session_compacted`.
- `crates/oxide-code/src/tui/components/chat/blocks/compacted.rs`: new `CompactedBlock`.
