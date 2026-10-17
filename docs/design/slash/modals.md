# Modal UI

Focus-grabbing UI overlays opened by slash commands, by the App in response to engine events requesting tool approval, and at startup with `ox --resume`. Lives in the band between the chat scroll and the input area, the same row range the slash autocomplete popup uses, but wider.

Companion: [commands.md](commands.md) covers the slash-command surface that opens modals.

## Goals

A modal is a self-contained UI that takes keyboard focus, owns its render, emits a typed result, and dismisses. Chat blocks are persistent transcript artifacts, while modals are ephemeral overlays.

Three things drove the abstraction:

1. **Live preview.** `/theme` swaps palettes as the user arrows through choices and snaps back on Esc. This was the original motivating case for the trait shape.
2. **Multi-step interaction.** The combined `/model + /effort` picker and the confirm-delete modal nested in `/resume`.
3. **Agent-driven prompts.** When a tool call needs approval, the agent emits `Event::ApprovalRequested` and blocks until the user's decision comes back. The App pushes an `ApprovalModal`, which routes the decision back as `Op::Resolve`.

## Trait Shape

A modal is `Send` (App lives on tokio) but not `Sync`, since modals own mutable state and are never shared across threads. It declares its `height` for layout, paints itself into a `Rect`, and routes one `KeyEvent` to one of five outcomes:

- **Consumed**: Stay open, key handled internally.
- **Cancelled**: Close and dispatch the modal's `on_cancel` action, if any.
- **Submitted**: Close and dispatch an `Action` (an op for the engine or a TUI-only change, or a no-op when the modal already applied its effect locally).
- **Preview**: Forward an action without closing. Used for live-preview cursor moves like `/theme`, where each Up / Down repaints the chat in the candidate without committing.
- **Push**: Nest a child modal above the current one. The parent regains focus on pop via `Modal::on_focus_regained` (default no-op).

`Submitted` and `Preview` carry a `ModalAction`: `Dispatch(Action)` (dispatched like a keyboard action) or `SystemMessage(String)` (push a confirmation line into chat after pop). A close without an action of its own surfaces as `ModalAction::Cancelled`, which reverts any live preview.

Dispatched actions flow through the same channel as keyboard input, so the modal never needs to know what handler will run on the other side.

Bracketed pastes reach `Modal::handle_paste`, which defaults to a no-op. Text fields such as `/rename`, the picker search, and the approval prompt's editors accept them, and a paste never answers a confirm or approval prompt.

`Modal::on_cancel` fires whenever a modal leaves the stack without submitting: the stack-level Esc / Ctrl+C cancel, a `Cancelled` outcome, or `ModalStack::clear` on resume or compaction. A modal whose `Modal::captures_esc` returns true takes Esc itself, while Ctrl+C still cancels. The approval prompt's rule and feedback editors take it to step back to the choice, and the `/resume` picker takes it so dismissal runs through its keymap. `on_cancel` returns an optional `ModalAction` that the App dispatches in place of the silent close. The default returns `None`. `ApprovalModal` overrides it to emit an abort decision so the blocked agent always gets an answer. The `ox --resume` picker overrides it to emit `Op::Shutdown` because dismissing the picker quits `ox`.

## Implementation

The modal implementation uses these shared components:

- `tui/modal` defines the trait, key outcome, and `ModalStack` manager.
- Generic primitives sit alongside `tui/modal`: `list_picker` (cursor + render for selection lists), `kv_overview` (sectioned read-only kv table), and `searchable_list` (substring filter for the `/resume` picker).
- `chrome` supplies the shared title header and hint footer. `render_footer` joins hint fragments such as `CONFIRM_HINTS` with `HINT_SEPARATOR` and cuts the tail on a narrow row, so callers list the essential hints first. `line_editor` is the single-line text field behind `/rename`, the picker search query, and the approval rule and feedback editors, built on `ratatui-textarea` so readline keys work and Ctrl chords never insert letters.
- `keymap` binds a modal's named actions to keys and footer labels. `Keymap::action_for` dispatches a key and `Keymap::hints` renders the footer from the same entries, so the `/resume` picker's keys and hints cannot drift apart.
- Lists scroll through `tui::viewport`, whose window settles at render time from the height the modal was granted.

Concrete modals embed these primitives.

Modals shipping today:

- **Combined `/model + /effort` picker** (over `ListPicker`).
- **`/effort` slider**: Bare `/effort` Speed ↔ Intelligence slider.
- **`/theme` picker**: Live-preview palette picker (over `ListPicker`).
- **`/init` picker**: Instruction-file picker for a project with no `AGENTS.md` or `CLAUDE.md` (over `ListPicker`).
- **`/rename` editor**: Single-line title editor pre-filled with the current title.
- **`/resume` picker**: Searchable session picker (over `SearchableList`).
- **Confirm-delete modal**: Y/N gate pushed by `/delete <id-prefix>` and `/resume`'s Ctrl+D.
- **Approval modal**: Approve-or-deny gate the App pushes when the agent asks to run a gated tool call.
- **`/status`, `/config`, `/help`**: Read-only overviews assembled from `KvOverview` + `KvSection` fixtures inside the per-command files.

App owns `ModalStack` and runs the key gate first inside `handle_crossterm_event`, so an active modal sees every key before any other component. `apply_modal_action` then dispatches the result through the same `App::dispatch_action` path as a keyboard action.

## Design Decisions

1. **Trait per modal.** Each concrete modal is its own type implementing `Modal`, so adding one is a new file plus a constructor with no central match arm.

2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** The `Vec` lets an overlay stack on an open modal without a redesign, such as the confirm-delete modal over the `/resume` picker or an approval prompt over a read-only overview.

3. **Typed result delivery via `ModalAction`, no callbacks.** Modal emits `ModalKey::Submitted(ModalAction)` and the manager dispatches. Boxed `FnOnce` callbacks were rejected for the lifetime / `Send` complexity and because they hide the dispatch graph.

4. **Modals receive a `&LiveSessionInfo` snapshot at open.** Modals do not subscribe to updates, so when a value changes mid-modal (rare) the modal closes and reopens with fresh state.

5. **Layout band sized by `ModalStack::height(width, max_height)`.** When the stack is empty, the band takes no rows. When active, it shifts the chat upward, just like the slash popup. The App caps the band so the status bar and one chat row stay visible, and sizes it before the queued-prompt preview, which takes the rows left over. Pickers and `KvOverview` fit the cap by scrolling their bodies between a pinned header and footer, and the stack clips any fixed-size modal that still runs taller.

6. **Commands return modals as `SlashOutcome::Modal`.** `execute` reads a borrowed `SlashContext` and hands every effect back as a value, so the App pushes the modal in the same place it prints chat output, and commands never touch the modal stack or `ChatView`.

7. **Bare `/model` and bare `/effort` open separate modals.** Threading both bare forms through one modal would force a single-axis decision through a two-axis interface. `/model` keeps the multi-axis combined picker (model and effort cycle together), while `/effort` gets its own single-axis slider. Typed-arg `/model <id>` and `/effort <level>` keep direct-switch behaviour for scripting and power users.

8. **`ListPicker` is a state + render primitive.** Concrete pickers embed it and forward keys to it. This separates "list selection state" from "what does Enter dispatch", avoiding the boxed-callback pattern while staying broadly reusable. `/model + /effort`, `/theme`, and `/init` build on it.

9. **Read-only kv overviews share `KvOverview`.** `/status`, `/config`, and `/help` all build the same title + sectioned-rows + footer shape, so the layout, key handling, and dismiss live in one place. Per-command files own only the fixture (rows, headings) and a thin constructor, and new overviews are a `Vec<KvSection>` away.

10. **Read-only modals don't bind Enter.** `KvOverview::handle_key` scrolls on Up / Down, `j` / `k`, PageUp / PageDown, and Home / End, and consumes every other key. Esc / Ctrl+C cancel universally at the stack layer. Enter stays reserved for commit semantics in `ListPicker`-based modals, since binding it to dismiss in `KvOverview` would give the same gesture two meanings across modal types.

11. **Nested modals use `ModalKey::Push`.** A push outcome keeps `handle_key` returning one value and avoids threading a stack borrow through every modal. `Debug for ModalKey` is hand-rolled because `Box<dyn Modal>` cannot derive it.

12. **`Modal::on_focus_regained` is a generally-useful hook.** It defaults to a no-op, but any picker that opens a sub-modal mutating shared state can override it to refresh on pop.

## Per-Modal Notes

- **Combined `/model + /effort` picker**: `ModelEffortPicker` wraps `ListPicker<ModelRow>` and tracks the effort axis separately. Left / Right cycles the effort row through all five tiers, which every supported model accepts. Until the user picks a tier, the row shows the active effort, falling back to the highlighted model's default. Submit emits one `Op::SetModel { model, effort }` with `Option` axes (only changed axes populated). Enter on a no-op cancels.

- **`/effort` slider**: `EffortSlider` is a horizontal Speed ↔ Intelligence visual. Lists all five tiers and seeds the cursor at the active effort. Unknown model ids have no effort, so bare `/effort` errors on them. Tiers render with uniform `●` / `○` glyphs plus per-tier ANSI color along blue → red. Color encodes identity, bold encodes the active pick. ANSI-named colors decouple the gradient from theme TOML so the user's terminal palette supplies the actual rendering.

- **`/theme` picker**: `ThemePicker` wraps `ListPicker<ThemeRow>` over the curated built-in roster and emits `Action::PreviewTheme` on every cursor move so the App repaints the chat in the candidate palette without committing. Esc snaps back via the cached `preview_theme_snapshot`, while Enter promotes the preview to an `Action::SwapTheme` swap. Numeric `1`–`9` shortcuts jump to a row to match the visual ladder.

- **`/init` picker**: `InitPicker` wraps `ListPicker<FileRow>` over `AGENTS.md` and `CLAUDE.md`, each described by the other tools that read it. Enter forwards the prompt that creates the highlighted file, and numeric `1` / `2` shortcuts jump to a row. [init.md](init.md) covers when it opens.

- **`/rename` editor**: `RenameModal` is a single-line text editor pre-filled with the current title (cap 80 chars, mirroring the actor's first-prompt cap). Render is a fixed five-row stack: title, gap, prompt + buffer, gap, footer hint. Editing runs through the shared `LineEditor`, so the cursor moves within the title, pastes land as one line, and a title wider than the row scrolls to keep the cursor in view. Enter on a non-empty trimmed buffer submits `Op::Rename`, and blank Enter is a silent no-op so the user can keep typing.

- **`/resume` picker**: `ResumePicker` wraps `SearchableList<SessionRow>` and adds a footer line. Each row paints a two-line title plus dim metadata block (id prefix · relative time · message count · branch · project) followed by a trailing blank. Tab toggles current-project ↔ all-projects and Ctrl+A toggles the archived view, each reloading rows while the typed query survives the rebuild. Enter on a focused row submits `Op::Resume`. Enter with no selection stays open so the user can Tab the scope or Esc out. The footer leads with the Enter and Esc hints, then counts the loaded rows, and flags when more sessions remain beyond the 30-session load cap (`30 most recent sessions`, then `more not loaded`). Load failures take over the footer so a failure can't disguise itself as "0 sessions". Ctrl+D on the cursor row pushes `ConfirmDeleteSessionModal`, and Ctrl+X archives or unarchives the cursor row in place. On focus regain the picker reloads and re-seeks the cursor to the previously selected row, or to the row that followed a deleted one.

- **Confirm-delete modal**: `ConfirmDeleteSessionModal` runs `session::saved::delete` on the session's file synchronously on Y / Enter and submits `ModalAction::SystemMessage("Deleted session {id}: {title}")` on success so the user has chat-stream evidence the action ran. Failures latch into a sticky inline error that clears only on a deliberate Y / N choice, so a stray key can't wipe the diagnostic. Pushed by `/delete <id-prefix>` and by `/resume`'s Ctrl+D.

- **Approval modal**: `ApprovalModal` holds the tool-use id and renders the call summary as its title above the body: the command text for `bash`, or `-` / `+` diff rows for `edit` and `write`. The body shows at most 16 rows and scrolls when the call runs longer or the band is shorter. Y submits `Op::Resolve` with `Approve` once a render has shown the first body row, while Enter does nothing. [permissions.md](../tools/permissions.md) covers the type-ahead guard. When the call offers a session grant, A opens a rule editor prefilled with the suggested rule, and Enter submits `AllowForSession` with the edited rule once a render has shown the whole editor. N submits `Deny`. F opens a feedback editor whose Enter submits `DenyWithFeedback` with the typed text. Esc from the choice, Ctrl+C, and a `clear` on resume or compaction all resolve through `on_cancel` to `Abort`, which ends the turn.

- **`/status` overview**: `KvOverview` of session descriptors (model, effort, cwd, session id, auth, version, cache TTL, auto compaction, show-thinking, show-welcome), plus a Session Rules section listing the session allow rules once any is granted and a Background Shells section listing each open shell with its state and the first line of its command. Constructed in `slash/status`.

- **`/config` overview**: Two-section `KvOverview`. "Resolved" pairs each effective value with a dim note naming its source, while "Source Files" lists the config files loaded at startup, lowest precedence first, each labeled User, Drop-in, Legacy Project, or Project. Constructed in `slash/config`.

- **`/help` overview**: Single-section `KvOverview` of every registered command. Aliases parenthesize after the canonical name, and the `usage()` placeholder appends. Constructed in `slash/help`.

## Out of Scope

- **Persistent modals in chat scroll.** Modals are ephemeral. `/diff` stays a chat-pushed printer because the diff genuinely earns scrollback, and modal cropping would lose value.

- **Mouse interaction.** The TUI captures the mouse ([mouse interactions](../tui/mouse-interactions.md)), but a modal receives only the wheel, as Up / Down keys. It ignores clicks and stays keyboard-driven.

- **Concurrent modals on different layers** (e.g. toast over modal). The chat error-block path already covers what toasts would.

- **User-defined modal commands.** The trait already supports them, and user-invocable skills are tracked in [#163](https://github.com/hakula139/oxide-code/issues/163).

## Sources

- `crates/oxide-code/src/session/display.rs`: shared metadata-line formatter used by the picker row and the confirm-delete modal.
- `crates/oxide-code/src/slash/config.rs`: `/config` row builder plus sectioned `KvOverview`.
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal`.
- `crates/oxide-code/src/slash/context.rs`: `SlashContext`, the read-only state each `execute` call receives.
- `crates/oxide-code/src/slash/delete.rs`: `DeleteCmd` plus prefix resolver.
- `crates/oxide-code/src/slash/effort_slider.rs`: `/effort` slider.
- `crates/oxide-code/src/slash/help.rs`: `/help` row builder plus `KvOverview`.
- `crates/oxide-code/src/slash/init.rs`: `/init` instruction-file picker (`InitPicker`).
- `crates/oxide-code/src/slash/picker.rs`: combined `/model + /effort` picker.
- `crates/oxide-code/src/slash/registry.rs`: `SlashOutcome`, including the `Modal` variant.
- `crates/oxide-code/src/slash/rename.rs`: `RenameCmd` plus `RenameModal` editor.
- `crates/oxide-code/src/slash/resume.rs`: `ResumeCmd` plus `ResumePicker`.
- `crates/oxide-code/src/slash/status.rs`: `/status` row builder plus `KvOverview`.
- `crates/oxide-code/src/slash/theme.rs`: `/theme` live-preview picker (`ThemeCmd` plus `ThemePicker`).
- `crates/oxide-code/src/tui/app/dispatch.rs`: `App::handle_crossterm_event` modal gate, `run_slash`, `apply_slash_outcome`, `apply_modal_action`, `dispatch_action`, `clear_modals`.
- `crates/oxide-code/src/tui/app/events.rs`: `push_approval_modal`.
- `crates/oxide-code/src/tui/app/render.rs`: `draw_frame` modal layout band.
- `crates/oxide-code/src/tui/modal.rs`: `Modal` (including the `on_cancel` and `captures_esc` hooks), `ModalKey`, `ModalAction`, `ModalStack`.
- `crates/oxide-code/src/tui/modal/approval.rs`: `ApprovalModal` approve-or-deny gate for agent tool calls, plus the rule and feedback editors.
- `crates/oxide-code/src/tui/modal/chrome.rs`: shared `Header`, `render_footer`, and the hint fragments.
- `crates/oxide-code/src/tui/modal/keymap.rs`: `Keymap`, `KeyAction`, and `KeyChord`.
- `crates/oxide-code/src/tui/modal/kv_overview.rs`: generic `KvOverview` plus `KvSection`.
- `crates/oxide-code/src/tui/modal/line_editor.rs`: single-line `LineEditor` over `ratatui-textarea`.
- `crates/oxide-code/src/tui/modal/list_picker.rs`: generic `ListPicker<T: PickerItem>`.
- `crates/oxide-code/src/tui/modal/searchable_list.rs`: generic `SearchableList<T: SearchableItem>`.
- `crates/oxide-code/src/tui/viewport.rs`: `Viewport` scroll window plus `wrap_next` / `wrap_prev` cursor steps.
