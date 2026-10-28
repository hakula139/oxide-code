# Modal UI

Focus-grabbing UI overlays opened by slash commands, by the App in response to engine events requesting tool approval, and at startup with `ox --resume`. Lives in the band between the chat scroll and the input area, the same row range the slash autocomplete popup uses, but wider.

Companion docs: [Slash Commands](slash-commands.md) covers the slash-command surface that opens modals.

A modal is a self-contained UI that takes keyboard focus, owns its render, emits a typed result, and dismisses. Chat blocks are persistent transcript artifacts, while modals are ephemeral overlays. Three needs drove the abstraction: `/theme` swaps palettes as the user arrows through choices and snaps back on Esc, the combined `/model + /effort` picker and the confirm-delete modal nested in `/resume` need several steps, and a tool call that needs approval emits `Event::ApprovalRequested` and blocks until the App pushes an `ApprovalModal` and routes the decision back as `Op::Resolve`.

## Trait Shape

A modal is `Send` (App lives on tokio) but not `Sync`, since modals own mutable state and are never shared across threads. It declares its `height` for layout, paints itself into a `Rect`, and routes one `KeyEvent` to one of five outcomes:

- **Consumed**: Stay open, key handled internally.
- **Cancelled**: Close and dispatch the modal's `on_cancel` action, if any.
- **Submitted**: Close and dispatch an `Action` (an op for the engine or a TUI-only change, or a no-op when the modal already applied its effect locally).
- **Emit**: Forward an action without closing. Used for live-preview cursor moves like `/theme`, where each Up / Down repaints the chat in the candidate without committing, and for `/resume`'s Ctrl+X archive toggle and the confirm modal's delete, which commit while the modal stays open.
- **Push**: Nest a child modal above the current one, which returns focus to the parent when it pops.

The App's frame tick also calls `Modal::tick` on the top modal, which reports whether time-driven state such as an expiring notice or a page loaded in the background needs a repaint. `Modal::next_tick` tells the loop when that state next changes, so an idle modal does not keep the loop awake. Both default to no-ops.

`Submitted` and `Emit` carry a `ModalAction`: `Dispatch(Action)` (dispatched like a keyboard action) or `SystemMessage(String)` (push a confirmation line into chat after pop). A close without an action of its own surfaces as `ModalAction::Cancelled`, which reverts any live preview.

Dispatched actions flow through the same channel as keyboard input, so the modal never needs to know what handler will run on the other side.

`Modal::on_engine` tells every open modal about each op the App sends (`EngineUpdate::Admitted`) and each event the engine reports (`EngineUpdate::Event`), so a modal that sent an op learns whether it left and how it ended, while an op the App drops never reaches it. Its `UpdateReply` says whether the modal shows the update, closes on it with a replacement action, or leaves it for the App to report, which the App does only when no modal claims it.

Bracketed pastes reach `Modal::handle_paste`, which defaults to a no-op. Text fields such as `/rename`, the picker search, and the approval prompt's editors accept them, and a paste never answers a confirm or approval prompt.

`Modal::on_cancel` fires whenever a modal leaves the stack without submitting: the stack-level Esc / Ctrl+C cancel, a `Cancelled` outcome, or `ModalStack::clear`. A modal whose `Modal::captures_esc` returns true takes Esc itself, while Ctrl+C still cancels. The approval prompt's rule and feedback editors take Esc to step back to the choice, and the `/resume` picker takes it so dismissal runs through its keymap.

Only a plain Esc cancels. The stack drops every other key held with Ctrl, Alt, or another modifier but Shift unless `Modal::binds_modified_key` claims it, so a picker's `j` or Enter never fires as Alt+J or Ctrl+Enter. A modal with a text field claims every modified key but a line break or an indent through `LineEditor::binds_modified_key`, which covers the `/resume` picker's Ctrl keymap, and that picker moves its cursor only on unmodified arrows. The approval prompt claims every key while its type-ahead guard is armed, so any key restarts the guard.

`on_cancel` returns an optional `ModalAction` that the App dispatches in place of the silent close. The default returns `None`. `ApprovalModal` overrides it to emit an abort decision so the blocked agent always gets an answer. The `ox --resume` picker overrides it to emit `Op::Shutdown` because dismissing the picker quits `ox`.

## Shared Primitives

The modal implementation uses these shared components:

- `ox_tui::modal` defines the trait, key outcome, and `ModalStack` manager.
- Generic primitives sit under `ox_tui::modal`: `list_picker` (cursor and rendering for selection lists), `kv_overview` (sectioned read-only kv table), and `searchable_list` (substring filter for the `/resume` picker).
- `chrome` supplies the shared title header and hint footer. `render_footer` joins hint fragments such as `CONFIRM_HINTS` with `HINT_SEPARATOR` and cuts the tail on a narrow row, so callers list the essential hints first. `line_editor` is the single-line text field behind `/rename`, the picker search query, and the approval rule and feedback editors, which keeps its text in a `ratatui-textarea` buffer and routes keys through the prompt's `grapheme::edit`. That table steps deletions and cursor moves over grapheme clusters and kills to the line start on Ctrl+U, and it leaves the other readline keys to the textarea's emacs bindings, which never insert a letter for a Ctrl chord.
- `ox_tui::keymap` binds named actions to keys and hint labels, for a modal or the app. `Keymap::action_for` dispatches a key and `Keymap::hints` renders the footer from the same entries, so the `/resume` picker's keys and hints cannot drift apart.
- Lists scroll through `ox_tui::viewport`, whose window settles at render time from the height the modal was granted.

App owns `ModalStack` and runs the key gate first inside `handle_crossterm_event`, so an active modal takes every key before any other component sees it, though the stack drops the modified keys the modal does not bind. `apply_modal_action` applies the result. A `ModalAction::Dispatch` follows the same `App::dispatch_action` path as a keyboard action.

## Modals

- **Combined `/model + /effort` picker**: `ModelEffortPicker` wraps `ListPicker<ModelRow>` and tracks the effort axis separately. Left / Right steps the effort row through all five levels, which every supported model accepts, and stops at either end as the `/effort` slider does, so holding a key cannot land on the opposite end. Until the user picks a level, the row shows the active effort, falling back to the highlighted model's default. Submit emits one `Op::SetModel` whose `ConfigPick` carries only the changed axes, and Enter on a no-op cancels.

- **`/effort` slider**: `EffortSlider` is the bare `/effort` horizontal Speed ↔ Intelligence visual. Lists all five levels and seeds the cursor at the active effort. Unknown model IDs have no effort, so bare `/effort` errors on them. Levels render with uniform `●` / `○` glyphs in the five `effort_*` theme slots, graded blue → red. Color encodes identity, bold encodes the active pick, and a theme can restyle the gradient.

- **`/mode` picker**: `ModePicker` wraps `ListPicker<ModeRow>` over `manual`, `plan`, and `yolo`, each with a short description, and seeds the cursor at the active mode. Enter forwards `Op::SetMode` for the highlighted row and cancels on the active one, and numeric `1`–`3` shortcuts jump to a row.

- **`/theme` picker**: `ThemePicker` wraps `ListPicker<ThemeRow>` over the curated built-in roster and emits `Action::PreviewTheme` on every cursor move so the App repaints the chat in the candidate palette without committing. Esc snaps back via the cached `preview_theme_snapshot`, while Enter promotes the preview to an `Action::SwapTheme` swap. Numeric `1`–`9` shortcuts jump to a row to match the visual ladder.

- **`/init` picker**: `InitPicker` wraps `ListPicker<FileRow>` over `AGENTS.md` and `CLAUDE.md`, each described by the other tools that read it. Enter sends the prompt that creates the highlighted file, and numeric `1` / `2` shortcuts jump to a row. [init.md](init.md) covers when it opens.

- **`/rename` editor**: `RenameModal` is a single-line text editor pre-filled with the current title, which takes input up to the 80-column title cap. Render is a fixed five-row stack: title, gap, prompt + buffer, gap, footer hint. Editing runs through the shared `LineEditor`, so the cursor moves within the title, pastes land as one line, and a title wider than the row scrolls to keep the cursor in view. Enter on a non-empty trimmed buffer submits `Op::Rename` through `command::rename`, the typed form's parser, and blank Enter is a silent no-op so the user can keep typing.

- **`/resume` picker**: `ResumePicker` wraps `SearchableList<SessionRow>`, adds a footer line, and pushes `ConfirmDeleteSessionModal` on Ctrl+D. [Resume](resume.md) covers its rows, paging, keys, and footer.

- **Confirm-delete modal**: `ConfirmDeleteSessionModal` shows the session's title above its `format_metadata_line` row, which leads with the ID prefix. Y sends `Op::DeleteSaved` with the session's ID once the target is visible and the shared type-ahead guard expires. Once the app admits the op, `Modal::on_engine` receives it as `EngineUpdate::Admitted`, and the modal ignores another Y until the engine reports back, while a delete the app drops, because another operation runs or the op channel is full, leaves Y working. Enter does nothing, and repeated Y events restart the guard. The `SavedDeleted` result reaches every open modal as an `EngineUpdate::Event`. When no modal claims it, as after the user dismissed this modal mid-delete, the app reports the result in chat. Success closes this modal into a `ModalAction::SystemMessage` naming the deleted session by ID prefix and title, so the chat records the deletion. A failure shows an inline error that stays until a retry replaces it or the modal closes, so a stray key can't wipe the diagnostic. N cancels. Pushed by `/delete <id-prefix>` and by `/resume`'s Ctrl+D.

- **Approval modal**: `ApprovalModal` is the approve-or-deny gate the App pushes when the agent asks to run a gated tool call. It holds the tool-use ID and renders the call summary as its title above the body: the command text for `bash`, or `-` / `+` diff rows for `edit` and `write`. The body shows at most 16 rows and scrolls when the call runs longer or the band is shorter. Y submits `Op::Resolve` with `Approve` while the first body row is visible and the type-ahead guard has expired, while Enter does nothing. [permissions.md](permissions.md) covers the guard. When the call offers a session grant, A opens a rule editor prefilled with the suggested rule, and Enter submits `AllowForSession` with the edited rule while the whole editor is visible and armed. N submits `Deny`. F opens a feedback editor whose Enter submits `DenyWithFeedback` with the typed text. Esc from the choice, Ctrl+C, and clearing the stack all resolve through `on_cancel` to `Abort`, which ends the turn.

- **`/status` overview**: `KvOverview` of session descriptors (model, effort, permission mode, cwd, session ID, auth, version, cache TTL, auto compaction, show-thinking, show-welcome), plus a Session Rules section listing the session allow rules once any is granted and a Background Shells section listing each open shell with its state and the first line of its command. Constructed in `slash/status`.

- **`/config` overview**: Sectioned `KvOverview`. "Resolved" pairs each effective value with a dim note naming its source. "Permissions" appears once the config files set an extra root, a `deny` / `ask` / `allow` pattern, or a turned-off default group, and lists each entry under its dotted key with the layer that added it, because rule lists and extra roots concatenate across layers and any user file can turn a default off. "Source Files" lists the config files loaded at startup, lowest precedence first, each labeled User, Drop-in, or Project. Notes align in one column per section, and a value too wide for that column carries its note right after it. A value too wide for the row wraps after the commas between its items onto lines indented to the value column, and any part still too wide truncates in its middle. Constructed in `slash/config`.

- **`/help` overview**: Single-section `KvOverview` of every registered command. Aliases parenthesize after the canonical name, and the `usage()` placeholder appends. Constructed in `slash/help`.

## Design Decisions

1. **Trait per modal.** Each concrete modal is its own type implementing `Modal`, so adding one is a new file plus a constructor with no central match arm.

2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** The `Vec` lets an overlay stack on an open modal without a redesign, such as the confirm-delete modal over the `/resume` picker or an approval prompt over a read-only overview.

3. **Typed result delivery via `ModalAction`, no callbacks.** Modal emits `ModalKey::Submitted(ModalAction)` and the manager dispatches. Boxed `FnOnce` callbacks were rejected for the lifetime / `Send` complexity and because they hide the dispatch graph.

4. **Commands build modals from read-only App state.** `SlashContext` supplies the current `&LiveSessionInfo`, title included, when a command runs. The App clears every modal on resume and after manual or pre-turn compaction. Mid-turn compaction preserves open modals and their live previews, since the user may open one at any point during a long turn.

5. **Layout band sized by `ModalStack::height(width, max_height)`.** When the stack is empty, the band takes no rows. When active, it shifts the chat upward, just like the slash popup. The App caps the band so the status bar and one chat row stay visible, and sizes it before the queued-prompt preview, which takes the rows left over. Pickers and `KvOverview` fit the cap by scrolling their bodies between a pinned header and footer, and the stack clips any fixed-size modal that still runs taller.

6. **Bare `/model` and bare `/effort` open separate modals.** Threading both bare forms through one modal would force a single-axis decision through a two-axis interface. `/model` keeps the multi-axis combined picker (one modal sets both axes), while `/effort` gets its own single-axis slider. Typed-arg `/model <id>` and `/effort <level>` keep direct-switch behavior for scripting and power users.

7. **`ListPicker` handles state, navigation, and rendering.** Concrete pickers embed it, handle Enter and any keys of their own, and pass the rest to `ListPicker::navigate`, which steps on Up / `k` and Down / `j` and jumps on a row's digit. This keeps list selection apart from what Enter dispatches, with no callbacks. Each `ListPicker` roster is fixed, so `ListPicker::new` rejects an empty one and `selected` always returns a row. `/mode`, `/theme`, and `/init` draw through `ListPicker::render_with_footer`, while `/model + /effort` pins an effort row above its footer.

8. **Read-only kv overviews share `KvOverview`.** `/status`, `/config`, and `/help` all build the same shape of a title, sectioned rows, and a footer, so the layout, key handling, and dismiss live in one place. Per-command files supply only the sections, with their rows and headings, and a thin constructor, so a new overview is a `Vec<KvSection>` away.

9. **Read-only modals don't bind Enter.** `KvOverview::handle_key` scrolls on Up / Down, `j` / `k`, PageUp / PageDown, and Home / End, and consumes every other key. Esc / Ctrl+C cancel universally at the stack layer. Enter stays reserved for commit semantics in `ListPicker`-based modals, since binding it to dismiss in `KvOverview` would give the same gesture two meanings across modal types.

10. **Nested modals use `ModalKey::Push`.** A push outcome keeps `handle_key` returning one value and avoids threading a stack borrow through every modal. `Debug for ModalKey` is hand-rolled because `Box<dyn Modal>` cannot derive it.

11. **Modals learn saved-session results from the engine's events.** The engine owns saved-session writes, so `ModalStack` passes `ArchiveUpdated` and `SavedDeleted` to every modal on the stack. A covered `/resume` picker updates its rows from the same result that closes the confirm-delete modal above it, without rereading the store when it regains focus.

## Out of Scope

- **Persistent modals in chat scroll.** Modals are ephemeral. `/diff` stays a chat-pushed printer because a diff needs scrollback, which a modal would crop.

- **Mouse interaction.** The TUI captures the mouse ([mouse interactions](mouse.md)), but a modal receives only the wheel, as Up / Down keys. It ignores clicks and stays keyboard-driven.

- **Concurrent modals on different layers.** A toast over a modal would need a second layer, while the chat error-block path already covers what toasts would.

- **User-defined modal commands.** The trait already supports them, and user-invocable skills are tracked in [#163](https://github.com/hakula139/oxide-code/issues/163).

- **Imperative `dialog.show()` calls.** A command returns an effect that the App applies, so opening a modal needs no handle outside the stack.

- **A permanent `ChatComposer`.** The input box lives outside the modal stack, so a modal that needs text opens an editor instead.

## Sources

- `crates/ox-tui/src/app/dispatch.rs`: `App::handle_crossterm_event` modal gate, `run_slash`, `apply_slash_outcome`, `apply_modal_action`, `dispatch_action`, `notify_modals`, `clear_modals`.
- `crates/ox-tui/src/app/events.rs`: `push_approval_modal`, `apply_session_resumed`, and `apply_session_compacted`.
- `crates/ox-tui/src/app/render.rs`: `draw_frame` modal layout band.
- `crates/ox-tui/src/keymap.rs`: `Keymap`, `KeyAction`, and `KeyChord`.
- `crates/ox-tui/src/modal.rs`: `Modal` (including the `on_engine`, `on_cancel`, `captures_esc`, `binds_modified_key`, `sends_idle_only_ops`, `tick`, and `next_tick` hooks), `ModalKey`, `ModalAction`, `EngineUpdate`, `UpdateReply`, `ModalStack`.
- `crates/ox-tui/src/modal/acceptance.rs`: shared visibility and type-ahead guard for destructive confirmation and approval.
- `crates/ox-tui/src/modal/approval.rs`: `ApprovalModal` approve-or-deny gate for agent tool calls, plus the rule and feedback editors.
- `crates/ox-tui/src/modal/chrome.rs`: shared `Header`, `render_footer`, and the hint fragments.
- `crates/ox-tui/src/modal/kv_overview.rs`: generic `KvOverview` plus `KvSection`.
- `crates/ox-tui/src/modal/line_editor.rs`: single-line `LineEditor` over `ratatui-textarea`.
- `crates/ox-tui/src/modal/list_picker.rs`: generic `ListPicker<T: PickerItem>`.
- `crates/ox-tui/src/modal/searchable_list.rs`: generic `SearchableList<T: SearchableItem>`.
- `crates/ox-tui/src/slash/config.rs`: `/config` row builder plus sectioned `KvOverview`.
- `crates/ox-tui/src/slash/confirm.rs`: `ConfirmDeleteSessionModal`.
- `crates/ox-tui/src/slash/context.rs`: `SlashContext`, the read-only state each `execute` call receives.
- `crates/ox-tui/src/slash/delete.rs`: `DeleteCmd`, which forwards `Op::FindSaved`, and `confirm_modal`, which builds the confirm-delete modal for the session the engine found.
- `crates/ox-tui/src/slash/effort_slider.rs`: `/effort` slider.
- `crates/ox-tui/src/slash/help.rs`: `/help` row builder plus `KvOverview`.
- `crates/ox-tui/src/slash/init.rs`: `/init` instruction-file picker (`InitPicker`).
- `crates/ox-tui/src/slash/mode.rs`: `/mode` picker (`ModeCmd` plus `ModePicker`).
- `crates/ox-tui/src/slash/picker.rs`: combined `/model + /effort` picker.
- `crates/ox-tui/src/slash/registry.rs`: `SlashOutcome`, including the `Modal` variant.
- `crates/ox-tui/src/slash/rename.rs`: `RenameCmd` plus `RenameModal` editor.
- `crates/ox-tui/src/slash/resume.rs`: `ResumeCmd` plus `ResumePicker`.
- `crates/ox-tui/src/slash/sessions.rs`: the metadata-line formatter for the confirm-delete modal.
- `crates/ox-tui/src/slash/status.rs`: `/status` row builder plus `KvOverview`.
- `crates/ox-tui/src/slash/theme.rs`: `/theme` live-preview picker (`ThemeCmd` plus `ThemePicker`).
- `crates/ox-tui/src/viewport.rs`: `Viewport` scroll window plus `wrap_next` / `wrap_prev` cursor steps.
