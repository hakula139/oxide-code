# Modal UI

Focus-grabbing UI overlays that any slash command can open. Lives in the band between the chat scroll and the input area, the same row range the slash autocomplete popup uses, but wider.

Companion: [commands.md](commands.md) — slash-command surface that opens modals. Research: [`docs/research/slash/modals.md`](../../research/slash/modals.md).

## Goals

A modal is a self-contained UI that takes keyboard focus, owns its render, emits a typed result, and dismisses. Chat blocks are persistent transcript artifacts; modals are ephemeral overlays. They are not the same primitive.

Three things drove the abstraction:

1. **Live preview.** A future `/theme` command needs to swap palettes as the user arrows through choices and snap back on Esc.
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

[`crates/oxide-code/src/tui/modal.rs`](../../../crates/oxide-code/src/tui/modal.rs) defines the trait, key outcome, and `ModalStack` manager. [`crates/oxide-code/src/tui/modal/list_picker.rs`](../../../crates/oxide-code/src/tui/modal/list_picker.rs) is the generic primitive that concrete pickers embed.

Two concrete modals ship today:

- [`crates/oxide-code/src/slash/picker.rs`](../../../crates/oxide-code/src/slash/picker.rs) — combined `/model + /effort` picker.
- [`crates/oxide-code/src/slash/status_modal.rs`](../../../crates/oxide-code/src/slash/status_modal.rs) — `/status` overview.

App owns `ModalStack` and runs the key gate first in `handle_crossterm_event`: an active modal sees every key before any other component, then `apply_modal_action` dispatches the result through the same path as a keyboard `UserAction`.

## Design Decisions

1. **Modal trait, not enum.** Each concrete modal is its own type implementing `Modal`. Adding one is a new file plus a constructor — no central match arm.
2. **Stack-based ownership (`Vec<Box<dyn Modal>>`).** Single-element today; the `Vec` is there so a future "confirm leave?" overlay inside a picker can `push` without a redesign.
3. **Typed result delivery, no callbacks.** Modal emits `ModalKey::Submitted(ModalAction)`; manager dispatches. Boxed `FnOnce` callbacks were rejected for lifetime / `Send` complexity and because they hide the dispatch graph.
4. **Modals receive a `&SessionInfo` snapshot at open.** Reactive subscriptions are deferred — when a value changes mid-modal (rare), the modal closes and reopens with fresh state.
5. **Layout band sized by `ModalStack::height(width)`.** Zero rows when empty (existing layout unchanged); displaces the chat upward when active, just like the slash popup.
6. **Modals open via `SlashContext::open_modal`, not a new `SlashOutcome` variant.** Keeps `SlashOutcome` derive-clean (`Debug + PartialEq + Eq`). The dispatcher harvests the slot after `execute` and pushes onto the App's stack — same shape as `chat: &mut ChatView` for write-effects.
7. **Bare `/model` and bare `/effort` open the same modal**, with different initial focus (Codex pattern). Typed-arg `/model <id>` and `/effort <level>` keep their direct-switch behaviour for scripting and power users.
8. **Generic [`ListPicker<T: PickerItem>`] is _not_ a `Modal`.** It is a state + render primitive that concrete pickers embed and forward keys to. This separates "list selection state" from "what does Enter dispatch", which avoids the boxed-callback pattern while staying broadly reusable (`/model + /effort` today; future `/theme`, future approval prompts).
9. **`/status` on Esc and Enter both dismiss.** Read-only overview — there's nothing to "confirm". The dual binding makes the close gesture muscle-memory-friendly across users coming from different conventions.

## Per-Modal Notes

### Combined `/model + /effort` picker

[`slash::picker::ModelEffortPicker`](../../../crates/oxide-code/src/slash/picker.rs) wraps `ListPicker<ModelRow>` and tracks the effort axis separately. `ModelRow` carries `is_active` (drawn with `✓`) and a `key_hint` (`'1'`–`'9'`) for jump-to-row. The picker drops Claude Code's "Default (recommended)" upsell label, drops the `/fast` line, and shows the canonical model id alongside the marketing name (so `/model <id>` direct-switch and the picker share a mental model).

Effort row hides automatically on no-tier models (Haiku 4.5). Left/Right cycles only through tiers the highlighted model supports; the resolved effort is recomputed on every cursor move so the displayed value never claims a tier the next request would silently clamp.

Submit emits a single `UserAction::SwapConfig { model, effort }`. Both axes are `Option`s — only the axes the user actually changed are populated, so the agent loop can re-clamp atomically without redundant work. When neither axis changed, Enter is treated as `Cancelled`.

### `/status` overview

[`slash::status_modal::StatusModal`](../../../crates/oxide-code/src/slash/status_modal.rs) renders a kv-table of session descriptors (model, effort, working directory, session id, auth, version, context-cache TTL, show-thinking). Single panel — no tabs today. When `/usage` and `/stats` exist, the modal grows a tab bar (modal-internal change, no infrastructure work).

## Out of Scope / Deferred

- **Persistent modals in chat scroll.** Modals are ephemeral. Persistent "what models exist" output goes through `/help` or future `/model --list` text.
- **Mouse interaction.** Defer to a polish PR if the workflow asks for it.
- **Concurrent modals on different layers** (e.g. toast over modal). The chat error-block path already covers what toasts would.
- **Custom user-defined modal commands.** The trait is open, but `~/.config/ox/commands/*.md` discovery / loader is tracked separately under "Workflow Skills" in the roadmap.
- **Agent-triggered modal path** (`AgentEvent::PromptRequest` round-trip). The trait shape supports it; lands with the Permission & Approval roadmap item.

## Sources

- [`crates/oxide-code/src/tui/modal.rs`](../../../crates/oxide-code/src/tui/modal.rs) — `Modal`, `ModalKey`, `ModalAction`, `ModalStack`.
- [`crates/oxide-code/src/tui/modal/list_picker.rs`](../../../crates/oxide-code/src/tui/modal/list_picker.rs) — generic `ListPicker<T: PickerItem>`.
- [`crates/oxide-code/src/slash/picker.rs`](../../../crates/oxide-code/src/slash/picker.rs) — model + effort picker.
- [`crates/oxide-code/src/slash/status_modal.rs`](../../../crates/oxide-code/src/slash/status_modal.rs) — status overview.
- [`crates/oxide-code/src/slash/context.rs`](../../../crates/oxide-code/src/slash/context.rs) — `SlashContext::open_modal` / `take_modal`.
- [`crates/oxide-code/src/tui/app.rs`](../../../crates/oxide-code/src/tui/app.rs) — `App::handle_crossterm_event` modal gate, `apply_modal_action`, layout band.
