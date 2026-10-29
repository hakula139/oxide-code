# Modal UI

A modal is an overlay that takes keyboard focus, owns its render, emits a typed result, and dismisses, opened by a slash command, by a tool call that needs approval, or at startup by `ox --resume`. It draws in the band between the chat scroll and the input area, the rows the slash popup uses. Chat blocks are persistent transcript artifacts, while modals are ephemeral.

Three needs drove the abstraction: `/theme` swaps palettes as the user arrows through choices and snaps back on Esc, the combined `/model + /effort` picker and the confirm-delete modal nested in `/resume` need several steps, and a tool call that needs approval emits `Event::ApprovalRequested` and blocks until the App pushes an `ApprovalModal` and routes the decision back as `Op::Resolve`.

## Trait Shape

A modal is `Send`, since the App lives on tokio, but not `Sync`, since modals own mutable state and are never shared across threads. It declares its `height` for layout, paints itself into a `Rect`, and routes one `KeyEvent` to one of five outcomes:

- **Consumed**: Stay open, key handled internally.
- **Cancelled**: Close and dispatch the modal's `on_cancel` action, if any.
- **Submitted**: Close and dispatch an `Action`, which is an op for the engine, a TUI-only change, or a no-op when the modal already applied its effect locally.
- **Emit**: Forward an action without closing, as `/theme`'s live preview does on each cursor move, and as `/resume`'s archive toggle and the confirm modal's delete do while the modal stays open.
- **Push**: Nest a child modal above the current one, which returns focus to the parent when it pops.

`Submitted` and `Emit` carry a `ModalAction`: `Dispatch(Action)`, which follows the same `App::dispatch_action` path as a keyboard action, or `SystemMessage(String)`, which prints a confirmation into the chat after the pop.

`Modal::on_cancel` fires whenever a modal leaves the stack without submitting, whether by the stack-level Esc / Ctrl+C, a `Cancelled` outcome, or `ModalStack::clear`. It returns an optional `ModalAction` that the App dispatches as the modal leaves, and a modal without one closes silently. `ApprovalModal` emits an abort decision so the blocked turn always gets an answer, the `ox --resume` picker emits `Op::Shutdown` because dismissing it quits `ox`, and the `/theme` picker emits `Action::RevertThemePreview` to restore the theme it previewed over.

`Modal::on_engine` tells every open modal about each op `dispatch_op` admits (`EngineUpdate::Admitted`) and each event the engine reports (`EngineUpdate::Event`), so a modal that sent an op learns whether it left and how it ended. Prompts go through the prompt queue and never reach modals. Its `UpdateReply` says whether the modal shows the update, closes on it with a replacement action, or leaves it for the App to report. The App skips its own report of an archive or delete result that a modal shows, and applies the actions of the modals an admitted op closes only after it sends that op.

The stack routes keys before any component sees them. A modal whose `captures_esc` returns true takes Esc itself, while Ctrl+C still cancels. The stack drops every key held with a modifier other than Shift unless `binds_modified_key` claims it, so a picker's `j` or Enter never fires as Alt+J or Ctrl+Enter. Bracketed pastes reach `handle_paste`, which text fields implement, and a paste never answers a confirm or approval prompt. The frame tick calls `tick` on the top modal, and `next_tick` tells the loop when its time-driven state, such as an expiring notice, next changes, so an idle modal does not keep the loop awake.

A page of sessions that a `/resume` picker's worker read reaches the stack as an app event, and `Modal::on_sessions_page` offers it to each open modal from the top until the picker whose request it answers takes it, so a picker keeps loading under a confirm dialog.

## Shared Primitives

- `list_picker`: cursor, navigation, and rendering for a fixed roster, which `/mode`, `/theme`, `/init`, and the `/model + /effort` picker embed.
- `searchable_list`: a substring filter over paged rows, behind the `/resume` picker.
- `kv_overview`: a sectioned read-only table, behind `/status`, `/config`, and `/help`.
- `line_editor`: the single-line text field behind `/rename`, the picker search, and the approval rule and feedback editors. It keeps its text in a `ratatui-textarea` buffer and edits through the prompt's grapheme-aware table.
- `chrome`: the title header and hint footer, which cuts trailing hints on a narrow row, so callers list the essential hints first.
- `acceptance`: the visibility check and type-ahead guard that the confirm-delete and approval modals share, so a key typed before the prompt appeared cannot answer it.
- `ox_tui::keymap`: named actions with their keys and hint labels, so the `/resume` picker's keys and footer come from one table.

## Modals

- **`/model + /effort` picker**: sets the model and the effort in one `Op::SetModel` whose `ConfigPick` carries only the changed axes.
- **`/effort` slider**: a Speed ↔ Intelligence scale of the five levels, colored by the five `effort_*` theme slots.
- **`/mode`, `/theme`, and `/init` pickers**: list pickers over the permission modes, the built-in themes, and the instruction files `/init` can create.
- **`/rename` editor**: a `LineEditor` prefilled with the title, which submits through `command::rename`, the typed form's parser.
- **`/resume` picker**: a `SearchableList` of saved sessions, which [Resume](resume.md) covers.
- **Confirm-delete modal**: a Y / N gate for deleting a saved session, pushed by `/delete <id-prefix>` and by the `/resume` picker. A failure stays inline until a retry replaces it or the modal closes, so a stray key cannot wipe the diagnostic.
- **Approval modal**: the gate for a tool call that needs approval, showing the command or the diff it would apply. [Permissions](permissions.md) covers its choices and guard.
- **`/status`, `/config`, and `/help` overviews**: `KvOverview` tables of the session, the resolved config with each value's source, and the registered commands.

## Design Decisions

1. **Trait per modal.** Each concrete modal is its own type implementing `Modal`, so adding one is a new file plus a constructor with no central match arm.
2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** The `Vec` lets an overlay stack on an open modal without a redesign, such as the confirm-delete modal over the `/resume` picker or an approval prompt over a read-only overview.
3. **Typed result delivery via `ModalAction`.** `ModalStack::handle_key` returns the outcome a modal reports, and `App::apply_modal_action` dispatches the action it carries. Boxed `FnOnce` callbacks would add lifetime and `Send` complexity and hide the dispatch graph.
4. **Commands build modals from read-only App state.** Each command runs on a borrow of the current `LiveSessionInfo`, title included, and returns the modal as its outcome, where imperative `dialog.show()` calls would need a handle to the stack outside it. The App clears every modal on resume and after manual or pre-turn compaction, while a mid-turn compaction keeps open modals and their previews, since the user may open one at any point during a long turn.
5. **Layout band sized by `ModalStack::height(width, max_height)`.** An empty stack takes no rows, and an open one shifts the chat upward, as the slash popup does. The App caps the band so the status bar and one chat row stay visible, and pickers and overviews fit the cap by scrolling their bodies between a pinned header and footer.
6. **Bare `/model` and bare `/effort` open separate modals.** Threading both through one modal would force a single-axis decision through a two-axis interface, so `/model` keeps the combined picker and `/effort` gets its own slider.
7. **`ListPicker` handles state, navigation, and rendering.** Concrete pickers handle Enter and their own keys and pass the rest to `ListPicker::navigate`, which keeps list selection apart from what Enter dispatches. Each roster is fixed, so `ListPicker::new` rejects an empty one and `selected` always returns a row.
8. **Read-only overviews share `KvOverview`.** `/status`, `/config`, and `/help` share one shape of a title, sectioned rows, and a footer, so the layout, keys, and dismissal live in one place, and a new overview supplies only its sections.
9. **Read-only modals don't bind Enter.** Enter commits in a picker, so binding it to dismiss an overview would give one gesture two meanings across modal types. Esc / Ctrl+C dismiss an overview at the stack layer.
10. **Nested modals use `ModalKey::Push`.** A push outcome keeps `handle_key` returning one value and avoids threading a stack borrow through every modal.
11. **Modals learn the fate of their ops through one hook.** The engine owns saved-session writes, so `ModalStack` passes every admitted op and every engine event to every modal on the stack through `Modal::on_engine`, and a new modal that sends an op needs only a match arm in that hook. The App does not filter updates per modal, because each modal matches the session ID it acted on and ignores the rest. A covered `/resume` picker updates its rows from the same result that closes the confirm-delete modal above it, without rereading the store when it regains focus. When no modal claims a result, as after the user dismissed the confirm-delete modal mid-delete, the App reports it in the chat. `Admitted` follows `dispatch_op`'s busy check and delivery reservation, so an op the App drops never reaches a modal, and a confirm modal whose delete was dropped still takes Y again.
12. **The input box stays outside the stack.** A modal that needs text opens its own `LineEditor`, so a modal never shares the prompt's buffer and closing one never disturbs a draft.

## Out of Scope

- **Persistent modals in the chat scroll.** Modals are ephemeral, and output that needs scrollback, such as `/diff`, prints into the chat.
- **Mouse interaction.** The TUI captures the mouse ([Mouse Interactions](mouse.md)), but a modal receives only the wheel, as Up / Down keys, and stays keyboard-driven.
- **Concurrent modals on different layers.** A toast over a modal would need a second layer, while the chat's error blocks already cover what toasts would.
- **User-defined modal commands.** The trait already supports them, and user-invocable skills are tracked in [#163](https://github.com/hakula139/oxide-code/issues/163).

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
- `crates/ox-tui/src/modal/searchable_list.rs`: `SearchableList<T: SearchableItem>`.
