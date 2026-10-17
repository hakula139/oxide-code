# Context Compression / `/compact`

Manual context compression triggered by `/compact [instructions]`. The driver forks the conversation into a summarization request, replaces the in-memory transcript with a synthetic continuation, and persists a `Compact` JSONL boundary so resume only sees the post-compact tail.

Companions: [commands.md](commands.md), [session/persistence.md](../session/persistence.md). Underlying research: [research/slash/compact.md](../../research/slash/compact.md).

## Implementation

`slash/compact` hosts `CompactCmd`, which forwards the op `command::compact` parses, the same typed form the bare REPL runs. Bare or whitespace-only args become `None`, and non-empty args trim into a `Some(instructions)` that the agent loop forwards verbatim into the summarization request. Both shapes echo the input line and forward `Op::Compact { instructions }`. The classifier is always `Mutating`, so the input is refused mid-turn (the in-flight reply is allowed to finish first).

`engine/compaction` owns the summarization request:

- `compact_session` forks the live conversation. It sends the conversation's own system prompt, user context, tools, and transcript, appends the rubric as the last user message, and reads the reply through the agent loop's `collect_response`.
- Its `SummaryStream` sink forwards the reply's text as `Event::SummaryToken`, passes retry notices and usage limits through, and drops its thinking. A retry, a failure, or a dropped future emits `Event::SummaryDiscarded` when any text has streamed.
- A request the API rejects with HTTP 400 `prompt is too long`, or a reply that stops on `model_context_window_exceeded`, gets up to three retries. Each retry copies the transcript without thinking blocks and drops its oldest API rounds until it frees the overflow the error reports plus about 20k tokens of output room for the summary.
- It rejects a reply cut short by `max_tokens`, a refusal, a tool call, a truncated stream, or an overflow that outlasts the retries or leaves only the newest round, and otherwise returns the trimmed summary.
- The driver is its own module so the request shape is testable in isolation.

`session/handle` exposes `compact(summary, instructions, tasks, synthetic_message) -> CompactOutcome`:

- The actor writes one `Entry::Compact` followed by the synthetic post-compact `Entry::Message` (a `role: user` carrying `SUMMARY_PREFIX + summary`, plus reminders of the task list and the open background shells when there are any), with the synthetic message's `parent_uuid` deliberately set to `None`.
- `SessionState`'s `last_message_uuid` is reset to the synthetic message's ID and `message_count` resets to `1`.
- `SessionCmd::Compact` is a batch barrier, so queued writes cannot share the same flush after the boundary.
- The file tracker is reset to match `/clear`.
- On resume, `load_session_data_from_path` treats `Entry::Compact` as a generation boundary, clears pre-compact sidecars, filters out later stale writes from the old chain, and carries `CompactInfo` so the TUI can render the boundary block while the model transcript keeps the synthetic root.

`Engine::compact` in `engine.rs` coordinates compaction:

- On success, it hands the summary and the session's task list to `engine/compact_boundary`'s `replace_session_with_summary`, which calls `session.compact(...)`, clears the file tracker, replaces the in-memory `Vec<Message>` with the synthetic continuation, and emits `Event::SessionCompacted { summary, pre_count, instructions, automatic }` with `automatic: false`.
- Failure paths (stream error, unfinished or empty summary, too-short guard) emit `Event::TurnFailed` and leave the transcript unchanged. A summary whose boundary write fails is discarded before the failure is reported.
- Cancellation routes through the existing cancel infrastructure and emits `Event::Cancelled` like a regular turn, and a quit ends the compaction without a terminal event. Dropping the summarization future discards the streamed summary first.
- Each summary request is recorded as a `usage` entry, and its estimated cost joins the session total whether the summary succeeds, fails, or is cancelled. An `Event::UsageUpdated` then shows the new total, after `SessionCompacted` on success and before the terminal event otherwise.

While the summary streams, the TUI shows it below the chat under a `Summarizing the conversation...` header, laid out like the `CompactedBlock` it becomes, and `Event::SummaryDiscarded` removes it. `App::apply_session_compacted` clears the chat, pushes a single `CompactedBlock` with a count header and markdown summary body, keeps queued prompts for the normal idle drain, clears the modal stack, and resumes idle after a manual compact. Resume renders the same boundary block and skips the synthetic root in visible chat.

## Design Decisions

1. **One path for manual and automatic compaction.** Manual `/compact` and auto-compaction share the summarization driver and the compact boundary described here. See [auto-compaction.md](../agent/auto-compaction.md) for the automatic trigger, threshold, and failure circuit breaker. Setting `client.compaction.auto_enabled = false` turns off only the automatic path.

2. **Optional free-text custom instructions.** `/compact <instructions>` lets the user steer the summary toward what they care about (`focus on the build error and how we fixed it`). The argument shape matches the existing `/rename <title>` / `/model <id>` typed-arg form, and adding it costs little. Claude Code shipped it because the rubric is generic enough that user-supplied focus noticeably improves recall.

3. **Same model and request path as a turn.** The summarization request reuses the live model selection, thinking / effort settings, and OAuth / API-key auth, because it goes through the same `Wire` and the agent loop's `collect_response`. It also inherits the turn's truncated-stream detection and stop-reason handling, so a summary cut off by `max_tokens` fails the compaction before it can replace the transcript.

4. **Keep the conversation's tools and ask for text only.** The `tools` array is part of the cached prefix, so dropping it would miss the conversation's cache. The rubric ends by asking the model not to call any tool, as Claude Code's `NO_TOOLS_PREAMBLE` does on its fork path, and a reply that stops on `tool_use` fails the compaction.

5. **Reuse the conversation's system prompt.** The system prompt belongs to the same cached prefix. The rubric and any custom instructions ride in the final user message, which is the only part of the request the conversation has not already sent.

6. **Send the transcript verbatim.** The fork's `system`, `tools`, and message prefix match what the conversation sends, so the summarizer reads the conversation's prompt cache and pays cache-read rates for most of its input. Codex forks the same way: `codex-rs/core/src/compact.rs` sends the cloned history with the compaction prompt as its last input item.

   - **Preserve the prefix.** Editing the history first (dropping tool or thinking blocks, capping tool results) would miss that cache. On models that bind thinking blocks to the conversation prefix, such as Opus 5.5, it would also invalidate the thinking blocks the request replays, which enforced accounts reject with a 400. Tool results are already capped when the tool returns, so the request is the size of the conversation's next turn.
   - **Replace history after success.** The summary replaces the whole history, so later requests replay no earlier thinking.
   - **Trim on overflow.** Retries drop the oldest API rounds and strip every thinking block, since a new turn may omit earlier thinking but must not replay it after a changed prefix. A tool-use turn is the one place the API requires its thinking, so a retry whose kept rounds end on a tool result closes that turn with a short assistant message before the rubric.
   - **Reserve summary room.** Claude Code's `truncateHeadForPTLRetry` drops the oldest API rounds, retrying a `prompt is too long` rejection up to three times, and frees just the reported overflow, or a fifth of the rounds when the error names no counts. oxide-code retries a `model_context_window_exceeded` stop the same way and frees about 20k tokens beyond the reported overflow, so the retry leaves the summary room to finish. The retries miss the cache, which a recovery path can afford, and mark no cache breakpoint on a transcript no later request reads.

7. **Synthetic post-compact user message with `parent_uuid: None`.** Materializing the summary as a `role: user` `Message` is the converged answer across all three reference CLIs, since assistant messages can't lead a turn and system blocks are special-cased at the prefix. Setting `parent_uuid: None` on the synthetic head lets the existing `chain` walker stop naturally at the boundary, with no special-case in `chain.rs`. The synthetic message body is `SUMMARY_PREFIX + "\n\n" + summary`, where `SUMMARY_PREFIX` is a curated re-entry framing taken from the Codex template that tells the next-turn model to _use_ the summary and continue without re-asking what to do. When the session keeps a [task list](../tools/tasks.md) or open [background shells](../tools/background-shells.md), a `<system-reminder>` block after the summary lists each, since the `tasks` and `bash` calls behind them are gone.

8. **`Entry::Compact` JSONL boundary.** Carries `summary`, `pre_message_count`, optional `instructions`, the task list as `tasks` when it holds any, and `timestamp`. Position: written immediately before the synthetic post-compact `Entry::Message`. Loader treats the compact entry as a chain reset, keeps `CompactInfo` for resume display, and only accepts messages that belong to the new tail.

9. **Preserve session identity.** All three reference CLIs converged on this. `/clear` rolls because intent resets, while `/compact` preserves because intent is retained and context is compressed. The JSONL file, session ID, project, and title all carry through unchanged. The chain reset is purely an in-memory or replay concern.

10. **Reset the file tracker.** `/compact` discards the read history because Edit and Write contracts depend on a Read having happened _in the visible transcript_. Since the visible transcript is now the summary, the previous `Read`s are no longer "in scope" from a user-visible standpoint. The reset forces a fresh `Read` before any `Edit`, matching the post-`/clear` behavior. The trade-off (extra Reads after compact) is the right side of the safety / convenience line.

11. **Keep queued prompts.** Unlike `/resume` (queued prompts belong to the source thread and get dropped), `/compact` _preserves_ the user's intent. Queued prompts continue to make sense after compaction, and dropping them would surprise the user. `/clear` also keeps queued prompts, though it starts a new session.

12. **Stream the summary as it generates.** A long session's summary can take tens of seconds, and streaming it shows progress and lets the user judge it early. The engine streams the text and decides its fate, so every frontend sees one protocol: `SummaryToken` chunks end in `SessionCompacted` or `SummaryDiscarded`. A discard precedes the event that reports a failure or cancellation, and a retry discards before streaming again, so a frontend never shows two attempts at once. The stdio frontends skip the stream and keep their one-line boundary notice.

13. **Refuse mid-turn (`classify = Mutating`).** All three reference CLIs treat manual compaction as a between-turns operation. `Mutating` is the existing oxide-code lever for that. The slash registry refuses if a turn is in flight, and the user retries after the in-flight reply finishes.

14. **Refuse a short transcript.** The driver refuses an empty transcript and one under about 1,000 tokens, estimated at four bytes of JSON per token, since below that the summary is about as long as the transcript itself. Text, tool calls, and tool results all count, so a session made mostly of tool output compacts as readily as a conversation. The system prompt, instructions, and tool definitions do not count, since a summary cannot shrink them. The refusal fails the compaction and leaves the session untouched.

15. **Refuse on empty summary.** If the model returns whitespace-only text (model errored quietly, content filter, etc.), fail the compaction and leave the transcript unchanged. Better to retry than to commit a useless summary.

16. **No `pre_tokens` field.** `pre_message_count` is enough for the post-compact UI line, and auto-compaction reads its trigger from the last turn's usage without persisting it.

17. **Custom instructions appended verbatim under an "Additional instructions" section in the user message.** Matches Claude Code's pattern: the rubric runs first, then custom instructions follow as steering for the same task.

18. **`CompactedBlock` is one chat block.** The accent-bar surface gives compaction a visual identity while keeping the header and summary one conceptual unit.

19. **`echoes_input` keeps the trait default of `true`.** The `> /compact` line shows while the summarization runs. The compact event then clears chat down to the `CompactedBlock`, whose `(focus: ...)` suffix keeps any instructions visible.

## Per-Component Notes

- **`CompactCmd`**: `name = "compact"`, no aliases. `description = "Compress conversation context into a summary"`. `usage() = Some("[<instructions>]")`. `classify` is always `Mutating`. `echoes_input` uses the trait default of `true`. `execute` returns `SlashOutcome::Forward(Action::Op(command::compact(args)))`, which trims the args into `instructions`, or `None` when nothing is left.

- **`Op::Compact { instructions }`**: `Option<String>`. Empty or whitespace input becomes `None` in `command::compact` so `Engine::compact` doesn't repeat the trim.

- **`Event::SummaryToken(text)`**, **`Event::SummaryDiscarded`**: The summary in flight and its discard. Manual and automatic compaction emit both, and the TUI renders the stream below the chat with `CompactedBlock::render_streaming`.

- **`Event::SessionCompacted { summary, pre_count, instructions, automatic }`**: Emitted post-success. `summary` is the rendered body, `pre_count` is for the count header, `instructions` becomes the header's `(focus: ...)` suffix, and `automatic` is false for `/compact` and true for auto-compaction. The TUI repaints from it, and `StdioSink` prints a `Compacted N messages into summary` line to stderr (`Auto-compacted` when automatic).

- **`engine::compaction::compact_session`**: Async fn taking the client, the conversation's `PromptParts` and tool definitions, the transcript, optional instructions, the event sink the summary streams to, and a `&mut Vec<TokenUsage>` that receives the usage of every request it sends, even when the summary fails or the caller drops the future. Returns the trimmed summary or an error.

- **`SUMMARIZATION_USER_RUBRIC`**, **`SUMMARY_PREFIX`**: Two `&'static str` constants. The rubric is the terse list (intent, decisions, code paths touched, current state, next step) and ends by asking for text only. The prefix is the next-turn framing prepended to the synthetic message.

- **`Engine::compact` (`engine.rs`)**: Drive `compact_session` with the conversation's prompt and tool definitions, record each summary request as a `usage` entry, add its cost to the session total, and surface failure as `Event::TurnFailed`. On success, `replace_session_with_summary` synthesizes the continuation and calls `session.compact(summary, instructions, synthetic_message)`. A session-write failure surfaces through the sink and ends the compaction with `TurnFailed` before the file tracker or the in-memory transcript changes. Otherwise it clears the file tracker, swaps the in-memory `Vec<Message>` for the synthetic continuation, and emits `SessionCompacted`.

- **`Entry::Compact`**: Variant on the internally tagged `Entry` enum. Fields: `summary: String`, `pre_message_count: u32`, `instructions: Option<String>`, `timestamp: OffsetDateTime`. Tagged `"type": "compact"` (lowercase, snake_case).

- **`SessionCmd::Compact`**: Actor command carrying the new state. Writes `Entry::Compact` and the synthetic post-compact `Entry::Message` in one batched flush, resets `last_message_uuid` to the synthetic message ID, resets `message_count` to `1`, and forces a batch flush before later queued commands run. Acks via `oneshot` like the rest of `SessionCmd`.

- **`session::handle::compact`**: Async API. Sends `SessionCmd::Compact`, awaits the ack, and returns `CompactOutcome { pre_count, failure }`. Like `archive`, it reports a failure whenever the command or its ack cannot reach the writer task. The caller clears the file tracker only after the boundary flush succeeds.

- **`CompactedBlock`**: Chat block with an accent-bar surface, a `Compacted N messages → 1 summary` header (with a `(focus: ...)` suffix when instructions were given), and the rendered summary markdown body. No footer. Reuses the existing markdown renderer. `CompactedBlock::render_streaming` renders the summary in flight under a `Summarizing the conversation...` header.

- **`apply_session_compacted` (TUI)**: Clears the chat, pushes a single `CompactedBlock`, keeps queued prompts for the normal idle drain, clears the modal stack, and resumes idle unless the compaction was automatic.

- **`ChainBuilder::resolve`**: Needs no compaction-specific logic. The store loader starts a fresh `ChainBuilder` at the compact boundary, and the walk back from the latest tail leaf stops at the post-compact head because its `parent_uuid` is `None`.

- **`session::sanitize`**: Treats the synthetic continuation as a normal `role: user` text message without dropping anything.

## Out of Scope

- **Anchored re-compaction (`<previous-summary>` block).** Updating a previous summary in place pays off only when one session is compacted many times. Each compaction summarizes the transcript since the last boundary, which already starts from the previous summary.

- **Microcompact / prune (in-place tool-result body deletion).** Claude Code ships this because its auto-compaction is more aggressive. Whole-transcript summaries at turn boundaries cover the same need with one mechanism.

## Related Issues

A `pre_compact` hook is tracked in [#168](https://github.com/hakula139/oxide-code/issues/168).

## Sources

- `crates/oxide-code/src/client.rs`: `PromptTooLong`, the wire-neutral rejection that carries the reported overflow.
- `crates/oxide-code/src/client/anthropic/transport.rs`: `prompt_too_long_message`, which types an HTTP 400 `prompt is too long` as `PromptTooLong`.
- `crates/oxide-code/src/engine.rs`: `Engine::compact` beside `Engine::resume`, and session-cost accounting.
- `crates/oxide-code/src/engine/abort.rs`: `await_unless_aborted`, which races the summarization against cancel and quit.
- `crates/oxide-code/src/engine/compact_boundary.rs`: `replace_session_with_summary`, shared by manual and automatic compaction.
- `crates/oxide-code/src/engine/compaction.rs`: `compact_session` driver, the `SummaryStream` sink, the context-window retries (`drop_oldest_rounds`, `retry_request`), and the `SUMMARIZATION_USER_RUBRIC` constant.
- `crates/oxide-code/src/engine/turn.rs`: `collect_response`, which reads the summary the way it reads a turn's reply.
- `crates/oxide-code/src/protocol.rs`: `Op::Compact`, `Event::SummaryToken`, `Event::SummaryDiscarded`, `Event::SessionCompacted`.
- `crates/oxide-code/src/session/actor.rs`: `SessionCmd::Compact` handler.
- `crates/oxide-code/src/session/entry.rs`: `Entry::Compact` variant.
- `crates/oxide-code/src/session/handle.rs`: `SessionHandle::compact`, `CompactOutcome`.
- `crates/oxide-code/src/session/post_compact.rs`: `SUMMARY_PREFIX`, `synthesize_post_compact_message` and its `task_reminder` and `shell_reminder` blocks, plus `strip_synthetic_post_compact_prefix`, which hides the prefix when a resumed session displays it.
- `crates/oxide-code/src/session/state.rs`: `compact_entries` and `commit_compact` (chain reset).
- `crates/oxide-code/src/session/store.rs`: resume loader compact boundary reset, tail filtering, and `CompactInfo`.
- `crates/oxide-code/src/slash/compact.rs`: `CompactCmd`.
- `crates/oxide-code/src/slash/registry.rs`: `BUILT_INS` entry `&CompactCmd`.
- `crates/oxide-code/src/tui/app/events.rs`: `apply_session_compacted`.
- `crates/oxide-code/src/tui/components/chat.rs`: `ChatView::append_summary_token` and `ChatView::discard_summary`.
- `crates/oxide-code/src/tui/components/chat/blocks/compacted.rs`: `CompactedBlock`.
