# Modal UI

Focus-grabbing UI overlays that any slash command can open. Lives in the band between the chat scroll and the input area, the same row range the slash autocomplete popup uses, but wider.

Companion: [commands.md](commands.md) — slash-command surface that opens modals.

## Goals

A modal is a self-contained UI that takes keyboard focus, owns its render, emits a typed result, and dismisses. Chat blocks are persistent transcript artifacts; modals are ephemeral overlays. They are not the same primitive.

Three things drove the abstraction:

1. **Live preview.** `/theme` swaps palettes as the user arrows through choices and snaps back on Esc — the original motivating case for the trait shape.
2. **Multi-step interaction.** The combined `/model + /effort` picker. Plan approval. MCP server pick-then-configure.
3. **Agent-driven prompts.** When a tool wants permission, the agent must surface a prompt and route the user's decision back. Today there is no UI seam for this; the modal trait is shaped to support it later.

## Trait Shape

```rust
pub(crate) trait Modal: Send {
    fn height(&self, width: u16) -> u16;
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);
    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey;
}

pub(crate) enum ModalKey {
    Consumed,                     // stay open; key handled
    Cancelled,                    // close; no dispatch
    Submitted(ModalAction),       // close; apply action
}

pub(crate) enum ModalAction {
    None,                         // modal already applied effects locally
    User(UserAction),             // forward through the agent channel
}
```

`Send` because App lives on tokio; never `Sync` — modals own mutable state and are not shared across threads.

## Implementation

[`crates/oxide-code/src/tui/modal.rs`](../../../crates/oxide-code/src/tui/modal.rs) defines the trait, key outcome, and `ModalStack` manager. Two generic primitives sit alongside it: [`list_picker`](../../../crates/oxide-code/src/tui/modal/list_picker.rs) (cursor + render for selection lists) and [`kv_overview`](../../../crates/oxide-code/src/tui/modal/kv_overview.rs) (sectioned read-only kv table). Concrete modals embed them.

Modals shipping today:

- [`crates/oxide-code/src/slash/picker.rs`](../../../crates/oxide-code/src/slash/picker.rs) — combined `/model + /effort` picker (over `ListPicker`).
- [`crates/oxide-code/src/slash/effort_slider.rs`](../../../crates/oxide-code/src/slash/effort_slider.rs) — bare `/effort` Speed ↔ Intelligence slider.
- [`crates/oxide-code/src/slash/theme.rs`](../../../crates/oxide-code/src/slash/theme.rs) — `/theme` live-preview palette picker (over `ListPicker`).
- `/status`, `/config`, `/help` — read-only overviews assembled from `KvOverview` + `KvSection` fixtures inside the per-command files.

App owns `ModalStack` and runs the key gate first in `handle_crossterm_event`: an active modal sees every key before any other component, then `apply_modal_action` dispatches the result through the same path as a keyboard `UserAction`.

## Design Decisions

1. **Modal trait, not enum.** Each concrete modal is its own type implementing `Modal`. Adding one is a new file plus a constructor — no central match arm.
2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** Single-element today; the `Vec` is there so a future "confirm leave?" overlay inside a picker can `push` without a redesign.
3. **Typed result delivery, no callbacks.** Modal emits `ModalKey::Submitted(ModalAction)`; manager dispatches. Boxed `FnOnce` callbacks were rejected for lifetime / `Send` complexity and because they hide the dispatch graph.
4. **Modals receive a `&LiveSessionInfo` snapshot at open.** Reactive subscriptions are deferred — when a value changes mid-modal (rare), the modal closes and reopens with fresh state.
5. **Layout band sized by `ModalStack::height(width)`.** Zero rows when empty (existing layout unchanged); displaces the chat upward when active, just like the slash popup.
6. **Modals open via `SlashContext::open_modal`, not a new `SlashOutcome` variant.** Keeps `SlashOutcome` derive-clean (`Debug + PartialEq + Eq`). The dispatcher harvests the slot after `execute` and pushes onto the App's stack — same shape as `chat: &mut ChatView` for write-effects.
7. **Bare `/model` and bare `/effort` open separate modals — combined picker vs. slider.** `/model` keeps the multi-axis combined picker (model and effort cycle together); `/effort` gets its own single-axis slider. Threading both bare forms through one modal would force a single-axis decision through a two-axis interface. Typed-arg `/model <id>` and `/effort <level>` keep direct-switch behaviour for scripting and power users.
8. **Generic [`ListPicker<T: PickerItem>`] is _not_ a `Modal`.** It is a state + render primitive that concrete pickers embed and forward keys to. This separates "list selection state" from "what does Enter dispatch", which avoids the boxed-callback pattern while staying broadly reusable — `/model + /effort` and `/theme` both build on it today; future approval prompts will too.
9. **Read-only kv overviews share `KvOverview`.** `/status`, `/config`, `/help` all build the same shape — title + sectioned label-value rows + footer — so the layout, key handling, and Esc / Enter dismiss live in one place. Per-command files own only the fixture (rows, headings) and a thin constructor. New overviews are a `Vec<KvSection>` away.
10. **Esc and Enter both dismiss `KvOverview`.** Read-only — there's nothing to "confirm". The dual binding makes the close gesture muscle-memory-friendly across users coming from different conventions.

## Per-Modal Notes

- **Combined `/model + /effort` picker** — [`ModelEffortPicker`](../../../crates/oxide-code/src/slash/picker.rs) wraps `ListPicker<ModelRow>` and tracks the effort axis separately. Effort row hides on no-tier models; Left / Right cycles only through tiers the highlighted model supports, recomputed per cursor move so the display never claims a tier the next request would clamp. Submit emits one `UserAction::SwapConfig { model, effort }` with `Option` axes — only changed axes populated; Enter on a no-op cancels.
- **`/effort` slider** — [`EffortSlider`](../../../crates/oxide-code/src/slash/effort_slider.rs) is a horizontal Speed ↔ Intelligence visual. Lists only tiers the active model accepts; seeds the cursor at the resolved active effort. Tiers render with uniform `●` / `○` glyphs plus per-tier ANSI color along blue → red — color encodes identity, BOLD encodes the active pick. ANSI-named colors decouple the gradient from theme TOML so the user's terminal palette supplies the actual rendering.
- **`/theme` picker** — [`ThemePicker`](../../../crates/oxide-code/src/slash/theme.rs) wraps `ListPicker<ThemeRow>` over the curated built-in roster and emits `UserAction::PreviewTheme` on every cursor move so the App repaints the chat in the candidate palette without committing. Esc snaps back via the cached `preview_theme_snapshot`; Enter promotes the preview to a `SwapTheme` swap. Numeric `1`–`9` shortcuts jump to a row to match the visual ladder.
- **`/status` overview** — Single-section [`KvOverview`](../../../crates/oxide-code/src/tui/modal/kv_overview.rs) of session descriptors (model, effort, cwd, session id, auth, version, cache TTL, show-thinking, show-welcome). Constructed in [`slash/status.rs`](../../../crates/oxide-code/src/slash/status.rs).
- **`/config` overview** — Two-section [`KvOverview`](../../../crates/oxide-code/src/tui/modal/kv_overview.rs): "Resolved" (effective config values) and "Source Files" (the layered TOML paths it was assembled from). Path discovery runs per-invocation. Constructed in [`slash/config.rs`](../../../crates/oxide-code/src/slash/config.rs).
- **`/help` overview** — Single-section [`KvOverview`](../../../crates/oxide-code/src/tui/modal/kv_overview.rs) of every registered command. Aliases parenthesize after the canonical name; `usage()` placeholder appends. Constructed in [`slash/help.rs`](../../../crates/oxide-code/src/slash/help.rs).

## Out of Scope / Deferred

- **Persistent modals in chat scroll.** Modals are ephemeral. `/diff` stays a chat-pushed printer because the diff genuinely earns scrollback; modal cropping would lose value. Future `/model --list`-style listings can ride the same printer path.
- **Mouse interaction.** Defer to a polish PR if the workflow asks for it.
- **Concurrent modals on different layers** (e.g. toast over modal). The chat error-block path already covers what toasts would.
- **Custom user-defined modal commands.** The trait is open, but `~/.config/ox/commands/*.md` discovery / loader is tracked separately under "Workflow Skills" in the roadmap.
- **Agent-triggered modal path** (`AgentEvent::PromptRequest` round-trip). The trait shape supports it; lands with the Permission & Approval roadmap item.

## Sources

- `crates/oxide-code/src/tui/modal.rs` — `Modal`, `ModalKey`, `ModalAction`, `ModalStack`.
- `crates/oxide-code/src/tui/modal/list_picker.rs` — generic `ListPicker<T: PickerItem>`.
- `crates/oxide-code/src/tui/modal/kv_overview.rs` — generic `KvOverview` + `KvSection`.
- `crates/oxide-code/src/slash/picker.rs` — model + effort picker.
- `crates/oxide-code/src/slash/effort_slider.rs` — `/effort` slider.
- `crates/oxide-code/src/slash/theme.rs` — `/theme` live-preview picker (`ThemeCmd` + `ThemePicker`).
- `crates/oxide-code/src/slash/status.rs` — `/status` row builder + `KvOverview`.
- `crates/oxide-code/src/slash/config.rs` — `/config` row builder + sectioned `KvOverview`.
- `crates/oxide-code/src/slash/help.rs` — `/help` row builder + `KvOverview`.
- `crates/oxide-code/src/slash/context.rs` — `SlashContext::open_modal` / `take_modal`.
- `crates/oxide-code/src/tui/app.rs` — `App::handle_crossterm_event` modal gate, `apply_modal_action`, layout band.
