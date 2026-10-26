# Context Compression / `/compact`

Manual context compression triggered by `/compact [instructions]`. The driver forks the conversation into a summarization request, replaces the in-memory transcript with a synthetic continuation, and persists a `Compact` JSONL boundary so resume only sees the post-compact tail.

Companions: [commands.md](commands.md), [session/persistence.md](../session/persistence.md). Underlying research: [research/slash/compact.md](../../research/slash/compact.md).

## Implementation

`slash/compact` hosts `CompactCmd`, listed as `/compact [<instructions>]` with the description `Compress conversation context into a summary` and no aliases. It forwards the op parsed by `command::compact`, using the same typed form as the bare REPL. Bare or whitespace-only args become `None`, and non-empty args trim into a `Some(instructions)` that the agent loop forwards verbatim into the summarization request. Both shapes echo the input line and forward `Op::Compact { instructions }`. The classifier is always `Mutating`, so the input is refused mid-turn (the in-flight reply is allowed to finish first).

`engine/compaction` owns the summarization request:

- `request_summary` forks the live conversation. It sends the conversation's own system prompt, user context, tools, and transcript, appends the rubric as the last user message, and reads the reply through the agent loop's `collect_response`. The rubric, `SUMMARIZATION_USER_RUBRIC`, asks for a terse list of the intent, decisions, code paths touched, current state, and next step, and ends by asking for text only. Every request it sends records its usage, even when the summary fails or the caller drops the future.
- Its `SummaryStream` sink forwards the reply's text as `Event::SummaryToken` for manual and automatic compaction alike, passes retry notices and usage limits through, and drops its thinking. A retry, a failure, or a dropped future emits `Event::SummaryDiscarded` when any text has streamed.
- A request the API rejects with HTTP 400 `prompt is too long`, or a reply that stops on `model_context_window_exceeded`, gets up to three retries. Each retry copies the transcript without thinking blocks and drops its oldest API rounds until it frees the overflow the error reports plus about 20k tokens of output room for the summary.
- It rejects a reply cut short by `max_tokens`, a refusal, a tool call, a pause or an unknown stop reason that ends the summary early, a truncated stream, or an overflow that outlasts the retries or leaves only the newest round, and otherwise returns the trimmed summary.

`session/handle` exposes `compact(boundary, synthetic_message) -> CompactOutcome`, where `CompactBoundary` carries the summary, instructions, task list, and the session context the rebuilt prompt starts from:

- The actor writes one `Entry::Compact`, tagged `"type": "compact"` with the `summary`, `pre_message_count`, optional `instructions`, the `tasks` (omitted when empty), and a `timestamp`. An `Entry::Context` holding that session context and the synthetic post-compact `Entry::Message` follow it. The message is a `role: user` carrying `SUMMARY_PREFIX + summary`, plus reminders of the task list and the open background shells when there are any, and after a mid-turn compaction a reminder to resume the task, with its `parent_uuid` set to `None`. The context and the message share a timestamp, which binds the context to the synthetic root on resume.
- `SessionState`'s `last_message_uuid` is reset to the synthetic message's ID and `message_count` resets to `1`.
- `SessionCmd::Compact` is a batch barrier, so queued writes cannot share the same flush after the boundary.
- It returns `CompactOutcome { pre_count, checkpoint, failure }`, and like `archive`, it reports a failure whenever the command or its ack cannot reach the writer task. `checkpoint` is the session state just after the synthetic message, set once the boundary takes effect, on disk or, after persistence has stopped, in memory alone. A turn that compacts partway through rolls a later refusal back to it.
- The caller resets the file tracker, as `/clear` does, only once the boundary commits.
- On resume, `load_session_data_from_path` treats `Entry::Compact` as a generation boundary, clears pre-compact sidecars, filters out later stale writes from the old chain, and carries `CompactInfo` so the TUI can render the boundary block while the model transcript keeps the synthetic root. The loader starts a fresh `ChainBuilder` at the boundary, and the walk back from the latest tail leaf stops at the post-compact head because its `parent_uuid` is `None`, so `ChainBuilder::resolve` needs no compaction logic. `session::sanitize` treats the synthetic continuation as an ordinary `role: user` text message.

`Engine::compact` in `engine/compaction.rs` coordinates compaction through `Engine::run_compaction`, the pipeline auto-compaction also runs:

- On success, it reloads the instruction files and hands the summary, the session's task list, and the refreshed session context to `Engine::replace_with_summary`, which builds the synthetic continuation and passes it to `LiveTranscript::compact`. That method saves the boundary and then, once the boundary commits, clears the file tracker, replaces the in-memory messages, and adopts the refreshed prompt. The engine emits `Event::SessionCompacted { summary, pre_count, instructions, trigger }` with `trigger: CompactTrigger::Manual`, shows any instruction warning the rebuilt prompt newly raises, and ends the compaction with `SessionReady`.
- Failure paths (stream error, unfinished or empty summary, too-short guard) emit `Event::TurnFailed` and leave the transcript unchanged. A summary whose boundary write fails is discarded before the failure is reported. Once session persistence has stopped, the summary still replaces the transcript, and the stop is reported after `SessionCompacted`.
- Cancellation emits `Event::Cancelled` like a regular turn, and a quit ends the compaction without a terminal event. Dropping the summarization future discards the streamed summary first.
- Each summary request is recorded as a `usage` entry, and its estimated cost joins the session total whether the summary succeeds, fails, or is cancelled. An `Event::UsageUpdated` then shows the new total, between `SessionCompacted` and `SessionReady` on success, and before the terminal event when a failed or cancelled summary request reported usage.

While the summary streams, the TUI shows it below the chat under a `Summarizing the conversation...` header through `CompactedBlock::render_streaming`, laid out like the block it becomes, and `Event::SummaryDiscarded` removes it. `App::apply_session_compacted` clears the chat, pushes a single `CompactedBlock`, keeps queued prompts, and clears the modal stack. The block has an accent-bar surface and no footer. It fills its `Compacted N messages into a summary` header from `pre_count`, adding a `(focus: ...)` suffix when `instructions` were given, and renders `summary` as Markdown below it. At `Event::SessionReady`, a manual compact returns to idle and submits the queue head, so the prompt renders below the notices the rebuilt prompt raises, or only returns to idle after a cancel that lost the race to the swap. Resume renders the same boundary block and skips the synthetic root in visible chat.

The stdio driver's `StdioSink` prints the same header on stderr, which `protocol::notice::compacted` words for both frontends.

## Design Decisions

1. **One path for manual and automatic compaction.** Manual `/compact` and auto-compaction share the summarization driver and the compact boundary described here. See [auto-compaction.md](../agent/auto-compaction.md) for the automatic trigger, threshold, and failure circuit breaker. Setting `client.compaction.auto_enabled = false` turns off only the automatic path.

2. **Optional free-text custom instructions.** `/compact <instructions>` lets the user steer the summary toward what they care about (`focus on the build error and how we fixed it`). The argument takes the same typed-arg form as `/rename <title>` and `/model <id>`, and Claude Code accepts the same argument.

3. **Same model and request path as a turn.** The summarization request reuses the live model selection, thinking / effort settings, and OAuth / API-key auth, because it goes through the same `Wire` and the agent loop's `collect_response`, which also detects a truncated stream. `request_summary` checks the stop reason itself, so a summary cut off by `max_tokens` or stopped early fails the compaction before it can replace the transcript.

4. **Keep the conversation's tools and ask for text only.** The `tools` array is part of the cached prefix, so dropping it would miss the conversation's cache. The rubric ends by asking the model not to call any tool, as Claude Code's `NO_TOOLS_PREAMBLE` does on its fork path, and a reply that stops on `tool_use` fails the compaction.

5. **Reuse the conversation's system prompt.** The system prompt belongs to the same cached prefix. The rubric and any custom instructions ride in the final user message, which is the only part of the request the conversation has not already sent.

6. **Send the transcript verbatim.** The fork's `system`, `tools`, and message prefix match what the conversation sends, so the summarizer reads the conversation's prompt cache and pays cache-read rates for most of its input. Codex forks the same way: `codex-rs/core/src/compact.rs` sends the cloned history with the compaction prompt as its last input item.

   - **Preserve the prefix.** Editing the history first (dropping tool or thinking blocks, capping tool results) would miss that cache. On models that bind thinking blocks to the conversation prefix, such as Opus 5.5, it would also invalidate the thinking blocks the request replays, which enforced accounts reject with a 400. Tool results are already capped when the tool returns, so the request is the size of the conversation's next turn.
   - **Replace history after success.** The summary replaces the whole history, so later requests replay no earlier thinking.
   - **Trim on overflow.** Retries drop the oldest API rounds and strip every thinking block, since a new turn may omit earlier thinking but must not replay it after a changed prefix. A tool-use turn is the one place the API requires its thinking, so a retry whose kept rounds end on a tool result closes that turn with a short assistant message before the rubric.
   - **Reserve summary room.** Claude Code's `truncateHeadForPTLRetry` drops the oldest API rounds, retrying a `prompt is too long` rejection up to three times, and frees just the reported overflow, or a fifth of the rounds when the error names no counts. oxide-code retries a `model_context_window_exceeded` stop the same way and frees about 20k tokens beyond the reported overflow, so the retry leaves the summary room to finish. The retries miss the cache, which a recovery path can afford, and mark no cache breakpoint on a transcript no later request reads.

7. **Synthetic post-compact user message with `parent_uuid: None`.** Materializing the summary as a `role: user` `Message` is the converged answer across all three reference CLIs, since assistant messages can't lead a turn and system blocks are special-cased at the prefix. Setting `parent_uuid: None` on the synthetic head lets the `chain` walker stop at the boundary with no special case in `chain.rs`. The synthetic message body is `SUMMARY_PREFIX + "\n\n" + summary`, where `SUMMARY_PREFIX` is a curated re-entry framing taken from the Codex template that tells the next-turn model to _use_ the summary and continue without re-asking what to do. When the session keeps a [task list](../tools/tasks.md) or open [background shells](../tools/background-shells.md), a `<system-reminder>` block after the summary lists each, since the `tasks` and `bash` calls behind them are gone.

8. **`Entry::Compact` JSONL boundary.** The compact entry is written immediately before the refreshed `Entry::Context` and the synthetic post-compact `Entry::Message`. The loader treats it as a chain reset, keeps `CompactInfo` for resume display, and only accepts messages that belong to the new tail.

9. **Preserve session identity.** All three reference CLIs converged on this. `/clear` rolls because intent resets, while `/compact` preserves because intent is retained and context is compressed. The JSONL file, session ID, project, and title all carry through unchanged. The chain reset is purely an in-memory or replay concern.

10. **Reset the file tracker.** `/compact` discards the read history because Edit and Write contracts depend on a Read having happened _in the visible transcript_. Since the visible transcript is now the summary, the previous `Read`s are no longer "in scope" from a user-visible standpoint. The reset forces a fresh `Read` before any `Edit`, matching the post-`/clear` behavior, at the cost of extra Reads after a compaction.

11. **Keep queued prompts.** A session switch (`/clear`, bare `/archive`, or `/resume`) drops the prompts queued before it because they belong to the conversation that ended, as [cancellation.md](../tui/cancellation.md) decision 5 records. `/compact` _preserves_ the user's intent, so queued prompts continue to make sense after it, and dropping them would surprise the user.

12. **Stream the summary as it generates.** A long session's summary can take tens of seconds, and streaming it shows progress and lets the user judge it early. The engine streams the text and decides its fate, so every frontend sees one protocol: `SummaryToken` chunks end in `SessionCompacted` or `SummaryDiscarded`. A discard precedes the event that reports a failure or cancellation, and a retry discards before streaming again, so a frontend never shows two attempts at once. The stdio frontends skip the stream and keep their one-line boundary notice.

13. **Refuse mid-turn (`classify = Mutating`).** All three reference CLIs treat manual compaction as a between-turns operation. The slash registry refuses a `Mutating` command while a turn is in flight, and the user retries after the reply finishes.

14. **Refuse a short transcript.** The driver refuses an empty transcript and one under about 1,000 tokens, estimated at four bytes of JSON per token, since below that the summary is about as long as the transcript itself. Text, tool calls, and tool results all count, so a session made mostly of tool output compacts as readily as a conversation. The system prompt, instructions, and tool definitions do not count, since a summary cannot shrink them. The refusal fails the compaction and leaves the session untouched.

15. **Refuse on empty summary.** If the model returns whitespace-only text (model errored quietly, content filter, etc.), fail the compaction and leave the transcript unchanged. Better to retry than to commit a useless summary.

16. **No `pre_tokens` field.** `pre_message_count` is enough for the post-compact UI line, and auto-compaction estimates its threshold signal from the latest reported usage without persisting it.

17. **Custom instructions appended verbatim under an "Additional instructions" section in the user message.** Matches Claude Code's pattern: the rubric runs first, then custom instructions follow as steering for the same task.

18. **`CompactedBlock` is one chat block.** The accent-bar surface gives compaction a visual identity while keeping the header and summary one conceptual unit.

19. **`echoes_input` keeps the trait default of `true`.** The `> /compact` line shows while the summarization runs. The compact event then clears chat down to the `CompactedBlock`, whose `(focus: ...)` suffix keeps any instructions visible.

## Out of Scope

- **Anchored re-compaction (`<previous-summary>` block).** Updating a previous summary in place pays off only when one session is compacted many times. Each compaction summarizes the transcript since the last boundary, which already starts from the previous summary.

- **Microcompact / prune (in-place tool-result body deletion).** Claude Code ships this because its auto-compaction is more aggressive. Whole-transcript summaries at turn and round boundaries cover the same need with one mechanism.

- **`pre_compact` hook.** Tracked in [#168](https://github.com/hakula139/oxide-code/issues/168).

## Sources

- `crates/ox-core/src/client.rs`: `PromptTooLong`, the wire-neutral rejection that carries the reported overflow.
- `crates/ox-core/src/client/anthropic/transport.rs`: `prompt_too_long_message`, which types an HTTP 400 `prompt is too long` as `PromptTooLong`.
- `crates/ox-core/src/engine/abort.rs`: `await_unless_aborted`, which races the summarization against cancel and quit.
- `crates/ox-core/src/engine/compaction.rs`: `Engine::compact`, `Engine::run_compaction` and `Engine::replace_with_summary`, shared by manual and automatic compaction, the `request_summary` driver, the `SummaryStream` sink, the context-window retries (`drop_oldest_rounds`, `retry_request`), and the `SUMMARIZATION_USER_RUBRIC` constant.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::compact`, which saves the boundary and swaps the transcript to the synthetic continuation.
- `crates/ox-core/src/engine/turn.rs`: `collect_response`, which reads the summary the way it reads a turn's reply.
- `crates/ox-core/src/engine/usage.rs`: session-cost accounting.
- `crates/ox-core/src/protocol.rs`: `Op::Compact`, `Event::SummaryToken`, `Event::SummaryDiscarded`, `Event::SessionCompacted`.
- `crates/ox-core/src/protocol/notice.rs`: `compacted`, the boundary header every frontend shows.
- `crates/ox-core/src/session/actor.rs`: `SessionCmd::Compact` handler.
- `crates/ox-core/src/session/chain.rs`: `ChainBuilder`, whose `resolve` walks back from the latest tail leaf.
- `crates/ox-core/src/session/entry.rs`: `Entry::Compact` variant.
- `crates/ox-core/src/session/handle.rs`: `SessionHandle::compact`, `CompactOutcome`.
- `crates/ox-core/src/session/post_compact.rs`: `SUMMARY_PREFIX`, `synthesize_post_compact_message` and its `task_reminder` and `shell_reminder` blocks, plus `strip_post_compact_summary`, which removes the generated summary block when the compaction boundary already displays it.
- `crates/ox-core/src/session/sanitize.rs`: `sanitize_resumed_messages`, which treats the synthetic continuation as an ordinary user message.
- `crates/ox-core/src/session/state.rs`: `compact_entries` and `commit_compact` (chain reset).
- `crates/ox-core/src/session/store.rs`: resume loader compact boundary reset, tail filtering, and `CompactInfo`.
- `crates/ox-tui/src/app/events.rs`: `apply_session_compacted` and `apply_session_ready`.
- `crates/ox-tui/src/components/chat.rs`: `ChatView::append_summary_token` and `ChatView::discard_summary`.
- `crates/ox-tui/src/components/chat/blocks/compacted.rs`: `CompactedBlock`.
- `crates/ox-tui/src/slash/compact.rs`: `CompactCmd`.
- `crates/ox-tui/src/slash/registry.rs`: `BUILT_INS` entry `&CompactCmd`.
- `crates/ox/src/driver/stdio.rs`: `StdioSink`, which prints the compaction line on stderr.
