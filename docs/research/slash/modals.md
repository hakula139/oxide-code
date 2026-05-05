# Slash-Command Modals (Reference)

Research on the modal / picker / dialog primitives that turn slash commands into interactive surfaces (live model picker, theme switcher, permission prompt). Companion to [commands.md](commands.md), which covers the command-surface shape; this file focuses on the interactive UI.

Verified against locally-mirrored sources (2026-05-05): [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex) `codex-rs/tui`, [opencode](https://github.com/anomalyco/opencode) `packages/`.

> **Status:** the design synthesis from this research has been implemented. See [`docs/design/slash/modals.md`](../../design/slash/modals.md) for the shipped shape.

## Claude Code (TypeScript + Ink)

Modals are React components rendered by Ink. ~50 of ~100 commands are `type: 'local-jsx'`.

- **Discriminator**: `type: 'local-jsx'` on the command record. Lazy module via `load: () => import('./<name>.js')` — the modal component never loads until the command runs.
- **Registry shape**: each command directory ships `index.ts` (declarative metadata) + `<name>.tsx` (the modal component itself). Adding a modal command is one directory.
- **Lifecycle**: trigger → lazy-load module → mount component via `showSetupDialog(root, render)` → component captures keys (arrows / Enter / Esc) → `onSelect(value)` callback → unmount → result delivered to dispatcher.
- **State ownership**: per-modal local state (selected index, filter query). No shared modal-state store; each component is self-contained.
- **Layering**: modal renders **above** the input area, **not** inside chat scroll. Chat is visible behind. Input is implicitly suppressed because Ink routes keys to the active component.
- **Result delivery**: callback-driven. Modal calls `onDone(value, { displayMode, shouldQuery })`. `displayMode: 'system'` posts a synthetic `SystemMessageBlock`; `shouldQuery: true` forwards to the agent loop.
- **Reusable primitives**: lightweight — `Box`, `Text`, `SelectInput`-style hand-rolled lists. No shared `Modal` / `Dialog` wrapper. Each command builds its UI directly with Ink primitives.
- **Nesting**: supported via React tree (modal can render another modal as a child).
- **Live data**: dynamic `description` getter on the command record pulls live state (e.g., `Set the AI model (currently ${renderModelName(getMainLoopModel())})`).

## OpenAI Codex (Rust + Ratatui)

Trait-based view stack at the bottom of the frame. The closest stack-wise to oxide-code.

- **Core trait**: `BottomPaneView` (`tui/src/bottom_pane/bottom_pane_view.rs`). Methods: `handle_key_event`, `is_complete`, `completion()`, `on_ctrl_c`, `terminal_title_requires_action`, plus paste / approval / input-request consumption hooks.
- **View stack**: `BottomPane` owns `view_stack: Vec<Box<dyn BottomPaneView>>` plus a permanent `ChatComposer` that hides while a view is active.
- **Focus model**: stack presence **is** focus state. Non-empty stack = top view receives keys; empty stack = composer receives keys. No global flag.
- **Auto-cascade**: when `view.is_complete()` returns true, `BottomPane` pops the view, re-renders the next one (or composer), and emits the view's `completion()` payload as an `AppEvent`.
- **Triggers**: two paths.
  1. Slash command match arm pushes a view (`/model`, `/effort`, etc.).
  2. Agent emits `AskForApproval` event → `BottomPane::try_consume_approval_request()` pushes an `ApprovalOverlay`.
- **Reusable primitives**:
  - `ListSelectionView` — generic ranked + filtered picker (`SelectionItem` items, Up / Down / numeric shortcuts, side-by-side preview).
  - `MultiSelectPicker` — checkbox variant.
  - `SelectionTabs` — tab bar across multiple list views.
  - All implement the `Renderable` ratatui-widget trait.
- **Result delivery**: async via `AppEvent::SubmitApprovalDecision { decision, ... }`. The agent doesn't block; views drop a typed event on the app event channel.
- **Keymap composition**: `RuntimeKeymap` + per-view overrides (`ApprovalKeymap` locks Esc to Cancel; `ListKeymap` adds j/k navigation).

## opencode (TypeScript + Solid.js + Kobalte)

Imperative single-modal API.

- **Primitive**: `dialog.show(component, onClose?)` from `useDialog()` context. Single active modal — `.show()` disposes any prior modal before mounting.
- **Component shape**: dialog components live in `packages/app/src/components/dialog-*.tsx`. Each is a Solid component wrapping a Kobalte `<Dialog>` (overlay, focus trap, accessibility built in).
- **Trigger pattern**: command's `onSelect` handler does a lazy import then `dialog.show()`:

  ```typescript
  const chooseModel = () => {
    void import("@/components/dialog-select-model").then((x) => {
      dialog.show(() => <x.DialogSelectModel model={local.model} />)
    })
  }
  ```

- **Layering**: full-screen modal overlay via Kobalte portal. `z: modal` for dialogs, `z: toast` (higher) for toasts. Input field is rendered outside the portal but is layered under the overlay.
- **Result delivery**: side effects + explicit close. Component mutates app state directly (`local.model.set(selected)`) and calls `dialog.close()`. No promise-based result return.
- **Toast vs dialog**: separate. `showToast({ variant, title, description })` is a different API for non-blocking notifications, never embedded in a dialog.
- **Nesting**: not supported (rationale: simpler focus management, but blocks confirm-inside-picker UX).

## Comparison

| Aspect             | Claude Code                          | Codex (Rust)                        | opencode                       |
| ------------------ | ------------------------------------ | ----------------------------------- | ------------------------------ |
| Trigger            | `local-jsx` discriminator on command | match arm in slash dispatch         | command `onSelect` callback    |
| Trait / API        | React component                      | `BottomPaneView` trait + view stack | imperative `dialog.show(node)` |
| Focus gating       | implicit (Ink scope)                 | stack presence                      | full-screen overlay + portal   |
| Nesting            | yes (React tree)                     | yes (Vec stack)                     | no (single-at-a-time)          |
| Result delivery    | callback (`onDone(value, opts)`)     | async event (`AppEvent::*`)         | side effects + `close()`       |
| Reusable primitive | none — each modal hand-rolled        | `ListSelectionView` (generic)       | Kobalte Dialog primitives      |
| Lazy load          | `load: () => import(...)`            | n/a (compiled binary)               | `void import(...).then()`      |
| Agent-triggered    | no first-class path                  | `try_consume_approval_request()`    | no first-class path            |

## Patterns Worth Borrowing for oxide-code

1. **Codex's `BottomPaneView` trait + view stack.** Idiomatic Rust+Ratatui shape — small trait, single owner, stack semantics give nesting for free without adding focus-flag bookkeeping. Direct port target.
2. **Codex's generic `ListSelectionView`.** A single picker primitive parameterized by an item trait is what `/model`, `/effort`, `/theme`, future `/agents`, and any approval prompt all want. Beats hand-rolling per command (Claude Code's path).
3. **Two open paths from day one.** Slash-triggered + agent-triggered. The trait shape is identical for both — only the call site differs. Codex already does this with `try_consume_approval_request`. Designing the trait for both up front means the future Permission feature drops in without re-shaping.
4. **Async event for completion, not callback.** Modal pushes a `UserAction` (or richer `ModalEvent`) onto the existing `user_tx` channel. Symmetric with how the rest of oxide-code's UI talks to the agent loop. Avoids the `Box<dyn FnOnce>` / lifetime gymnastics of a callback-based design.
5. **Lazy live-data getters on slash command metadata.** Claude Code's dynamic `description()` lets the popup show `(currently sonnet)` without a refresh hook. Cheap, useful, doesn't require the modal infrastructure.

## Patterns to Reject

1. **opencode's single-modal-only.** Saves ~30 lines of stack code; costs every confirm-inside-picker flow forever.
2. **Claude Code's per-command UI hand-rolling.** With ~50 modal commands they need either a shared kit or a lot of duplication. A generic picker primitive scales better. Build it before the second modal lands.
3. **opencode's imperative `dialog.show()` with side-effecting components.** State-mutation-as-result is hard to test in Rust. Prefer typed `ModalAction` returns.
4. **Codex's permanent `ChatComposer` behind every view.** Useful for "preserve typed text across modal dismiss" but adds layout complexity. Only worth it if oxide-code's typing-during-modal turns out to be common — defer.
