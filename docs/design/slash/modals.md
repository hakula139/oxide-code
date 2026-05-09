# Modal UI

Focus-grabbing UI overlays that any slash command can open. Lives in the band between the chat scroll and the input area, the same row range the slash autocomplete popup uses, but wider.

Companion: [commands.md](commands.md) — slash-command surface that opens modals.

## Goals

A modal is a self-contained UI that takes keyboard focus, owns its render, emits a typed result, and dismisses. Chat blocks are persistent transcript artifacts; modals are ephemeral overlays.

Three things drove the abstraction:

1. **Live preview.** `/theme` swaps palettes as the user arrows through choices and snaps back on Esc — the original motivating case for the trait shape.
2. **Multi-step interaction.** The combined `/model + /effort` picker. Plan approval. MCP server pick-then-configure.
3. **Agent-driven prompts.** When a tool wants permission, the agent must surface a prompt and route the user's decision back. Today there is no UI seam for this; the modal trait is shaped to support it later.

## Trait Shape

A modal is `Send` (App lives on tokio) but not `Sync`, since modals own mutable state and are never shared across threads. It declares its `height` for layout, paints itself into a `Rect`, and routes one `KeyEvent` to one of four outcomes:

- **Consumed** — stay open, key handled internally.
- **Cancelled** — close, no dispatch.
- **Submitted** — close and forward an action (a `UserAction` through the agent channel, or a no-op when the modal already applied its effect locally).
- **Previewed** — forward an action without closing. Used for live-preview cursor moves like `/theme`, where each Up / Down repaints the chat in the candidate without committing.

Dispatched actions flow through the same channel as keyboard input, so the modal never needs to know what handler will run on the other side.

## Implementation

`tui/modal` defines the trait, key outcome, and `ModalStack` manager. Two generic primitives sit alongside it: `list_picker` (cursor + render for selection lists) and `kv_overview` (sectioned read-only kv table). A third, `searchable_list`, adds a substring filter and viewport for the `/resume` picker. Concrete modals embed these.

Modals shipping today:

- **Combined `/model + /effort` picker** (over `ListPicker`).
- **`/effort` slider** — bare `/effort` Speed ↔ Intelligence slider.
- **`/theme` picker** — live-preview palette picker (over `ListPicker`).
- **`/rename` editor** — single-line title editor pre-filled with the current title.
- **`/resume` picker** — searchable session picker (over `SearchableList`).
- **`/status`, `/config`, `/help`** — read-only overviews assembled from `KvOverview` + `KvSection` fixtures inside the per-command files.

App owns `ModalStack` and runs the key gate first inside `handle_crossterm_event`, so an active modal sees every key before any other component. `apply_modal_action` then dispatches the result through the same path as a keyboard `UserAction`.

## Design Decisions

1. **Trait per modal.** Each concrete modal is its own type implementing `Modal`, so adding one is a new file plus a constructor with no central match arm.
2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** Single-element today, but the `Vec` lets a future "confirm leave?" overlay inside a picker `push` itself on without a redesign.
3. **Typed result delivery, no callbacks.** Modal emits `ModalKey::Submitted(ModalAction)` and the manager dispatches; boxed `FnOnce` callbacks were rejected for the lifetime / `Send` complexity and because they hide the dispatch graph.
4. **Modals receive a `&LiveSessionInfo` snapshot at open.** Reactive subscriptions are deferred, so when a value changes mid-modal (rare) the modal closes and reopens with fresh state.
5. **Layout band sized by `ModalStack::height(width)`.** Zero rows when empty (so the existing layout stays unchanged); displaces the chat upward when active, just like the slash popup.
6. **Modals open via `SlashContext::open_modal` instead of a new `SlashOutcome` variant.** Keeps `SlashOutcome` derive-clean (`Debug + PartialEq + Eq`). The dispatcher harvests the slot after `execute` and pushes onto the App's stack, mirroring how `chat: &mut ChatView` carries write-effects.
7. **Bare `/model` and bare `/effort` open separate modals.** Threading both bare forms through one modal would force a single-axis decision through a two-axis interface. `/model` keeps the multi-axis combined picker (model and effort cycle together), while `/effort` gets its own single-axis slider. Typed-arg `/model <id>` and `/effort <level>` keep direct-switch behaviour for scripting and power users.
8. **`ListPicker` is a state + render primitive.** Concrete pickers embed it and forward keys to it. This separates "list selection state" from "what does Enter dispatch", avoiding the boxed-callback pattern while staying broadly reusable. `/model + /effort` and `/theme` both build on it today; future approval prompts will too.
9. **Read-only kv overviews share `KvOverview`.** `/status`, `/config`, and `/help` all build the same title + sectioned-rows + footer shape, so the layout, key handling, and dismiss live in one place. Per-command files own only the fixture (rows, headings) and a thin constructor, and new overviews are a `Vec<KvSection>` away.
10. **Read-only modals don't bind Enter.** `KvOverview::handle_key` consumes every key, and Esc / Ctrl+C cancel universally at the stack layer. Enter stays reserved for commit semantics in `ListPicker`-based modals, since binding it to dismiss in `KvOverview` would give the same gesture two meanings across modal types.

## Per-Modal Notes

- **Combined `/model + /effort` picker** — `ModelEffortPicker` wraps `ListPicker<ModelRow>` and tracks the effort axis separately. The effort row hides on no-tier models. Left / Right cycles only through tiers the highlighted model supports, recomputed per cursor move so the display never claims a tier the next request would clamp. Submit emits one `UserAction::SwapConfig { model, effort }` with `Option` axes (only changed axes populated). Enter on a no-op cancels.
- **`/effort` slider** — `EffortSlider` is a horizontal Speed ↔ Intelligence visual. Lists only tiers the active model accepts and seeds the cursor at the resolved active effort. Tiers render with uniform `●` / `○` glyphs plus per-tier ANSI color along blue → red. Color encodes identity, bold encodes the active pick. ANSI-named colors decouple the gradient from theme TOML so the user's terminal palette supplies the actual rendering.
- **`/theme` picker** — `ThemePicker` wraps `ListPicker<ThemeRow>` over the curated built-in roster and emits `UserAction::PreviewTheme` on every cursor move so the App repaints the chat in the candidate palette without committing. Esc snaps back via the cached `preview_theme_snapshot`; Enter promotes the preview to a `SwapTheme` swap. Numeric `1`–`9` shortcuts jump to a row to match the visual ladder.
- **`/rename` editor** — `RenameModal` is a single-line text editor pre-filled with the current title (cap 80 chars, mirroring the actor's first-prompt cap). Render is a fixed five-row stack: title, gap, prompt + buffer, gap, footer hint. Enter on a non-empty trimmed buffer submits `UserAction::Rename`; blank Enter is a silent no-op so the user can keep typing. Cursor clamps to the right edge on overflow.
- **`/resume` picker** — `ResumePicker` wraps `SearchableList<SessionRow>` and adds a footer line. Each row paints a two-line title + dim metadata block (id prefix · relative time · message count · branch · project) plus a trailing blank. Tab toggles current-project ↔ all-projects and reloads rows; the typed query survives the rebuild. Enter on a focused row submits `UserAction::Resume`; Enter with no selection stays open so the user can Tab the scope or Esc out. Footer surfaces load failures inline so a failure can't disguise itself as "0 sessions".
- **`/status` overview** — Single-section `KvOverview` of session descriptors (model, effort, cwd, session id, auth, version, cache TTL, show-thinking, show-welcome). Constructed in `slash/status`.
- **`/config` overview** — Two-section `KvOverview`: "Resolved" (effective config values) and "Source Files" (the layered TOML paths it was assembled from). Path discovery runs per-invocation. Constructed in `slash/config`.
- **`/help` overview** — Single-section `KvOverview` of every registered command. Aliases parenthesize after the canonical name; `usage()` placeholder appends. Constructed in `slash/help`.

## Out of Scope / Deferred

- **Persistent modals in chat scroll.** Modals are ephemeral. `/diff` stays a chat-pushed printer because the diff genuinely earns scrollback; modal cropping would lose value. Future `/model --list`-style listings can ride the same printer path.
- **Mouse interaction.** Defer to a polish PR if the workflow asks for it.
- **Concurrent modals on different layers** (e.g. toast over modal). The chat error-block path already covers what toasts would.
- **Custom user-defined modal commands.** The trait is open, but `~/.config/ox/commands/*.md` discovery / loader is tracked separately under "Workflow Skills" in the roadmap.
- **Agent-triggered modal path** (`AgentEvent::PromptRequest` round-trip). The trait shape supports it; lands with the Permission & Approval roadmap item.

## Sources

- `crates/oxide-code/src/slash/config.rs` — `/config` row builder + sectioned `KvOverview`.
- `crates/oxide-code/src/slash/context.rs` — `SlashContext::open_modal` / `take_modal`.
- `crates/oxide-code/src/slash/effort_slider.rs` — `/effort` slider.
- `crates/oxide-code/src/slash/help.rs` — `/help` row builder + `KvOverview`.
- `crates/oxide-code/src/slash/picker.rs` — combined `/model + /effort` picker.
- `crates/oxide-code/src/slash/rename.rs` — `RenameCmd` + `RenameModal` editor.
- `crates/oxide-code/src/slash/resume.rs` — `ResumeCmd` + `ResumePicker`.
- `crates/oxide-code/src/slash/status.rs` — `/status` row builder + `KvOverview`.
- `crates/oxide-code/src/slash/theme.rs` — `/theme` live-preview picker (`ThemeCmd` + `ThemePicker`).
- `crates/oxide-code/src/tui/app.rs` — `App::handle_crossterm_event` modal gate, `apply_modal_action`, layout band.
- `crates/oxide-code/src/tui/modal.rs` — `Modal`, `ModalKey`, `ModalAction`, `ModalStack`.
- `crates/oxide-code/src/tui/modal/kv_overview.rs` — generic `KvOverview` + `KvSection`.
- `crates/oxide-code/src/tui/modal/list_picker.rs` — generic `ListPicker<T: PickerItem>`.
- `crates/oxide-code/src/tui/modal/searchable_list.rs` — generic `SearchableList<T: SearchableItem>`.
