# Slash Commands (Reference)

Research on client-side command surfaces. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

For modal-specific architecture (how `local-jsx` / `BottomPaneView` / `dialog.show()` actually work) see [modals.md](modals.md).

## Claude Code (TypeScript)

Declarative registry with three execution modes; lazy-loaded implementations.

- **Registry**: ~100 `Command` records under `src/commands/<name>/index.ts`, ~50 of which are `local-jsx` modals. Metadata: `name`, `aliases`, `description`, `type: 'local' | 'local-jsx' | 'prompt'`, `isEnabled`, `isHidden`, `immediate`, `isSensitive`, `availability`. Each command directory ships its own modal component (`<name>.tsx`) loaded via `load: () => import('./<name>.js')`.
- **Parser**: `slashCommandParsing.ts` splits on whitespace. Unknown names use Fuse.js (threshold 0.3).
- **Dispatch**: Three modes — `local` returns `{ resultText, displayMode }`, `local-jsx` returns React JSX (modal pickers), `prompt` expands to text and submits.
- **Output**: Display modes: `'skip'` (no transcript entry), `'system'` (synthetic local-stdout message), `'user'` (default). Meta flag (`isMeta: true`) keeps a message model-visible while hiding from UI.
- **Autocomplete**: Fuse.js across name, aliases, name parts, descriptions (weights: name 3, alias/part 2, description 0.5). Max 5 items. Re-sorts by tier: exact-name -> exact-alias -> prefix-name -> prefix-alias -> fuzzy.
- **Custom**: Markdown files in `~/.claude/skills/`, `~/.claude/commands/`, `./.claude/skills/`, `./.claude/commands/` with YAML frontmatter.

## OpenAI Codex (Rust)

Single strum-derived `enum SlashCommand` (~50 variants) with per-variant methods.

- **Registry**: `EnumString`, `EnumIter`, `serialize_all = "kebab-case"`. Enum order is presentation order (not alpha-sorted).
- **Metadata**: `description()`, `supports_inline_args()`, `available_in_side_conversation()`, `available_during_task()`, `is_visible()`.
- **Parser + autocomplete**: `CommandPopup` activates when buffer starts with `/`.
- **Dispatch**: One big `match` on the variant.
- **Output**: Synthetic `history_cell`s in the chat list. No toasts.
- **No custom commands.**

## opencode (TypeScript)

Slim `CommandOption[]` from a React hook (~12 built-ins).

- **Registry**: `id`, `title`, `description`, `category`, `keybind`, `slash`, `onSelect`, `disabled`.
- **Parser**: Regex `^\/(\S*)$` (line start only, normal mode). `@` for file mentions, `!` for shell mode.
- **Dispatch**: Built-in closures + server-side `client.session.command()` for custom.
- **Output**: `showToast()` for notifications, `dialog.show()` for pickers.
- **Autocomplete**: Filtered list, max 10, custom commands flagged with source badges.
- **Custom**: Server-defined (not file-based). Server publishes `Command[]` array.

## Comparison

| Repo        | Registry shape              | Variants | Parser site    | Dispatch              | Output target                       | Custom commands              |
| ----------- | --------------------------- | -------- | -------------- | --------------------- | ----------------------------------- | ---------------------------- |
| Claude Code | declarative `Command[]`     | ~100     | submit handler | three modes           | synthetic messages w/ display modes | yes (markdown + YAML)        |
| Codex       | strum enum + impl methods   | ~55      | input layer    | one big `match`       | synthetic `history_cell`            | no                           |
| opencode    | `CommandOption[]` from hook | ~12      | input layer    | closures + server     | toast / dialog / synthetic message  | yes (server-published)       |
| oxide-code  | trait + `&[&dyn]` slice     | 8        | submit handler | `SlashOutcome` return | `SystemMessageBlock` / `ErrorBlock` | not yet (namespace reserved) |
