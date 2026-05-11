# Welcome Screen

Empty-state surface that paints when the chat has no blocks: identity, session environment, and a small starter slash-command list.

## Goals

The minimal `Welcome to ox / Ask anything to begin.` banner gets users to the prompt but says little about the loaded agent. The richer surface answers three questions:

1. **What am I running?** Name, version, active model + effort, auth source.
2. **Where am I?** Working directory.
3. **What can I do?** Three sampled starter commands plus a randomized `Tip - ...` hint.

## Implementation

`tui/components/welcome` is a stateless renderer (`paint`) plus a small data snapshot (`WelcomeSnapshot`) derived from `&LiveSessionInfo`. Painted by `App::draw_frame` into the chat region when:

- `chat.is_empty()` (no blocks, no streaming, no thinking buffer), AND
- `session_info.config.show_welcome` resolves to true (default: true).

`ChatView` keeps its own empty-state branch but now returns an empty `Text` for that case, and `App` paints the welcome on top instead. The "is the chat empty?" predicate stays where the data lives.

## Layout

At 80 cols with the test fixture (`claude-opus-4-7` plain, `xhigh` effort, OAuth), the render is:

```text
                           ━━━━ oxide-code v0.1.0 ━━━━

                  Claude Opus 4.7 · xhigh effort · OAuth
                  ~/github/oxide-code

                  Try one of:

                    /help     list commands
                    /diff     show staged changes
                    /model    switch model

                  Tip — ox --continue resumes your last session
```

Starter rows and the tip are sampled per session from `STARTER_POOL` and `TIP_POOL`, so a different launch shows different picks. `[1m]` model ids render a trailing `(1M context)` suffix on the model line.

Sections: identity ribbon above a body column (env, cwd, starter list, trailer). Body lines pad to one shared width so they keep one left edge under `Paragraph::alignment(Center)`. Without that pad, each line floats to its own visual center and the welcome reads as a "ransom note" stack. The ribbon centers as its own unit, and the body column centers on the same axis because every padded line has the same width.

### Width ladder

| Cols  | Identity                    | Environment         | Cwd                         | Starter list | Trailer hint |
| ----- | --------------------------- | ------------------- | --------------------------- | ------------ | ------------ |
| ≥60   | ribbon `━━━━ oxide-code...` | full line           | tildified, full             | 3 rows       | yes          |
| 40-59 | wordmark `oxide-code v...`  | full line           | tildified, full             | 3 rows       | yes          |
| 25-39 | wordmark                    | model + effort only | tildified, center-truncated | hidden       | hidden       |
| <25   | suppressed                  | suppressed          | suppressed                  | suppressed   | suppressed   |

Below 25 cols nothing paints, since the terminal is too narrow to read the welcome cleanly. The input field anchors the empty session instead.

### Theme slots

`accent` (bold) paints the `oxide-code` wordmark. `accent` (non-bold) paints starter command names and the `Tip` label. `text` paints the environment line and tip body. `dim` paints the version, ribbon flanks, cwd, starter descriptions, starter header, and tip separator. No new theme slots are needed because the welcome reuses the palette `/status` already paints with.

## Design Decisions

1. **Empty-chat branch stays in `ChatView`, while rendering moves to App.** `ChatView::is_empty()` remains the predicate, and `App::draw_frame` decides which renderer to invoke. The welcome is an ephemeral placeholder in the chat region. Pushing it into `ChatView::blocks` would mix onboarding UI with persisted transcript content and break the `is_empty` check.
2. **Stateless `paint(frame, area, theme, snapshot)` function.** The welcome owns no state across frames because it's a pure projection of `LiveSessionInfo`, and a struct would only invite caches and lifecycle hooks the welcome doesn't need.
3. **`WelcomeSnapshot` projects only what the welcome needs.** Model display, effort, auth label, cwd, version, and starter rows. Keeping the shape narrow makes snapshot-test fixtures cheap and decouples the welcome from `LiveSessionInfo` evolution.
4. **Starter rows sample 3 from a curated pool, plus a randomized tip.** Advertising all eleven slash commands defeats the point of "try one of". The 9-entry `STARTER_POOL` and 9-entry `TIP_POOL` add variety without becoming a dashboard, and every entry is an action a user can take next. Picks are seeded from `session_id`, so a session is stable while `/clear` yields a fresh pick.
5. **Curated rows live alongside the welcome.** Adding `is_starter() -> bool` to `SlashCommand` would push welcome-specific curation into every command.
6. **Editorial ribbon `━━━━ oxide-code v{ver} ━━━━` with no ASCII mark and no animation.** A four-side box reads as generic CLI chrome, and an ASCII mascot (Claude Code) or animation (Codex) is outsized for the value. The ribbon is a single line that anchors horizontally without surrounding the wordmark, giving the welcome typographic identity rather than container chrome. The wordmark itself is `oxide-code` (project name, the brand a migrator searches for) rather than `ox` (binary command).
7. **Coarse three-tier width ladder (full / collapsed / suppressed).** Codex and Claude Code lean on truncate-and-reflow, while opencode uses flex-shrink. For fixed content, a coarse ladder is simpler than per-element shrink rules.
8. **Body lines pad to one shared column width, while the box centers independently.** `Paragraph::alignment(Center)` centers each line separately, so a naive layout gives every line its own indent. Padding env / cwd / starters / trailer to one width forces a shared left edge. The identity box stays centered above the body column.
9. **`[tui] show_welcome = true` (default) + `OX_SHOW_WELCOME` env override.** Mirrors the existing `show_thinking` knob shape: TOML option + env, empty-is-absent. When false, the chat region is blank and the input field anchors the empty session.
10. **`/clear` re-shows the welcome automatically.** `/clear` clears `chat.blocks`, which restores `is_empty()` → welcome paints on next frame. No special re-emission path needed (the opposite of Codex's history-cell shape).
11. **Resume never shows the welcome.** Resume populates `chat.blocks` from the JSONL transcript, so `is_empty()` returns false on first paint. No special-casing needed.

## Out of Scope / Deferred

- **Live feeds** (recent sessions, release notes, upsells). Claude Code ships them, but the result is an onboarding dashboard with ongoing maintenance cost.
- **ASCII mascot / animations**: Codex / Claude Code both ship them, but static text carries the value with less complexity.
- **Plugin hooks for welcome content**: opencode-style slot overrides should wait for a plugin system.
- **Tab-into-the-welcome navigation**: The welcome is read-only, and the user types in the input below it.
- **Branch / git status on welcome**: Orthogonal to onboarding. `/diff` and the trailer hint cover the "what changed" angle.

## Sources

- `crates/oxide-code/src/config.rs`: `show_welcome` field.
- `crates/oxide-code/src/config/file.rs`: `[tui] show_welcome` TOML option.
- `crates/oxide-code/src/slash/context.rs`: `LiveSessionInfo` (snapshot input).
- `crates/oxide-code/src/tui/app.rs`: `draw_frame` empty-chat branch, `show_welcome` gate.
- `crates/oxide-code/src/tui/components/chat.rs`: `is_empty()` predicate (existing).
- `crates/oxide-code/src/tui/components/welcome.rs`: `paint`, `WelcomeSnapshot`, starter rows.
- `crates/oxide-code/src/util/path.rs`: `tildify` (cwd display).
