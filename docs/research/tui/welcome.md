# Welcome Screen (Reference)

Research on the empty-state / first-paint surface across reference projects. Based on [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Claude Code (TypeScript + Ink)

Most elaborate of the three: a multi-panel dashboard at wide widths, single-column at narrow.

- **Components**: `src/components/LogoV2/` houses `WelcomeV2.tsx`, `LogoV2.tsx`, `Clawd.tsx`, `FeedColumn.tsx`. Rendered when `messages.length === 0` via `Messages.tsx:679` conditional `!hideLogo && !(renderRange && renderRange[0] > 0)`.

- **Content**: ASCII Clawd mascot (4 pose variants: default, look-left, look-right, arms-up) plus "Welcome to Claude Code" header plus version badge. Side panel paints up to 3-4 feed columns: recent activity (session prompt summaries with relative timestamps and a `/resume for more` footer), what's new (release notes), tips for getting started (onboarding checkmarks), conditional upsells (guest passes, overage credit).

- **Environment line**: model plus effort suffix (truncated to 30 chars), billing type (`API Usage` / `Claude.ai Subscriber + org`), tildified cwd with `@agent-name` prefix where applicable, username in greeting.

- **Data shape**: `getLogoDisplayData()` (`logoV2Utils.ts`) reads `MACRO.VERSION`, `getCwd() / displayPath()`, `billingType` (subscription state), `agentName` (from settings). `getRecentActivitySync()` enumerates session logs, while `checkForReleaseNotesSync()` checks the changelog. All read at render time from `AppState` plus initial settings.

- **Dismissal**: first user message hides the logo (the `messages.length === 0` predicate flips). On resume the logo reappears only if history is cleared.

- **Layout**: full-screen takeover. **Wide mode (>80 cols)**: left ASCII panel (~50 cols wide) plus right feed panel (responsive). **Compact mode (<80 cols)**: single-column centered box with border. **Condensed fallback**: when upsells / notices fire, the full logo collapses to a one-line banner.

- **Width adaptation**: 3-tier ladder. Model name and cwd center-truncate, and feeds auto-size to the longest column clamped to a panel max. No section is ever hidden, since text reflows instead.

- **Configurability**: `CLAUDE_CODE_FORCE_FULL_LOGO` env var only. No clean disable, though debug flags can suppress conditionally.

- **Starter affordances**: not a slash-command picker. The Tips feed lists onboarding actions ("Create CLAUDE.md", "Set effort level") as text. `/resume`, `/release-notes`, `/passes` appear as feed footers.

- **Notable**: Aggressive memoization (`OffscreenFreeze`, `React.memo` on `LogoHeader`) avoids cascading re-renders. The release-notes-seen counter advances `lastReleaseNotesSeen` config so the logo suppresses on later startups. Mascot poses are static art.

## OpenAI Codex (Rust + Ratatui)

Two-stage flow: an animated onboarding splash (auth gate) followed by a permanent session header pushed onto chat history. Architecturally closest to oxide-code.

- **Components**: onboarding `WelcomeWidget` (`codex-rs/tui/src/onboarding/welcome.rs:26-72`) gates the auth flow. `SessionHeaderHistoryCell` (`history_cell.rs:1250-1327`) is the relevant comparison, pushed as the first cell of every new session.

- **Onboarding splash**: identity ("Welcome to Codex, OpenAI's command-line coding agent") plus 36-frame animated ASCII art (`frames.rs`). Four art variants (default, codex, openai, blocks) rotate via Ctrl+dot / Ctrl+Shift+dot. Skipped when viewport < 60×37.

- **Session header**: boxed header with title (`>_ OpenAI Codex vX`), model + effort, working directory, YOLO-mode flag (magenta bold). Followed inline by a `PlainHistoryCell` listing 5 hardcoded starter commands: `/init`, `/status`, `/permissions`, `/model`, `/review`.

- **Data shape**: `SessionHeaderHistoryCell { model, reasoning_effort: Option<...>, directory: PathBuf, version: &'static str, yolo_mode: bool }`, populated from `ThreadSessionState + Config`.

- **Dismissal**: onboarding splash hides on `is_logged_in` (`StepState::Hidden`). The session header is permanent. It stays at the top of chat scroll for the entire session because it lives inside `HistoryCell` storage. The `is_first_event` flag (`chatwidget.rs:2082`) gates rendering of the starter-help cell only.

- **Layout**: history-cell. The header participates in normal block stacking, scrolls with the chat, and gets blank-line separators like any other cell. **Not** an empty-state overlay.

- **Width adaptation**: directory path uses `text_formatting::center_truncate_path` (`history_cell.rs:1448`) for semantic-boundary truncation. Animation skipped at narrow widths.

- **Configurability**: `animations_enabled` toggles the onboarding splash. `config.show_tooltips` suppresses the starter-help cell. No disable for the session header itself.

- **Starter affordances**: 5-row hardcoded list with static descriptions. No live data per row.

- **Notable**: storing the welcome inside chat history means `/clear` would have to explicitly re-emit it. The trade-off is automatic preservation in scroll-back. Animations are surprisingly elaborate for a Rust TUI: 4 art variants × 36 frames each.

## opencode (TypeScript + @opentui + Solid.js)

Most distinct from chat: a route-based full-screen Home with no chat-region inheritance.

- **Components**: `Home` route (`packages/opencode/src/cli/cmd/tui/routes/home.tsx:1-70`) renders the welcome itself. The `Session` route's footer (`session/footer.tsx:40-45`) shows a rotating `Get started /connect` hint when no providers are connected.

- **Content**: two-tone ASCII logo (left muted, right bold, with depth via `_`, `^`, `~` shadow markers in `logo.ts:82-83`). Centered above a prompt input. No version, cwd, model, or auth display on the welcome itself.

- **Data shape**: 3-by-3 placeholder hint tuple `{ normal, shell }` hardcoded in `Home`. Tips array is 60 curated entries in `tips-view.tsx:50-104` covering keybinds, slash commands, config, MCP. `Prompt` picks a random placeholder per session change.

- **Dismissal**: route transition (`home` → `session`) on first message. Implicit, no flag.

- **Layout**: full-screen flex, vertical stack (logo + prompt + tips). Prompt `maxWidth` clamped to 75 chars. Adapts to terminal height by collapsing top / bottom flex ratios.

- **Width adaptation**: `flexShrink` on tips, and the logo respects natural box-model wrapping. No explicit hide-section ladder.

- **Configurability**: none. Plugin hook slots (`home_logo`, `home_prompt`, `home_bottom`, `home_footer`) let extensions override individual sections.

- **Starter affordances**: 3 example prompts ("Fix a TODO in the codebase", "What is the tech stack of this project?", "Fix broken tests") plus 3 shell examples (`ls -la`, `git status`, `pwd`). Tips list 60 rotating contextual hints with inline `{highlight}` markup.

- **Notable**: tips rotate on every home load or session switch. The footer hint at the bottom of the Session route doubles as a passive welcome for un-configured users (the only welcome that survives the transition).

## Comparison

| Aspect            | Claude Code                  | Codex (Rust)                       | opencode                     |
| ----------------- | ---------------------------- | ---------------------------------- | ---------------------------- |
| Layout primitive  | full-screen takeover         | history-cell pushed at start       | full-screen route            |
| Dismiss trigger   | `messages.length === 0` flip | header permanent, help cell on 1st | route change                 |
| Re-show on /clear | yes (if history clears)      | no (would need explicit re-emit)   | yes (route returns)          |
| Identity          | mascot + name + version      | name + version (boxed)             | ASCII logo only              |
| Environment       | model + billing + cwd + user | model + effort + cwd + YOLO        | none                         |
| Starters          | tips text, no picker         | 5 hardcoded slash commands         | 3 example prompts + 60 tips  |
| ASCII art         | mascot, 4 static poses       | 36-frame animated, 4 variants      | two-tone block-char logo     |
| Width modes       | 3-tier ladder                | center-truncate paths              | flex shrink + maxWidth clamp |
| Configurability   | env var only                 | two toggles                        | none                         |
| Live data         | recent sessions, releases    | none                               | rotating tips                |

## Patterns Worth Borrowing for oxide-code

1. **Codex's session-header content shape**: name + version (boxed), model + effort, cwd. Compact and informational, and matches what oxide-code's `/status` modal already aggregates from `LiveSessionInfo`. Lift the same formatter.

2. **Codex's small starter command list (3-5 rows)**: concrete, hardcoded, name plus one-line description. Discoverable without becoming a dashboard.

3. **opencode's example prompts framing**: "Try one of:" reads more inviting than "Available commands:". Worth borrowing the framing even if the content is slash commands rather than free-form prompts.

4. **Claude Code's two-mode width ladder**: full layout at typical widths, single-line collapse at narrow. The 3-tier ladder is overkill, but a 2-tier (full / collapsed) is the right discipline for a static welcome.

5. **opencode's footer-as-passive-welcome**: when the welcome dismisses, a thin always-on hint persists in the input footer. oxide-code's status bar already plays this role for the active model + cwd, so the welcome dismisses cleanly.

## Patterns to Reject

1. **Claude Code's feed panels**. Recent-sessions / release-notes / upsells turn onboarding into a dashboard. Maintenance cost is high (each feed is a separate live data source) and the value to a returning user is near-zero.

2. **Claude Code's mascot + Codex's animated frames**. ASCII art at this scale is outsized for the value. Static text reads as professional, and animations add complexity without payoff. The plan's earlier open-question on ASCII art lands "no" in light of this.

3. **Codex's welcome-as-history-cell shape**. Conflates ephemeral onboarding UI with conversation transcript. Forces explicit re-emission on `/clear` and bloats the JSONL persisted record. Empty-state branching in the chat view is the right model.

4. **opencode's 60-tip rotating array**. Maintenance cost on the array, and rotating content makes the welcome feel "noisy" rather than "deliberate." A static curated 3-row list wins.

5. **Claude Code's full-screen takeover**. Visually heavy and consumes vertical space the user wants for chat. The welcome should occupy the empty chat region.
