# /clear

Resets the conversation to a clean state without dropping the prior session: rolls the session UUID, finalizes the old JSONL (still resumable via `ox -c`), drops the in-memory message log, clears the file tracker, and clears the AI-generated title. Aliases `/new`, `/reset` route to the same impl. The action is reversible — the cleared session stays on disk under `$XDG_DATA_HOME/ox/sessions/{project}/` — so there is no confirmation prompt.

The cross-command surface (registry, parser, popup, dispatch) lives in [Slash Commands](README.md); this doc covers `/clear` only.

## Claude Code Reference

Claude Code's `clearConversation` (`commands/clear/conversation.ts`) runs ~15 distinct steps. The table maps each step to oxide-code's surface so the boundary between adopted behavior and out-of-scope steps is explicit. Codex's `/clear` is a screen-clear (`AppEvent::ClearUi`) and opencode has no `/clear` analog, so neither informs the design.

| Claude Code step                                    | oxide-code surface                                         | Notes                                                                           |
| --------------------------------------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `executeSessionEndHooks('clear', ...)`              | n/a                                                        | Hook system out of scope.                                                       |
| `tengu_cache_eviction_hint` analytics               | n/a                                                        | Prompt-cache eviction signal; not user-visible.                                 |
| Preserve background tasks                           | n/a                                                        | Background-task infra (`Ctrl+B`) out of scope.                                  |
| `setMessages(() => [])`                             | `messages.clear()` in `agent_loop_task`                    | In-memory message history.                                                      |
| `setConversationId(randomUUID())`                   | `AgentEvent::SessionRolled { id }`                         | oxide-code conflates session and conversation id; Claude Code keeps them split. |
| `clearSessionCaches(preservedAgentIds)`             | partial — `FileTracker::clear`                             | Claude Code also wipes per-agent skill / perm / cache-break caches.             |
| `setCwd(getOriginalCwd())`                          | n/a                                                        | Mid-session cwd changes unsupported.                                            |
| `readFileState.clear()`                             | `FileTracker::clear`                                       | Read-before-Edit gate state.                                                    |
| `discoveredSkillNames?.clear()`                     | n/a                                                        | Skills out of scope.                                                            |
| `loadedNestedMemoryPaths?.clear()`                  | n/a                                                        | Nested CLAUDE.md walk caching out of scope.                                     |
| `fileHistory` reset                                 | n/a                                                        | Undo / snapshot history out of scope.                                           |
| `clearAllPlanSlugs()`                               | n/a                                                        | Plan-mode out of scope.                                                         |
| `clearSessionMetadata()` (title / agent)            | `status_bar.set_title(None)` in App handler                | Only the AI title is in scope.                                                  |
| `regenerateSessionId({ setCurrentAsParent: true })` | `session::start(&store, model)` + `Client::set_session_id` | Parent linkage deferred (see [Deferred](#deferred)).                            |
| `resetSessionFilePointer()`                         | implicit via `session::start`                              | New JSONL file lazily materializes on first record.                             |
| `processSessionStartHooks('clear')`                 | n/a                                                        | Hooks out of scope.                                                             |

## oxide-code Implementation

`ClearCmd::execute` (`crates/oxide-code/src/slash/clear.rs`) forwards `UserAction::Clear` to the agent loop and clears the `ChatView`. The agent's Clear arm calls `session::handle::roll`, which snapshots the file tracker, clears it, swaps `SessionHandle` in place for a fresh one, and finalizes the old handle (writing the summary line and shutting down the actor). `RollOutcome { new_id, finalize_failure }` carries the new id back to the agent loop, which updates `Client::set_session_id`, drops the in-memory `messages`, and emits `AgentEvent::SessionRolled { id }`. The TUI rebinds `session_info.session_id` and clears any AI-generated title.

The two visible side effects: the session id surface (`/status`-visible id, `x-claude-code-session-id` header, `metadata.user_id` field) jumps to the new UUID, and the chat view shows a `SystemMessageBlock` confirming `Conversation cleared. Next message starts fresh.`

## Design Decisions for oxide-code

1. **Roll the session UUID; don't truncate the old JSONL.** Finalize-and-fork keeps the cleared conversation reachable via `ox -c <old-id>`. Truncating in place would match Claude Code's `setMessages(() => [])` semantics on the wire but lose the cleared transcript on disk. Disk space is cheap; an accidental clear is not.
2. **Send-first ordering in `ClearCmd::execute`.** Forward `UserAction::Clear` to `user_tx` first; only on success drop the chat history and push the confirmation. If the channel can't accept the action (agent task gone), the visible chat stays intact and the user sees a clean error block — never an emptied pane staring back at them.
3. **`AgentEvent::SessionTitleUpdated` carries the originating session id.** A slow Haiku title call straddling `/clear` would otherwise paint the old session's title onto the fresh one. The TUI ignores titles for sessions other than `session_info.session_id`. Cheaper than tracking the title task's `JoinHandle` and aborting on roll.
4. **`AgentEvent::SessionRolled { id }` rebinds the App's session id.** The `/status`-visible id and the now-stale title both update on roll. Without this rebind, the title-id gate (decision 3) would treat every fresh-session title as stale.
5. **`SessionHandle::roll` is the testable extraction point.** Snapshot the file tracker → clear it → swap the handle → finalize the old. Two orderings matter: snapshot **before** clear (so the snapshots ride the old JSONL via `finalize`); replace **before** finalize (so the new handle is in place when the old handle is consumed). Returns `RollOutcome { new_id, finalize_failure }` so the caller routes the failure (warn-log on TUI exit, sink-error on `/clear` roll).
6. **`/clear` overrides `is_read_only` to `false`.** State-mutating commands refuse mid-turn rather than racing the in-flight `messages` and session writer. The dispatcher's busy-branch refusal pushes a `SystemMessageBlock` with the wording `/clear runs only when idle. Try again after the turn finishes.` The cross-command read-only fast-path is decision 11 in [Slash Commands](README.md#design-decisions-for-oxide-code).
7. **No confirmation prompt.** Matches Claude Code. The cleared session is resumable via `ox -c <old-id>`, so a confirmation dialog would be friction without payoff.

## Deferred

Behaviors Claude Code's `/clear` ships that oxide-code skips today, with the subsystem each gates on:

1. **Parent-session linkage** (`regenerateSessionId({ setCurrentAsParent: true })`). Claude Code records the old session as parent so `--resume --parent-of=<id>` queries can walk the chain. oxide-code mints an unlinked UUID. Adding `parent_session_id: Option<String>` to the session header and rendering the chain in `--list` lands once `/clear` chains become a real review surface.
2. **Cache-eviction hint** (`tengu_cache_eviction_hint`). Claude Code logs a structured analytics event so the prompt-cache backend evicts the cleared conversation. oxide-code has no analytics surface; deferred until one exists.
3. **Background-task preserve set.** Claude Code keeps `isBackgrounded === true` tasks (and their per-agent caches) across `/clear` while killing foreground tasks. oxide-code has no background-task infrastructure (`Ctrl+B`); the preserve-set predicate lands with that subsystem.
4. **MCP client / tool / resource reset.** Claude Code wipes `mcp.clients / tools / commands / resources` (preserving only `pluginReconnectKey`) so the MCP layer reconnects against the fresh session id. Lands with the MCP client.

## Sources

- `crates/oxide-code/src/slash/clear.rs` — `ClearCmd::execute`, send-first ordering, alias declaration, `is_read_only` override.
- `crates/oxide-code/src/agent/event.rs` — `UserAction::Clear`, `AgentEvent::SessionRolled { id }`, `AgentEvent::SessionTitleUpdated { session_id, title }`.
- `crates/oxide-code/src/agent.rs` — `agent_loop_task` Clear arm: calls `roll`, updates `Client::set_session_id`, drops `messages`, emits `SessionRolled`.
- `crates/oxide-code/src/session/handle.rs` — `roll`, `RollOutcome`, the snapshot-before-clear / replace-before-finalize ordering invariants.
- `crates/oxide-code/src/session/title_generator.rs` — title task stamps the originating `session_id` on the emitted event.
- `crates/oxide-code/src/client/anthropic.rs` — `set_session_id` (header + `metadata.user_id` rebind), debug-asserts header-value validity.
- `crates/oxide-code/src/tui/app.rs` — `handle_agent_event` arms for `SessionTitleUpdated` (gates on current id) and `SessionRolled` (rebinds id, clears title).
- `claude-code/src/commands/clear/conversation.ts` — reference flow.
