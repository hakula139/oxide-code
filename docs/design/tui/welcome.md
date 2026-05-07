# Welcome Screen

Empty-state surface that paints when the chat has no blocks: identity, session environment, and a small starter slash-command list.

## Goals

The minimal `Welcome to ox / Ask anything to begin.` banner gets users to the prompt but signals nothing about the agent that's actually loaded. The richer surface should answer three questions a fresh terminal user has at a glance:

1. **What am I running?** — name, version, active model + effort, auth source.
2. **Where am I?** — working directory.
3. **What can I do?** — 3 starter slash commands plus a `Or type / to browse...` hint.

## Implementation

[`crates/oxide-code/src/tui/components/welcome.rs`](../../../crates/oxide-code/src/tui/components/welcome.rs) — a stateless renderer (`paint`) plus a small data snapshot (`WelcomeSnapshot`) derived from `&LiveSessionInfo`. Painted by `App::draw_frame` into the chat region when:

- `chat.is_empty()` (no blocks, no streaming, no thinking buffer), AND
- `session_info.config.show_welcome` resolves to true (default: true).

`ChatView` keeps its own empty-state branch — but now it returns an empty `Text` for that case, and `App` paints the welcome on top instead. The "is the chat empty?" predicate stays where the data lives.

## Layout

```text
                            ┌───────────────────┐
                            │     ox v0.1.0     │
                            └───────────────────┘

    Claude Opus 4.7 (1M) · xhigh effort · OAuth (Claude Code)
                       ~/github/oxide-code

                          Try one of:
                            /help        list commands
                            /init        author or update AGENTS.md
                            /diff        show staged changes

                  Or type / to browse all commands.
```

Sections: identity (boxed) → environment line → cwd → starter list → trailer hint. All centered. The chat area's full width.

### Width ladder

| Cols  | Identity             | Environment         | Cwd                         | Starter list | Trailer hint |
| ----- | -------------------- | ------------------- | --------------------------- | ------------ | ------------ |
| ≥60   | boxed `ox v{ver}`    | full line           | tildified, full             | 3 rows       | yes          |
| 40-59 | one-line `ox v{ver}` | full line           | tildified, full             | 3 rows       | yes          |
| 25-39 | one-line             | model + effort only | tildified, center-truncated | hidden       | hidden       |
| <25   | suppressed           | suppressed          | suppressed                  | suppressed   | suppressed   |

Below 25 cols nothing paints — the terminal is too narrow to read the welcome cleanly; let the input field anchor the empty session.

### Theme slots

`accent` for the `ox v{ver}` token. `text` for environment + starter command names. `dim` for descriptions, the trailer hint, and the box border. No new theme slots — reuses the palette `/status` already paints with.

## Design Decisions

1. **Empty-chat branch stays in `ChatView`; rendering moves to App.** `ChatView::is_empty()` is the predicate; `App::draw_frame` reads it and decides which renderer to invoke. Welcome is not transcript content — it's a placeholder painted in the chat region. Pushing it into `ChatView::blocks` would conflate ephemeral onboarding with persisted conversation state and break the `is_empty` check itself.
2. **Stateless `paint(frame, area, theme, snapshot)` function, not a `Welcome` struct.** The welcome owns no state across frames — it's a pure projection of `LiveSessionInfo`. A struct would invite caches and lifecycle hooks the welcome doesn't need.
3. **`WelcomeSnapshot` is a small projection, not the full `LiveSessionInfo`.** Keeping the shape narrow (model display, effort, auth label, cwd, version, starter rows) lets snapshot tests build fixtures cheaply and decouples the welcome from `LiveSessionInfo` evolution.
4. **Starter rows source from a curated 3-tuple, not all of `BUILT_INS`.** The full registry has nine entries; advertising all of them defeats the point of "try one of." The three rows are `/help`, `/init`, `/diff` — discovery, onboarding, repo-state. The trailer hint (`Or type / to browse all commands.`) covers everything else.
5. **Curated rows live alongside the welcome (in `welcome.rs`), not as a method on `SlashCommand`.** Adding `is_starter() -> bool` to the trait would push welcome-specific concern into every command. The welcome's curation is the welcome's responsibility.
6. **Static text logo (`ox v{ver}` boxed); no ASCII mark, no animation.** Reference projects span no-art (Codex header) → static mascot (Claude Code) → two-tone ASCII (opencode). All three came across as outsized lift for the value; static box reads as professional and is theme-agnostic.
7. **Two-tier width ladder, not three.** Codex / Claude Code lean on truncate-and-reflow; opencode leans on flex-shrink. For a fixed-content welcome a coarse ladder (full / collapsed / suppressed) is simpler than tuning per-element shrink behavior.
8. **`[tui] show_welcome = true` (default) + `OX_SHOW_WELCOME` env override.** Mirrors the existing `show_thinking` knob shape (TOML option + env, empty-is-absent). When false, the chat region is blank — the input field anchors the empty session.
9. **`/clear` re-shows the welcome automatically.** `/clear` clears `chat.blocks`, which restores `is_empty()` → welcome paints on next frame. No special re-emission path needed (the opposite of Codex's history-cell shape).
10. **Resume never shows the welcome.** Resume populates `chat.blocks` from the JSONL transcript, so `is_empty()` returns false on first paint. No special-casing needed.

## Out of Scope / Deferred

- **Live feeds** (recent sessions, release notes, upsells) — Claude Code does this; turns onboarding into a dashboard with a maintenance cost. The static welcome stays static.
- **Rotating tips** — opencode's 60-tip array. Static curated content wins for predictability.
- **ASCII mascot / animations** — Codex / Claude Code both ship them. Outsized for the value; static text reads as deliberate.
- **Plugin hooks for welcome content** — opencode-style slot overrides. Not until there's a plugin system.
- **Tab-into-the-welcome navigation** — the welcome is read-only; the user types in the input below it. No focus model.
- **Branch / git status on welcome** — orthogonal; `/diff` and the trailer hint cover the "what changed" angle.

## Sources

- `crates/oxide-code/src/tui/components/welcome.rs` — `paint`, `WelcomeSnapshot`, starter rows.
- `crates/oxide-code/src/tui/components/chat.rs` — `is_empty()` predicate (existing).
- `crates/oxide-code/src/tui/app.rs` — `draw_frame` empty-chat branch, `show_welcome` gate.
- `crates/oxide-code/src/slash/context.rs` — `LiveSessionInfo` (snapshot input).
- `crates/oxide-code/src/config.rs` — `show_welcome` field.
- `crates/oxide-code/src/config/file.rs` — `[tui] show_welcome` TOML option.
- `crates/oxide-code/src/util/path.rs` — `tildify` (cwd display).
