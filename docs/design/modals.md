# Modal UI

A modal is an ephemeral overlay drawn in the band between chat and input (the slash-popup region) that takes keyboard focus, renders itself, emits a typed action, and dismisses. Slash commands, tool approvals, mid-turn `ask` questions, and bare `ox --resume` open modals, supporting live `/theme` preview with Esc revert, multi-step stacks (`/model + /effort`, confirm-delete over `/resume`), and blocking `Event::ApprovalRequested` → `Op::Resolve` round-trips.

## Trait Shape

[`Modal` and its result types](../../crates/ox-tui/src/modal.rs) define rendering, input, and typed actions. Modals are `Send` (for tokio) and `!Sync` (owning unshared mutable state). `App` dispatches returned `ModalAction`s, and pushing a child leaves its parent on the stack to regain focus when the child closes.

## Hooks

- **`on_cancel`.** Runs whenever a modal leaves without submitting (stack Esc / Ctrl+C, `Cancelled`, or `ModalStack::clear`) and may return a `ModalAction`: `ApprovalModal` and `QuestionModal` emit abort so blocked turns always unblock, startup `ox --resume` emits `Op::Shutdown`, and `/theme` emits `Action::RevertThemePreview`.
- **`on_engine`.** Delivers every `dispatch_op`-admitted op (`EngineUpdate::Admitted`) and engine event (`EngineUpdate::Event`) to all open modals (prompts use the prompt queue and skip modals). `UpdateReply` indicates whether the modal displays the update, closes with a replacement action, or leaves it for `App` (which suppresses duplicate chat notices for handled archive / delete results and runs actions from modals closed by an admitted op after sending that op).
- **`on_page`.** Delivers `ModalPage` background worker results from top to bottom via `ModalStack::deliver_page` so a covered picker still loads under a confirm dialog. Shared `Pages` workers keep only the latest query / scope change while a page is in flight and discard completions for closed pickers via unique request IDs.
- **`tick` and `next_tick`.** Ticks the top modal and reports its next deadline (such as notice expiry) so idle modals keep the event loop asleep.

## Key Routing

The stack intercepts keys before any component. Modals returning true from `captures_esc` handle Esc themselves while Ctrl+C still cancels. Keys with modifiers other than Shift drop unless `binds_modified_key` claims them (preventing Alt+J or Ctrl+Enter from acting like `j` or Enter). Bracketed pastes route to `handle_paste` in text fields and never answer confirm or approval prompts.

## Shared Primitives

- `list_picker`: fixed-roster cursor, navigation, and rendering (`/mode`, `/theme`, `/init`, `/model + /effort`).
- `searchable_list`: substring-filtered paged rows (session and prompt history pickers).
- `pages`: typed bounded page workers and request routing for both pickers.
- `kv_overview`: sectioned read-only tables (`/status`, `/config`, `/help`).
- `line_editor`: single-line `ratatui-textarea` field using the prompt's grapheme editing table (`/rename`, search, approval rule / feedback, question answers).
- `chrome`: header and hint footer dropping trailing hints on narrow rows (callers put essential hints first).
- `acceptance`: visibility and type-ahead guard shared by confirm-delete, approval, and question modals.
- `ox_tui::keymap`: named actions uniting keys and footer hints (`/resume`).

## Modals

- **`/model + /effort` picker.** Submits one `Op::SetModel` whose `ConfigPick` carries only changed axes.
- **`/effort` slider.** Five-level Speed ↔ Intelligence scale colored by `effort_*` theme slots.
- **`/mode`, `/theme`, and `/init` pickers.** Fixed rosters for permission modes, built-in themes, and `/init` targets.
- **`/rename` editor.** Prefilled `LineEditor` submitting through `command::rename`.
- **`/resume` picker.** Paged `SearchableList` of saved sessions ([Resume](resume.md)).
- **Prompt history picker.** Retained ordinary user prompts with escaped scrollable multiline preview. Enter recalls exact text as an undoable composer edit, and cancel leaves the draft intact.
- **Confirm-delete modal.** Y / N gate pushed by `/delete <id-prefix>` and `/resume`. Errors stay inline until retry or close so stray keys cannot clear diagnostics.
- **Approval modal.** Shows command or diff preview for gated tool calls ([Permissions](permissions.md)).
- **Question modal.** Handles mid-turn `ask` prompts with choices, freeform `LineEditor`, optional deadline hints, and `Acceptance` guard ([Terminal UI](../guide/terminal-ui.md#answer-questions)).
- **`/status`, `/config`, and `/help` overviews.** `KvOverview` tables for session state, layered config with sources, and commands.

## Design Decisions

1. **Trait per modal.** Adding a modal is one file and constructor with no central match arm.
2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** Supports nested overlays (confirm-delete over `/resume`, approval over an overview) without redesign.
3. **Typed result delivery via `ModalAction`.** Returning outcomes from `ModalStack::handle_key` to `App::apply_modal_action` avoids boxed `FnOnce` lifetime / `Send` boilerplate and keeps dispatch explicit.
4. **Commands build modals from read-only App state.** Commands borrow `LiveSessionInfo` and return a modal outcome without holding a stack handle. `App` clears modals on resume and manual or pre-turn compaction, keeping them open across mid-turn compaction.
5. **Layout band sized by `ModalStack::height(width, max_height)`.** Empty stacks take zero rows, and open ones shift chat up while keeping the status bar and at least one chat row visible (scrolling bodies between pinned header and footer).
6. **Bare `/model` and bare `/effort` open separate modals.** Avoids forcing a single-axis effort choice through the two-axis model picker.
7. **`ListPicker` handles state, navigation, and rendering.** Separates navigation from Enter dispatch, and rejecting empty rosters ensures `selected` always returns a row.
8. **Read-only overviews share `KvOverview`.** Centralizes layout, scrolling, and dismissal so overviews supply only sections.
9. **Read-only modals don't bind Enter.** Keeps Enter exclusively for committing in pickers while stack Esc / Ctrl+C dismisses overviews.
10. **Nested modals use `ModalKey::Push`.** Keeps `handle_key` single-return without threading a mutable stack reference.
11. **Modals learn the fate of their ops through one hook.** Broadcasting `EngineUpdate` to every modal lets each match its own session ID without App-side filtering: a covered `/resume` picker updates rows from the delete result closing the confirm modal above it without re-querying SQLite, unclaimed results fall back to chat, and `Admitted` fires only after `dispatch_op`'s busy check and delivery reservation so dropped deletes remain retryable.
12. **The input box stays outside the stack.** Giving text modals their own `LineEditor` keeps composer drafts untouched across modal lifecycles.

## Out of Scope

- **Persistent modals in the chat scroll.** Output needing scrollback (such as `/diff`) writes to chat.
- **Mouse interaction.** Modals receive wheel events as Up / Down ([Mouse Interactions](mouse.md)) and stay keyboard-driven.
- **Concurrent modals on different layers.** Chat error blocks cover toast use cases without a second overlay layer.
- **User-defined modal commands.** Supported by the trait, with user-invocable skills tracked in [#163](https://github.com/hakulab/oxide-code/issues/163).

## Sources

- `crates/ox-tui/src/app/dispatch.rs`: the modal key gate in `handle_crossterm_event`, `apply_modal_action`, `notify_modals`, and `clear_modals`.
- `crates/ox-tui/src/keymap.rs`: `Keymap`, `KeyAction`, and `KeyChord`.
- `crates/ox-tui/src/modal.rs`: `Modal` and its hooks, `ModalKey`, `ModalAction`, `EngineUpdate`, `UpdateReply`, and `ModalStack`.
- `crates/ox-tui/src/modal/acceptance.rs`: the visibility check and type-ahead guard.
- `crates/ox-tui/src/modal/approval.rs`: `ApprovalModal`.
- `crates/ox-tui/src/modal/chrome.rs`: `Header`, `render_footer`, and the hint fragments.
- `crates/ox-tui/src/modal/kv_overview.rs`: `KvOverview` and `KvSection`.
- `crates/ox-tui/src/modal/line_editor.rs`: `LineEditor`.
- `crates/ox-tui/src/modal/list_picker.rs`: `ListPicker<T: PickerItem>`.
- `crates/ox-tui/src/modal/pages.rs`: `ModalPage`, `PageSource`, `Pages`, and `PageEnd`, the typed page workers shared by session and prompt pickers.
- `crates/ox-tui/src/modal/question.rs`: `QuestionModal`.
- `crates/ox-tui/src/modal/searchable_list.rs`: `SearchableList<T: SearchableItem>`.
