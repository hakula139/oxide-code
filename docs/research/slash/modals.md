# Slash-Command Modals (Reference)

Research on the modal / picker / dialog primitives that turn slash commands into interactive surfaces (live model picker, theme switcher, permission prompt). Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

Companion to [commands.md](commands.md), which covers the command-surface shape. This file focuses on the interactive UI.

## Claude Code (TypeScript + Ink)

Modals are React components rendered by Ink. ~50 of ~100 commands are `type: 'local-jsx'`.

- **Discriminator**: `type: 'local-jsx'` on the command record. Lazy module via `load: () => import('./<name>.js')`, so the modal component never loads until the command runs.
- **Registry shape**: Each command directory ships `index.ts` (declarative metadata) plus `<name>.tsx` (the modal component itself). Adding a modal command is one directory.
- **Lifecycle**: Trigger → lazy-load module → mount component via `showSetupDialog(root, render)` → component captures keys (arrows / Enter / Esc) → `onSelect(value)` callback → unmount → result delivered to dispatcher.
- **State ownership**: Per-modal local state (selected index, filter query). No shared modal-state store, and each component is self-contained.
- **Layering**: Modal renders **above** the input area, **not** inside chat scroll. Chat is visible behind. Input is implicitly suppressed because Ink routes keys to the active component.
- **Result delivery**: Callback-driven. Modal calls `onDone(value, { displayMode, shouldQuery })`. `displayMode: 'system'` posts a synthetic `SystemMessageBlock`, and `shouldQuery: true` forwards to the agent loop.
- **Reusable primitives**: Lightweight, with `Box`, `Text`, and `SelectInput`-style hand-rolled lists. No shared `Modal` / `Dialog` wrapper, so each command builds its UI directly with Ink primitives.
- **Nesting**: Supported via React tree (modal can render another modal as a child).
- **Live data**: Dynamic `description` getter on the command record pulls live state (e.g., `Set the AI model (currently ${renderModelName(getMainLoopModel())})`).

## OpenAI Codex (Rust + Ratatui)

Trait-based view stack at the bottom of the frame. The closest stack-wise to oxide-code.

- **Core trait**: `BottomPaneView` (`tui/src/bottom_pane/bottom_pane_view.rs`). Methods: `handle_key_event`, `is_complete`, `completion()`, `on_ctrl_c`, `terminal_title_requires_action`, plus paste / approval / input-request consumption hooks.
- **View stack**: `BottomPane` owns `view_stack: Vec<Box<dyn BottomPaneView>>` plus a permanent `ChatComposer` that hides while a view is active.
- **Focus model**: Stack presence **is** focus state. Non-empty stack means top view receives keys, while empty stack means composer receives keys. No global flag.
- **Auto-cascade**: When `view.is_complete()` returns true, `BottomPane` pops the view, re-renders the next one (or composer), and emits the view's `completion()` payload as an `AppEvent`.
- **Triggers**: Two paths.
  1. Slash command match arm pushes a view (`/model`, `/effort`, etc.).
  2. Agent emits `AskForApproval` event, then `BottomPane::try_consume_approval_request()` pushes an `ApprovalOverlay`.
- **Reusable primitives**:
  - `ListSelectionView`: generic ranked + filtered picker (`SelectionItem` items, Up / Down / numeric shortcuts, side-by-side preview).
  - `MultiSelectPicker`: checkbox variant.
  - `SelectionTabs`: tab bar across multiple list views.
  - All implement the `Renderable` ratatui-widget trait.
- **Result delivery**: Async via `AppEvent::SubmitApprovalDecision { decision, ... }`. The agent doesn't block, since views drop a typed event on the app event channel.
- **Keymap composition**: `RuntimeKeymap` plus per-view overrides (`ApprovalKeymap` locks Esc to Cancel, while `ListKeymap` adds j/k navigation).

## opencode (TypeScript + Solid.js + Kobalte)

Imperative single-modal API.

- **Primitive**: `dialog.show(component, onClose?)` from `useDialog()` context. Single active modal, since `.show()` disposes any prior modal before mounting.
- **Component shape**: Dialog components live in `packages/app/src/components/dialog-*.tsx`. Each is a Solid component wrapping a Kobalte `<Dialog>` (overlay, focus trap, accessibility built in).
- **Trigger pattern**: Command's `onSelect` handler does a lazy import then `dialog.show()`:

  ```typescript
  const chooseModel = () => {
    void import("@/components/dialog-select-model").then((x) => {
      dialog.show(() => <x.DialogSelectModel model={local.model} />)
    })
  }
  ```

- **Layering**: Full-screen modal overlay via Kobalte portal. `z: modal` for dialogs, `z: toast` (higher) for toasts. Input field is rendered outside the portal but is layered under the overlay.
- **Result delivery**: Side effects plus explicit close. Component mutates app state directly (`local.model.set(selected)`) and calls `dialog.close()`. No promise-based result return.
- **Toast vs dialog**: Separate. `showToast({ variant, title, description })` is a different API for non-blocking notifications, never embedded in a dialog.
- **Nesting**: Not supported (rationale: simpler focus management, but blocks confirm-inside-picker UX).

## Comparison

| Aspect             | Claude Code                          | Codex (Rust)                        | opencode                       |
| ------------------ | ------------------------------------ | ----------------------------------- | ------------------------------ |
| Trigger            | `local-jsx` discriminator on command | match arm in slash dispatch         | command `onSelect` callback    |
| Trait / API        | React component                      | `BottomPaneView` trait + view stack | imperative `dialog.show(node)` |
| Focus gating       | implicit (Ink scope)                 | stack presence                      | full-screen overlay + portal   |
| Nesting            | yes (React tree)                     | yes (Vec stack)                     | no (single-at-a-time)          |
| Result delivery    | callback (`onDone(value, opts)`)     | async event (`AppEvent::*`)         | side effects + `close()`       |
| Reusable primitive | none (each modal hand-rolled)        | `ListSelectionView` (generic)       | Kobalte Dialog primitives      |
| Lazy load          | `load: () => import(...)`            | n/a (compiled binary)               | `void import(...).then()`      |
| Agent-triggered    | no first-class path                  | `try_consume_approval_request()`    | no first-class path            |

## Patterns Worth Borrowing for oxide-code

1. **Codex's `BottomPaneView` trait plus view stack.** Idiomatic Rust+Ratatui shape, with a small trait, single owner, and stack semantics that give nesting for free without adding focus-flag bookkeeping. Direct port target.

2. **Codex's generic `ListSelectionView`.** A single picker primitive parameterized by an item trait is what `/model`, `/effort`, `/theme`, future `/agents`, and any approval prompt all want. Beats hand-rolling per command (Claude Code's path).

3. **Two open paths from day one.** Slash-triggered plus agent-triggered. The trait shape is identical for both, with only the call site differing. Codex already does this with `try_consume_approval_request`. Designing the trait for both up front means the future Permission feature drops in without re-shaping.

4. **Async event for completion rather than callback.** Modal pushes a `UserAction` (or richer `ModalEvent`) onto the existing `user_tx` channel. Symmetric with how the rest of oxide-code's UI talks to the agent loop. Avoids the `Box<dyn FnOnce>` and lifetime gymnastics of a callback-based design.

5. **Lazy live-data getters on slash command metadata.** Claude Code's dynamic `description()` lets the popup show `(currently sonnet)` without a refresh hook. Cheap, useful, doesn't require the modal infrastructure.

## Patterns to Reject

1. **opencode's single-modal-only.** Saves ~30 lines of stack code, but costs every confirm-inside-picker flow forever.

2. **Claude Code's per-command UI hand-rolling.** With ~50 modal commands they need either a shared kit or a lot of duplication. A generic picker primitive scales better. Build it before the second modal lands.

3. **opencode's imperative `dialog.show()` with side-effecting components.** State-mutation-as-result is hard to test in Rust. Prefer typed `ModalAction` returns.

4. **Codex's permanent `ChatComposer` behind every view.** Useful for "preserve typed text across modal dismiss" but adds layout complexity. Only worth it if oxide-code's typing-during-modal turns out to be common. Defer.
