# Welcome Screen

The welcome screen paints in the chat region while the chat has no blocks, showing the app's identity, the session environment, and a short list of starter slash commands. It answers three questions: what is running (the name, version, active model and effort, and auth source), where the session runs, and what to do next (three sampled starter commands plus a randomized tip).

## Rendering

`ox_tui::components::welcome` is a stateless renderer, `paint`, that reads `&LiveSessionInfo`. `App::draw_frame` paints it into the chat region when:

- `chat.is_empty()` (no blocks, no streaming, no thinking buffer, no compaction summary), and
- `session_info.config.show_welcome` resolves to true (default: true).

`App` paints the welcome in place of the chat render. With the welcome off, an empty `ChatView` paints only the surface background. The "is the chat empty?" predicate stays where the data lives.

## Layout

At 80 cols and 14 rows with the test fixture (`claude-opus-5-5`, `xhigh` effort, OAuth), the render is:

```text
                           ━━━━ oxide-code v0.1.0 ━━━━

                     Claude Opus 5.5 · xhigh effort · OAuth
                     ~/github/oxide-code

                     Try one of:

                       /resume    Resume a previous session
                       /mode      Switch the permission mode
                       /model     Switch the model

                     Tip — drag chat text to select it
```

The starter rows sample the built-in commands a new session can run bare and show each one's registry description, and the tip samples `TIP_POOL`, skipping `PR_TIP` unless the configured status line includes the `pull-request` segment. Both draws seed `fastrand` with the xxh64 hash of the session ID, so a session keeps its picks while each new session draws again.

Sections: identity ribbon above a body column (env, cwd, starter list, tip). Body lines pad to one shared width so they keep one left edge under `Paragraph::alignment(Center)`. Without that pad, each line floats to its own visual center and the welcome reads as a "ransom note" stack. The ribbon centers as its own unit, and the body column centers on the same axis because every padded line has the same width.

### Width Ladder

| Cols  | Identity                    | Environment         | Cwd                         | Starter list | Tip hint   |
| ----- | --------------------------- | ------------------- | --------------------------- | ------------ | ---------- |
| ≥60   | ribbon `━━━━ oxide-code...` | full line           | tildified, full             | 3 rows       | yes        |
| 40-59 | wordmark `oxide-code v...`  | full line           | tildified, full             | 3 rows       | yes        |
| 25-39 | wordmark                    | model + effort only | tildified, center-truncated | hidden       | hidden     |
| <25   | suppressed                  | suppressed          | suppressed                  | suppressed   | suppressed |

Below 25 cols nothing paints, since the terminal is too narrow to read the welcome cleanly. The input field anchors the empty session instead. The chat region also needs 13 rows for the starter list and tip hint, so a shorter region hides them at any width and centers the environment and cwd lines on their own width.

### Theme Slots

`accent` (bold) paints the `oxide-code` wordmark. `accent` (non-bold) paints starter command names and the `Tip` label. `text` paints the environment line and tip body. `dim` paints the version, ribbon flanks, cwd, starter descriptions, starter header, and tip separator. No new theme slots are needed because the welcome reuses the palette `/status` already paints with.

## Design Decisions

1. **Empty-chat predicate in `ChatView`, welcome rendering in App.** `ChatView::is_empty()` remains the predicate, and `App::draw_frame` decides which renderer to invoke. The welcome is an ephemeral placeholder in the chat region. Pushing it into `ChatView::blocks` would mix onboarding UI with persisted transcript content and break the `is_empty` check.
2. **Stateless `paint(frame, area, theme, snapshot)` function.** The welcome owns no state across frames because it's a pure projection of `LiveSessionInfo`, and a struct would only invite caches and lifecycle hooks the welcome doesn't need.
3. **`paint` reads `LiveSessionInfo` directly.** The picks are a pure function of the session ID and the configured status line, so the welcome keeps nothing across frames, and a model, effort, or sign-in change shows on the next frame with no cached copy to refresh.
4. **Starter rows sample 3 runnable commands, plus a randomized tip.** Advertising every slash command defeats the point of "try one of". Sampled starters and the curated `TIP_POOL` add variety without becoming a dashboard, and every entry is an action a user can take next. Picks are seeded from `session_id`, so a session is stable while `/clear` yields a fresh pick.
5. **Starters come from the slash registry.** Every built-in a new session can run bare is a starter, shown with its registry description, so a new command joins the pool with no second description to drift. `NOT_STARTERS` in the welcome names the exceptions, since an `is_starter() -> bool` on `SlashCommand` would push welcome-specific curation into every command.
6. **Editorial ribbon `━━━━ oxide-code v{ver} ━━━━` with no ASCII mark and no animation.** A four-side box reads as generic CLI chrome, and an ASCII mascot (Claude Code) or animation (Codex) is outsized for the value. The ribbon is a single line that anchors horizontally without surrounding the wordmark, giving the welcome a typographic identity. The wordmark uses the project name `oxide-code`, the brand a migrator searches for, while `ox` stays the binary name.
7. **Coarse three-tier width ladder (full / collapsed / suppressed).** Codex and Claude Code lean on truncate-and-reflow, while opencode uses flex-shrink. For fixed content, a coarse ladder is simpler than per-element shrink rules.
8. **Body lines pad to one shared column width, while the ribbon centers independently.** `Paragraph::alignment(Center)` centers each line separately, so padding env / cwd / starters / tip to one width is what gives the body a shared left edge.
9. **`[tui] show_welcome = true` (default) + `OX_SHOW_WELCOME` env override.** Mirrors the existing `show_thinking` knob shape: TOML option + env, empty-is-absent. When false, the chat region is blank and the input field anchors the empty session.
10. **`/clear` re-shows the welcome automatically.** `/clear` clears `chat.blocks`, which restores `is_empty()` → welcome paints on next frame. No special re-emission path is needed.
11. **Resume never shows the welcome.** Resume populates `chat.blocks` from the stored transcript, so `is_empty()` returns false on first paint. No special-casing needed.

## Out of Scope

- **Live feeds.** Recent sessions, release notes, and upsells would make an onboarding dashboard with ongoing maintenance cost, though Claude Code ships them.
- **An ASCII mascot or animations.** Codex and Claude Code both ship them, but static text carries the value with less complexity.
- **Tab-into-the-welcome navigation.** The welcome is read-only, and the user types in the input below it.
- **Branch or git status.** It is orthogonal to onboarding.

## Sources

- `crates/ox-core/src/config.rs`: `show_welcome` field.
- `crates/ox-core/src/config/schema.rs`: `[tui] show_welcome` TOML option.
- `crates/ox-core/src/util/path.rs`: `tildify` (cwd display).
- `crates/ox-tui/src/app/render.rs`: `draw_frame` empty-chat branch, `show_welcome` gate.
- `crates/ox-tui/src/components/chat.rs`: `is_empty()` predicate.
- `crates/ox-tui/src/components/welcome.rs`: `paint`, `Picks`, `NOT_STARTERS`, `TIP_POOL`, `PR_TIP`, starter rows.
- `crates/ox-tui/src/session_info.rs`: `LiveSessionInfo` (snapshot input).
- `crates/ox-tui/src/slash/registry.rs`: `BUILT_INS`, the commands and descriptions the starters sample.
