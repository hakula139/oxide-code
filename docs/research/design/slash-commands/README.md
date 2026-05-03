# Slash Commands

Research notes on how reference codebases let users invoke local commands inline — `/help`, `/clear`, `/model`, `/status`, and friends — without sending the line to the model. The shared problem: without a client-side command surface, there is no way to introspect the session (token usage, model, cwd), no way to switch model or theme mid-session, no way to clear or fork a transcript, and the input area has only two outputs (submit-as-prompt, quit). Reference projects diverge on three axes — registry shape (declarative metadata vs. enum), execution model (function tables vs. one big `match`), and how command output lands (synthetic message vs. modal / toast). Based on analysis of [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Reference Implementations

### Claude Code (TypeScript)

Declarative registry with three execution modes; lazy-loaded implementations to keep startup cheap.

**Registry.** `claude-code/src/commands.ts:258-346` — memoized `COMMANDS()` returns a vector of `Command` records (~24 built-ins: `clear`, `compact`, `config`, `cost`, `help`, `ide`, `keybindings`, `login`, `logout`, `memory`, `model`, `mcp`, `resume`, `session`, `skills`, `status`, `theme`, `vim`, `branch`, `agents`, `export`, `plan`, `tasks`, `hooks`). Each record carries metadata (`claude-code/src/types/command.ts:175-206`): `name`, `aliases`, `description`, `argumentHint`, `type: 'local' | 'local-jsx' | 'prompt'`, `isEnabled?()`, `isHidden?`, `immediate?`, `isSensitive?`, `userInvocable?`, `availability?: ['claude-ai' | 'console']`, `whenToUse`, `version`, `kind`, `loadedFrom`.

**Parser.** `claude-code/src/utils/slashCommandParsing.ts:25-60` splits on whitespace; first word is name, rest is args. Unknown names round-trip through Fuse.js (`claude-code/src/utils/suggestions/commandSuggestions.ts:53-78`, threshold 0.3, name weight 3) — no fuzzy auto-correction, just an "unknown" error.

**Dispatch.** Three execution modes (`processSlashCommand.tsx:525-650`). `local` returns `{ resultText, displayMode }`. `local-jsx` returns React JSX rendered into the terminal — the modal pickers (`/model`, `/resume`, `/config`, `/status`) live here. `prompt` expands to text and submits to the model as a normal user message (custom skills).

**Output.** Display mode selects between `'skip'` (no transcript entry), `'system'` (synthetic local-stdout message via `createCommandInputMessage()`), and `'user'` (default — user message + result message in transcript). Meta flag (`isMeta: true`) keeps a message model-visible while hiding it from the user UI.

**Autocomplete.** `PromptInput.tsx:1130` shows the popup when the buffer starts with `/` and `hasCommandArgs` says no args yet. `commandSuggestions.ts` runs Fuse.js across name, aliases, name parts (split on `_-:`), and descriptions (weights: name 3, alias / part 2, description 0.5), then re-sorts by tier — exact-name → exact-alias → prefix-name → prefix-alias → fuzzy — with shorter-name preferred among prefix matches. Empty query skips Fuse and emits a categorized list (recently-used skills, then built-in / user / project / policy / other, alpha within each). Max 5 items rendered; rows paint plain text (`PromptInputFooterSuggestions.tsx` — no matched-char emphasis). Aliases parenthesize only the alias the user typed (`findMatchedAlias`), never the full list.

**Custom commands.** Markdown files in `~/.claude/skills/`, `~/.claude/commands/`, `./.claude/skills/`, `./.claude/commands/` with YAML frontmatter (`description`, `argument-hint`, `when-to-use`, `allowed-tools`, `model`, `user-invocable`, `hooks`, `context`, `agent`, `paths`, `shell`). Discovered on demand by `getSkillDirCommands()` (`loadSkillsDir.ts:405-450`).

**State persistence.** `~/.claude.json` is a single mega-file the tool continuously writes — telemetry, recent-file lists, per-project state, login metadata, and runtime preferences all live in one place. The CLI re-serialises it on many state changes, including some triggered by slash commands. Notable failure modes: a corrupt write erases user preferences; the file is unfriendly to declarative-config systems (NixOS, dotfile-managed homes) because the tool both reads from and writes to the same path.

**Specific commands.**

- `/clear` (`commands/clear/clear.ts:1-8`) — aliases `reset`, `new`; `clearConversation()` resets session state and frees context.
- `/compact` (`commands/compact/compact.ts:40-60`) — calls the model to summarize; optional custom summarization instructions in args.
- `/model` (`commands/model/index.ts:1-17`) — `local-jsx` modal picker; mid-session swap.
- `/config` (`commands/config/index.ts:1-10`) — Settings Ink modal; viewer + editor tabs.
- `/help` (`commands/help/help.tsx:1-10`) — generated from the registry via the `HelpV2` component.
- `/status` (`commands/status/status.tsx:1-10`) — Settings modal on the Status tab; version, model, account, API connectivity, tool status.
- `/cost` (`commands/cost/index.ts:1-23`) — session cost + duration; hidden from subscribers unless `USER_TYPE=ant`.
- `/login`, `/logout` (`commands/login/index.ts:1-14`, `commands/logout/index.ts:1-10`) — `local-jsx` OAuth flows; available only on first-party auth.
- `/resume` (`commands/resume/resume.tsx:1-80`) — `local-jsx` modal picker; calls `context.resume()` with a `ResumeEntrypoint` tag (parallel to `--continue`).

### OpenAI Codex (Rust)

Single big enum with per-variant methods; ~50 variants spanning real commands (`Model`, `Compact`, `Status`), experimental ones (`Realtime`, `Plan`), and debug-only (`MemoryDrop`).

**Registry.** `codex-rs/tui/src/slash_command.rs:1-60` — strum-derived `enum SlashCommand` with `EnumString`, `EnumIter`, `AsRefStr`, `IntoStaticStr`, `serialize_all = "kebab-case"`. The file-level comment is explicit: _"DO NOT ALPHA-SORT! Enum order is presentation order in the popup, so more frequently used commands should be listed first."_

**Per-variant metadata** (`slash_command.rs:60-220`) lives as methods, not a struct. `description() -> &'static str` is the popup blurb; `command() -> &'static str` is the kebab-case name (auto via `IntoStaticStr`); `supports_inline_args() -> bool` flags the ~9 variants that take args; `available_in_side_conversation() -> bool` whitelists `Copy`, `Diff`, `Mention`, `Status`; `available_during_task() -> bool` gates the popup mid-turn; `is_visible() -> bool` does platform / debug filtering (`cfg!(target_os = "windows")`, `cfg!(debug_assertions)`). `built_in_slash_commands()` enumerates `SlashCommand::iter()` and filters by `is_visible()`.

**Parser + autocomplete.** `ChatComposer::sync_command_popup` checks `looks_like_slash_prefix` and activates `CommandPopup` when the buffer starts with `/`. Submit dispatches through `try_dispatch_bare_slash_command` (no args) or `try_dispatch_slash_command_with_args`, returning `InputResult::Command(SlashCommand)` or `InputResult::CommandWithArgs(SlashCommand, String)`.

**Dispatch.** `ChatWidget::dispatch_command` is one big `match` on the enum variant, emitting `AppEvent::ClearUi`, `NewSession`, `compact()`, `OpenResumePicker`, `ResumeSessionByIdOrName`, etc. Some commands open a modal popup (`Model`, `Theme`); others spawn async tasks (`Diff` shells out to `git diff`); others just submit a fixed prompt (`Init`).

**Output.** Synthetic `history_cell`s appended to the transcript: `add_status_output`, `add_diff_in_progress`, `add_mcp_output`. No modal toasts — output threads through the same vertical chat list as model responses, which keeps scrollback uniform.

**No custom commands.** Built-in only. The enum is the registry.

**Specific commands.**

- `/clear` — `AppEvent::ClearUi`, clears the screen and starts a fresh chat.
- `/new` — `AppEvent::NewSession`.
- `/compact` — `app_event_tx.compact()`; summarizes via the model.
- `/model` — `Model` opens a model selection popup; mid-session swap.
- `/status` — `add_status_output()` paints a history cell with config + token usage.
- `/quit`, `/exit` — `request_quit_without_confirmation()`.
- `/resume` — `AppEvent::OpenResumePicker` (no-arg) or `ResumeSessionByIdOrName` (with arg).
- `/diff` — async `git diff` (including untracked).
- `/init` — submits a fixed prompt asking the model to create an `AGENTS.md` for the project.
- `/mcp` — `add_mcp_output()`, lists configured MCP tools.

### opencode (TypeScript)

Slim client-side `CommandOption` records; server-defined custom commands; toast / dialog output instead of synthetic transcript entries.

**Registry.** `opencode/packages/app/src/pages/session/use-session-commands.tsx:35-575` — `useSessionCommands` hook returns `CommandOption[]`. Each option (`command.tsx:75-86`) has `id`, `title`, `description`, `category`, `keybind`, `slash`, `onSelect`, `disabled`. ~12 built-ins — narrow, action-oriented surface: `/new`, `/undo`, `/redo`, `/compact`, `/fork`, `/share`, `/unshare`, `/open`, `/terminal`, `/model`, `/mcp`, `/agent`. Notably absent: `/help`, `/status`, `/cost`, `/config`, `/login`, `/logout`.

**Parser.** `prompt-input.tsx:872-918`'s `handleInput()` matches `^\/(\S*)$` (line start only, not mid-line), and only when `store.mode === "normal"`. Two other prefixes share the input: `@` for file mentions, `!` for shell mode.

**Dispatch.** Built-in commands are client-side closures over component state (dialogs, navigation, SDK calls). User-defined commands are server-side: detected via `sync.data.command` array (SDK type), routed through `client.session.command()` POST.

**Output.** `showToast({ title, description, variant })` for notifications, `dialog.show()` for pickers (model selection, file picker), `client.session.command()` results land as synthetic messages in the transcript (`submit.ts:118-126`). No "system message" block kind.

**Autocomplete.** `slash-popover.tsx:38-141`. Filtered list via `useFilteredList<SlashCommand>` (`prompt-input.tsx:659-670`); max 10 items, dismissed on Escape, click-elsewhere, or selection. Custom commands flagged with `skill` / `mcp` / `custom` source badges.

**Custom commands.** Server-defined, not file-based. The server publishes a `Command` array (`types.gen.ts:2009-2018`) carrying `name`, `description`, `source`, `template`, `hints`, `agent`, `model`. The client merges builtin + custom into one popover list.

## Comparison

| Repo        | Registry shape                                  | Variants | Per-command metadata                                                | Parser site            | Dispatch                                       | Output target                       | Autocomplete         | Custom commands                         |
| ----------- | ----------------------------------------------- | -------- | ------------------------------------------------------------------- | ---------------------- | ---------------------------------------------- | ----------------------------------- | -------------------- | --------------------------------------- |
| Claude Code | declarative records (`Command[]`), lazy modules | ~24      | name, aliases, type, hidden, sensitive, available, kind, loadedFrom | submit handler         | three modes (`local` / `local-jsx` / `prompt`) | synthetic messages w/ display modes | Fuse.js fuzzy, max 5 | yes — markdown + frontmatter, four dirs |
| Codex       | strum `enum SlashCommand` + impl methods        | ~50      | description, inline args, side-conversation, during-task, visible   | input layer (composer) | one big `match` on variant                     | synthetic `history_cell`            | popup, enum order    | no                                      |
| opencode    | `CommandOption[]` from React hook               | ~12      | id, title, description, category, keybind, disabled, source         | input layer (regex)    | onSelect closures + server route               | toast / dialog / synthetic message  | filtered, max 10     | yes — server-published, not file-based  |
| oxide-code  | trait + `&[&dyn SlashCommand]` slice            | 7        | name, aliases, description, `is_read_only`, optional usage hint     | `apply_action_locally` | `SlashOutcome` returned by `execute`           | `SystemMessageBlock` / `ErrorBlock` | tier-ranked filter   | not yet — plugin namespace reserved     |

## oxide-code Today

Seven built-ins ship: `/clear`, `/config`, `/diff`, `/help`, `/init`, `/model`, `/status`. Each lives in its own `slash/<name>.rs` file implementing `SlashCommand`. Adding one is a new file plus an entry in `BUILT_INS` (alphabetical). The autocomplete popup, `//foo` literal-escape, and the busy-turn dispatch policy (read-only fast-path; mutators / prompt-submit refuse with a system message) all sit on the cross-command surface — see decisions 1–12 below for the contracts each command rides on.

`/clear` rolls the session UUID and clears chat + file tracker. `/model` swaps the active model mid-session via `Client::set_model` (re-clamping `config.effort`) and refreshes the status bar + `session_info`. `/init` is the only prompt-submit command — synthesizes a fixed `AGENTS.md` / `CLAUDE.md` author-or-update prompt and forwards it to the agent loop. The remaining four are read-only.

Still missing (will land with their respective commands): per-session token tracking (`wire::Usage` parses but drops tokens — `/cost` has no data to show), and `AgentEvent::PromptRequest` plumbing (blocks `/init`'s richer multi-phase flow and any `local-jsx`-style modal commands).

## Design Decisions for oxide-code

1. **Trait registry, not enum.** `trait SlashCommand` mirrors `tool::Tool`; one file per command, registered via a `&[&dyn SlashCommand]` slice. Codex's giant `match` is rejected — adding `/foo` would mean editing two files.
2. **Parse at submit, not in `InputArea`.** `App::dispatch_user_action` runs `parse_slash` first, then dispatches locally or forwards to the agent. Keeps the input component dumb.
3. **One synthetic block kind: `SystemMessageBlock`.** New `ChatBlock` impl with a left-bar in `accent`. Errors reuse `ErrorBlock`. Codex's `add_*_output` is the precedent; opencode's toast / dialog split is rejected.
4. **Two-column popup, plain rows.** Left column is the canonical name; right column is the description on a fixed gutter. No matched-char emphasis — none of the three references actually ship it. Aliases parenthesize only the alias the user typed (`/clear (new)`), never the full list. Filter ranks name-prefix > alias-prefix > name-substring > alias-substring, alphabetical within each tier; empty query renders the registry in declared (alphabetical) order, so every popup state is alphabetical. Names accept `:` and `.` so a future `/plugin:cmd` namespace rides on top without a parser rewrite. No Fuse.js — the surface is small enough that an explicit tier ladder is more predictable than a fuzzy score.
5. **Mid-session model swap via `&mut Client` in the agent loop.** The agent loop owns `Client` by value (the same shape `/clear` uses for `set_session_id`), so `/model` returns `Action(UserAction::SwitchModel(id))` and the `SwitchModel` arm calls `client.set_model(id)`. Per-request paths re-read `&self.config.model` every call, so betas / `output_config` / `context_management` pick up the swap on the next stream — no `Arc<RwLock<...>>` needed. The trait does grow `is_read_only(&self, args: &str)` so bare `/model` (a list view) can dispatch mid-turn while the arg-bearing form refuses; default impl ignores `args` and reports read-only, so unrelated commands stay one-line impls.
6. **Slash commands never write user config files.** `/model` and `/theme` mutate session-only state; restart returns to user-declared `~/.config/ox/config.toml` / `./ox.toml`. Deliberate rejection of Claude Code's `~/.claude.json` (silent mega-file writes break NixOS-style declarative homes and risk corrupting user preferences). Tool-owned state stays under `$XDG_DATA_HOME/ox/` / `$XDG_STATE_HOME/ox/`.
7. **Aliases resolve to the canonical command but display by surface.** `/clear` is canonical; `/new` and `/reset` are aliases that route to the same impl. `/help` lists every alias inline (`/clear (new, reset)`) so the page reads as documentation; the popup shows only the alias the user typed (`/clear` alone, or `/clear (new)` after typing `new`) so the row stays a clean confirmation that the alias resolved. Filter searches name + aliases; submitting an alias invokes the canonical impl.
8. **No `/quit` or `/exit` in v1.** Claude Code has neither. Ctrl+C×2 / Ctrl+D already exit oxide-code; the slash variants would be redundant.
9. **`/config` is read-only in v1.** Prints the resolved effective config + layered file paths. An interactive editor lands later behind a writable-path check.
10. **Built-in only in v1.** The trait registry leaves room for `~/.config/ox/commands/*.md` discovery later.
11. **Read-only commands fast-path the busy turn.** `SlashCommand::is_read_only` defaults to `true`; the dispatcher runs read-only commands client-side even when input is disabled. State-mutating commands override to `false` and refuse mid-turn with a system message — queueing them through the prompt buffer would persist them as user messages and forward to the LLM.
12. **Two command kinds, one trait return: `SlashOutcome { Local, Action(UserAction) }`.** `Local` covers read-only commands (`/config`, `/diff`, `/help`, `/status`) that finish via `ctx`. `Action(_)` is the state-mutating kind: the dispatcher hands the `UserAction` back to the App, which forwards it to the agent loop. `/init` returns `Action(UserAction::SubmitPrompt(body))` — the App flips turn-start UI state (input disabled, status `Streaming`) before forwarding. `/clear` returns `Action(UserAction::Clear)` and forwards directly. Slash impls never reach into `user_tx`; the trait is the only seam.

Per-command design lives alongside this doc in the same directory as commands earn the depth: see [/clear](clear.md), [/init](init.md), [/model](model.md). Simple read-only commands (`/config`, `/diff`, `/help`, `/status`) ride the surface decisions above without their own doc.

## Sources

- `crates/oxide-code/src/tui/components/input.rs:344-356` — `InputArea::submit()` → `UserAction::SubmitPrompt`.
- `crates/oxide-code/src/tui/app.rs:218-289` — `App::dispatch_user_action`, `apply_action_locally`.
- `crates/oxide-code/src/tui/components/chat/blocks.rs:13-25` — `ChatBlock` registry; insertion point for `SystemMessageBlock`.
- `crates/oxide-code/src/tui/components/chat/blocks/error.rs` — template for the new system-message block.
- `crates/oxide-code/src/tool.rs` — trait / registry pattern the slash-command module mirrors.
- `crates/oxide-code/src/model.rs:62` — comment explicitly earmarking a future `/model` picker.
- `crates/oxide-code/src/session/handle.rs:120-190` — `SessionHandle::record_message`, `append_ai_title`, `finish`, `shutdown`. Insertion point for `/clear` semantics.
- `crates/oxide-code/src/session/resolver.rs` — `resolve_session`, `ResumeMode`. Reused for `/resume`.
- `crates/oxide-code/src/agent.rs:78-105` — `AgentClient` trait the model-swap design pivots around.
- `crates/oxide-code/src/client/anthropic.rs` — `Client` ownership point; mid-session model mutation lands here.
- `claude-code/src/commands/clear/conversation.ts` — `clearConversation` reference flow for `/clear`.
- `claude-code/src/bootstrap/state.ts::regenerateSessionId` — parent-session-id linkage on roll (deferred).
