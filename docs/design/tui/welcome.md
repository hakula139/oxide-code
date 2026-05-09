# Welcome Screen

Empty-state surface that paints when the chat has no blocks: identity, session environment, and a small starter slash-command list.

## Goals

The minimal `Welcome to ox / Ask anything to begin.` banner gets users to the prompt but signals nothing about the agent that's actually loaded. The richer surface should answer three questions a fresh terminal user has at a glance:

1. **What am I running?** — name, version, active model + effort, auth source.
2. **Where am I?** — working directory.
3. **What can I do?** — 3 starter slash commands sampled from a curated pool, plus a randomized `Tip — ...` hint.

## Implementation

`tui/components/welcome` is a stateless renderer (`paint`) plus a small data snapshot (`WelcomeSnapshot`) derived from `&LiveSessionInfo`. Painted by `App::draw_frame` into the chat region when:

- `chat.is_empty()` (no blocks, no streaming, no thinking buffer), AND
- `session_info.config.show_welcome` resolves to true (default: true).

`ChatView` keeps its own empty-state branch but now returns an empty `Text` for that case, and `App` paints the welcome on top instead. The "is the chat empty?" predicate stays where the data lives.

## Layout

```text
                          ━━━━ oxide-code v0.1.0 ━━━━

                     Claude Opus 4.7 (1M context) · xhigh effort · OAuth
                     ~/github/oxide-code

                     Try one of:

                       /help    list commands
                       /init    author or update AGENTS.md
                       /diff    show staged changes

                     Tip — press / to browse all commands
```

Sections: identity ribbon (single line, centered) → body column (env / cwd / starter list / trailer). Body lines pad to a single shared column width so they all share one left edge under `Paragraph::alignment(Center)` — without that pad each line floats to its own visual center and the welcome reads as a "ransom note" stack. The ribbon centers as its own unit above the body column.

### Width ladder

| Cols  | Identity                  | Environment         | Cwd                         | Starter list | Trailer hint |
| ----- | ------------------------- | ------------------- | --------------------------- | ------------ | ------------ |
| ≥60   | ribbon `━━━━ oxide-code…` | full line           | tildified, full             | 3 rows       | yes          |
| 40-59 | wordmark `oxide-code v…`  | full line           | tildified, full             | 3 rows       | yes          |
| 25-39 | wordmark                  | model + effort only | tildified, center-truncated | hidden       | hidden       |
| <25   | suppressed                | suppressed          | suppressed                  | suppressed   | suppressed   |

Below 25 cols nothing paints — the terminal is too narrow to read the welcome cleanly; let the input field anchor the empty session.

### Theme slots

`accent` (bold) for the `oxide-code` wordmark, `accent` (non-bold) for starter command names and the `Tip` label. `text` for the environment line and tip body. `dim` for the version, ribbon flanks, cwd, starter descriptions, starter header, and the tip's em-dash separator. No new theme slots — reuses the palette `/status` already paints with.

## Design Decisions

1. **Empty-chat branch stays in `ChatView`; rendering moves to App.** `ChatView::is_empty()` is the predicate, and `App::draw_frame` reads it to decide which renderer to invoke. Welcome is a placeholder painted in the chat region rather than transcript content; pushing it into `ChatView::blocks` would conflate ephemeral onboarding with persisted conversation state and break the `is_empty` check itself.
2. **Stateless `paint(frame, area, theme, snapshot)` function.** The welcome owns no state across frames because it's a pure projection of `LiveSessionInfo`, and a struct would only invite caches and lifecycle hooks the welcome doesn't need.
3. **`WelcomeSnapshot` projects only what the welcome needs.** Model display, effort, auth label, cwd, version, and starter rows. Keeping the shape narrow makes snapshot-test fixtures cheap and decouples the welcome from `LiveSessionInfo` evolution.
4. **Starter rows sample 3 from a curated pool, plus a randomized tip.** Advertising all eleven slash commands defeats the point of "try one of"; the 9-entry `STARTER_POOL` and 9-entry `TIP_POOL` give the surface variety per launch without becoming a tip dashboard, and every entry is a concrete action a user can take next. Picks are seeded from `session_id`, so a session always shows the same surface but `/clear` (which rolls the session) yields a fresh pick.
5. **Curated rows live alongside the welcome.** Adding `is_starter() -> bool` to `SlashCommand` would push a welcome-specific concern into every command; the welcome's curation is the welcome's responsibility.
6. **Editorial ribbon `━━━━ oxide-code v{ver} ━━━━` with no ASCII mark and no animation.** A four-side box reads as generic CLI chrome, and an ASCII mascot (Claude Code) or animation (Codex) is outsized for the value. The ribbon is a single line that anchors horizontally without surrounding the wordmark, giving the welcome typographic identity rather than container chrome. The wordmark itself is `oxide-code` (project name, the brand a migrator searches for) rather than `ox` (binary command).
7. **Coarse three-tier width ladder (full / collapsed / suppressed).** Codex and Claude Code lean on truncate-and-reflow, opencode on flex-shrink; for a fixed-content welcome a coarse ladder is simpler than tuning per-element shrink behavior.
8. **Body lines pad to one shared column width; box centers independently.** `Paragraph::alignment(Center)` aligns each line on its own visual center, so a naive layout has every line floating to its own indent ("ransom note"). Padding env / cwd / starters / trailer to one common width forces a single shared left edge. The identity box stays centered as its own unit, anchoring the screen above the body column.
9. **`[tui] show_welcome = true` (default) + `OX_SHOW_WELCOME` env override.** Mirrors the existing `show_thinking` knob shape (TOML option + env, empty-is-absent); when false, the chat region is blank and the input field anchors the empty session.
10. **`/clear` re-shows the welcome automatically.** `/clear` clears `chat.blocks`, which restores `is_empty()` → welcome paints on next frame. No special re-emission path needed (the opposite of Codex's history-cell shape).
11. **Resume never shows the welcome.** Resume populates `chat.blocks` from the JSONL transcript, so `is_empty()` returns false on first paint. No special-casing needed.

## Out of Scope / Deferred

- **Live feeds** (recent sessions, release notes, upsells). Claude Code ships them, but the result is an onboarding dashboard with ongoing maintenance cost.
- **ASCII mascot / animations** — Codex / Claude Code both ship them. Outsized for the value; static text reads as deliberate.
- **Plugin hooks for welcome content** — opencode-style slot overrides. Not until there's a plugin system.
- **Tab-into-the-welcome navigation** — the welcome is read-only; the user types in the input below it. No focus model.
- **Branch / git status on welcome** — orthogonal; `/diff` and the trailer hint cover the "what changed" angle.

## Sources

- `crates/oxide-code/src/config.rs` — `show_welcome` field.
- `crates/oxide-code/src/config/file.rs` — `[tui] show_welcome` TOML option.
- `crates/oxide-code/src/slash/context.rs` — `LiveSessionInfo` (snapshot input).
- `crates/oxide-code/src/tui/app.rs` — `draw_frame` empty-chat branch, `show_welcome` gate.
- `crates/oxide-code/src/tui/components/chat.rs` — `is_empty()` predicate (existing).
- `crates/oxide-code/src/tui/components/welcome.rs` — `paint`, `WelcomeSnapshot`, starter rows.
- `crates/oxide-code/src/util/path.rs` — `tildify` (cwd display).
