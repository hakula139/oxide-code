# Theming

Pick a bundled Catppuccin or Material palette for the TUI, load a custom TOML theme file, or patch individual color slots.

## Quick Start

```toml
# ~/.config/ox/config.toml or .ox/config.toml

# Built-in name (mocha, macchiato, frappe, latte, material), or a file path in user config.
[tui.theme]
base = "latte"

# Patch individual slots on top of the base.
[tui.theme.overrides]
error = "#ff0000"
accent = { bold = false }
```

Both keys are optional. The default is `mocha` (Catppuccin Mocha) with no overrides.

## Built-In Themes

| Name        | Family     | Variant               |
| ----------- | ---------- | --------------------- |
| `mocha`     | Catppuccin | Dark, neutral default |
| `macchiato` | Catppuccin | Medium-dark           |
| `frappe`    | Catppuccin | Medium                |
| `latte`     | Catppuccin | Light                 |
| `material`  | Material   | Dark M2 baseline      |

Each ships as a TOML file [in the source tree](https://github.com/hakulab/oxide-code/tree/main/crates/ox-tui/themes), useful as a starting point for custom themes.

## Custom Theme Files

In user config or a drop-in, `base` accepts a path to a TOML file matching the built-in schema:

```toml
[tui.theme]
base = "~/.config/ox/themes/dark-extra.toml"
```

- **Paths**: Leading `~/` expands to your home directory. Environment variables (`$HOME`, `${XDG_CONFIG_HOME}`, `%USERPROFILE%`) do not expand. Relative paths resolve from the launch working directory.
- **Project config**: Must name a built-in theme, since a file path would let a repository choose what `ox` reads.
- **Limits**: Any value that is neither a built-in name nor a readable regular file of at most 64 KiB stops TUI startup.

## Slot Value Formats

Every `fg` / `bg` value and bare-string slot accepts:

| Form              | Example      | Maps to                                                                           |
| ----------------- | ------------ | --------------------------------------------------------------------------------- |
| 6-digit hex       | `"#cdd6f4"`  | 24-bit RGB, with no 3-digit shorthand                                             |
| ANSI 16 named     | `"red"`      | A [terminal palette color](https://en.wikipedia.org/wiki/ANSI_escape_code#Colors) |
| Indexed 256-color | `"ansi:174"` | 256-color palette index                                                           |
| Terminal default  | `"reset"`    | The terminal default for the channel (`fg` or `bg`) it is set to                  |

ANSI 16-color names are case-insensitive:

- **Standard**: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray` (alias `grey`).
- **Bright**: `dark_gray` (alias `dark_grey`), `bright_red` (alias `light_red`), `bright_green` (alias `light_green`), `bright_yellow` (alias `light_yellow`), `bright_blue` (alias `light_blue`), `bright_magenta` (alias `light_magenta`), `bright_cyan` (alias `light_cyan`), `white` (alias `bright_white` / `light_white`).

## Slot Definitions

A theme file is a flat TOML table defining **every slot** plus an optional [`syntax_theme`](#code). Missing slots or unknown keys fail parsing. Use [overrides](#overrides) to change a subset of slots. Each slot accepts two forms:

- **Bare color string**: Sets `fg` on foreground slots and `bg` on background-only slots, with no modifiers:

  ```toml
  user = "#fab387"
  blockquote = "#6c7086"
  surface = "reset"
  ```

- **Inline table**: Sets `fg`, `bg`, and modifier flags (`bold`, `italic`, `underlined`, `dim`, `reversed`, defaulting to `false`). Background-only slots accept only `bg`:

  ```toml
  accent = { fg = "#89b4fa", bold = true }
  link = { fg = "#89b4fa", underlined = true }
  diff_add = { bg = "#2a3a37" }
  ```

Unknown keys fail theme-file parsing and cause an override to [fall back](#errors).

## Slots

### Text Hierarchy

| Slot    | Role                                                                             |
| ------- | -------------------------------------------------------------------------------- |
| `text`  | Primary text                                                                     |
| `muted` | Secondary text, labels, and line numbers                                         |
| `dim`   | Low-emphasis text such as tool output, hints, modal footers, and unselected rows |

Keep `dim` readable against your background, since it is the faintest text color. Built-ins give it at least `tool_border`'s contrast (and ≥ 4:1 on dark backgrounds), while `border_unfocused`, `horizontal_rule`, `table_border`, and `separator` sit fainter because they only frame content.

### Surfaces

| Slot      | Role                                                  |
| --------- | ----------------------------------------------------- |
| `surface` | Chat / input / status panel background fill (bg-only) |

### Semantic Accents

| Slot        | Role                                      |
| ----------- | ----------------------------------------- |
| `accent`    | Highlights, active borders (bold)         |
| `user`      | User message bar / icon                   |
| `queued`    | Queued user prompts in the preview ghosts |
| `assistant` | Assistant message bar / icon              |

### Status Indicators

| Slot      | Role                                                                                            |
| --------- | ----------------------------------------------------------------------------------------------- |
| `info`    | In-progress and neutral signals, such as streaming                                              |
| `success` | Successful tool results, added diff lines, passing PR checks and approvals, and success notices |
| `warning` | Caution status, such as the **Ctrl+C** exit hint                                                |
| `error`   | Errors, failed tools, critical status                                                           |

### Effort Gradient

| Slot            | Role                          |
| --------------- | ----------------------------- |
| `effort_low`    | `low` on the effort slider    |
| `effort_medium` | `medium` on the effort slider |
| `effort_high`   | `high` on the effort slider   |
| `effort_xhigh`  | `xhigh` on the effort slider  |
| `effort_max`    | `max` on the effort slider    |

Built-ins use ANSI `blue`, `cyan`, `green`, `magenta`, and `red` to follow your terminal palette. Hex overrides pin levels to explicit colors.

### Code

| Slot          | Role                                           |
| ------------- | ---------------------------------------------- |
| `code`        | Fenced code blocks with no recognized language |
| `inline_code` | Inline `code` spans (between backticks)        |

Fenced code with a recognized language uses a bundled [syntect](https://github.com/trishume/syntect) theme set by top-level `syntax_theme` in a theme file:

```toml
syntax_theme = "InspiredGitHub"
```

Defaults to `base16-ocean.dark` (used by dark built-ins, while `latte` uses `InspiredGitHub`). Accepted values: `base16-ocean.dark`, `base16-eighties.dark`, `base16-mocha.dark`, `base16-ocean.light`, `InspiredGitHub`, `Solarized (dark)`, and `Solarized (light)`. Other names fail parsing. Because overrides patch slots only, changing `syntax_theme` on a built-in requires copying its TOML to a custom file.

### Diff Backgrounds (Background Only)

| Slot            | Role                              |
| --------------- | --------------------------------- |
| `diff_add`      | Added rows in Edit diffs          |
| `diff_del`      | Deleted rows                      |
| `diff_add_word` | The changed words of added rows   |
| `diff_del_word` | The changed words of deleted rows |

### Markdown Headings

| Slot            | Role                                   |
| --------------- | -------------------------------------- |
| `heading_h1`    | H1, most prominent (bold + underlined) |
| `heading_h2`    | H2, bold section header                |
| `heading_h3`    | H3, bold italic                        |
| `heading_minor` | H4-H6, italic                          |

### Markdown Body

| Slot          | Role                                                                                          |
| ------------- | --------------------------------------------------------------------------------------------- |
| `thinking`    | Thinking text (italic), as readable as `dim`                                                  |
| `link`        | Markdown links (underlined)                                                                   |
| `blockquote`  | Blockquote bar (`│`), beside quoted text in `muted`, or in `thinking` within a thinking block |
| `list_marker` | Bullet / number marker on list items                                                          |

### Markdown Chrome

| Slot              | Role                                     |
| ----------------- | ---------------------------------------- |
| `horizontal_rule` | `---` rule                               |
| `table_header`    | Table header cells (bold accent)         |
| `table_border`    | Table borders and the rules between rows |

### UI Chrome

| Slot               | Role                                                                 |
| ------------------ | -------------------------------------------------------------------- |
| `tool_border`      | Tool borders and the line-number separator                           |
| `tool_icon`        | Per-tool icon                                                        |
| `border_focused`   | Input rules while idle, which switch to `accent` while a turn runs   |
| `border_unfocused` | Unfocused component border                                           |
| `separator`        | Color and modifiers of the fixed status-line segment separator (`│`) |
| `selection`        | Chat text selected with the mouse (bg-only)                          |

Choose a `selection` background that keeps `dim` and `thinking` text at ≥ 3:1 contrast.

### Status-Line Segments

Each status-line segment uses one or more slots above. Override the slot to restyle it.

| Segment               | Slot                                      |
| --------------------- | ----------------------------------------- |
| `current-dir`         | `muted`                                   |
| `git-branch`          | `accent`                                  |
| `pull-request`        | `accent`                                  |
| `pull-request-checks` | `success` / `warning` / `error` / `muted` |
| `pull-request-review` | `success` / `warning` / `error`           |
| `model`               | `text`                                    |
| `model-with-effort`   | `text`                                    |
| `context-used`        | `text` / `warning` / `error`              |
| `session-cost`        | `muted`                                   |
| `five-hour-limit`     | `text` / `warning` / `error`              |
| `weekly-limit`        | `text` / `warning` / `error`              |
| `permission-mode`     | `muted` / `accent` / `error`              |
| `run-state` (idle)    | `muted`                                   |
| `run-state` (busy)    | `info`                                    |
| `run-state` (warn)    | `warning`                                 |
| `queued-prompts`      | `muted`                                   |
| `task-progress`       | `muted`                                   |
| `thread-title`        | `muted`                                   |
| `session-id`          | `muted`                                   |
| `theme`               | `muted`                                   |
| `current-time`        | `dim`                                     |

- **`context-used`, `five-hour-limit`, and `weekly-limit`**: `text` below 70%, `warning` from 70%, `error` from 90%.
- **`permission-mode`**: `muted` for `auto` and `manual`, `accent` for `plan`, `error` for `yolo`.
- **`pull-request-checks` and `pull-request-review`**: `success` (passing / approved), `warning` (pending / required), `error` (failing / changes requested), `muted` (cancelled).

## Overrides

`[tui.theme.overrides]` patches named slots on top of the base theme, changing only the fields you list:

```toml
[tui.theme.overrides]
# Bare-string form patches the slot's own channel, keeping the base's other channel and modifiers.
error = "#ff0000"
# Inline form patches just modifiers. fg / bg come from the base.
accent = { bold = false }
# Inline form patches fg and adds bold.
link = { fg = "#ff79c6", bold = true }
```

Bare strings patch `fg` on foreground slots and `bg` on background-only slots (`selection = "#393a4c"` recolors only the highlight). Passing `fg` or modifiers to a background-only slot emits a chat notice and ignores that slot's override. Omitted modifiers keep the base value, `true` sets them, and `false` clears them (so `accent = { bold = false }` clears bold while keeping the base color, and `accent = { italic = true }` adds italic while keeping bold). Empty patches (`accent = {}`) warn and fall back to the base.

Overrides persist across [`/theme`](slash-commands.md#pickers) previews and switches for the session, and merge by slot across [config layers](configuration.md#layers). Two inline-table patches for the same slot merge field by field (later layer wins). Otherwise the later patch replaces the earlier one.

## Errors

- **Theme selection errors** stop TUI startup with the cause: an unknown built-in name with no matching file, a file path in project config, a non-regular file or file over 64 KiB, an unreadable file, or a TOML parse error (line and column only, without quoting the source line). `-p` runs skip theme loading.
- **Per-slot override errors** emit a chat notice and keep the base slot without blocking startup: unparsable color, unknown slot, invalid patch type, empty inline patch, or `fg` / modifier on a background-only slot.

## Examples

### Custom Theme File with a Few Overrides on Top

```toml
[tui.theme]
base = "~/.config/ox/themes/dracula.toml"

[tui.theme.overrides]
accent = { fg = "#bd93f9", bold = true }
link = { fg = "#8be9fd", underlined = true }
```

### Use the Terminal's Foreground for Primary Text

Useful on transparent or non-truecolor terminals where RGB foregrounds clash with the terminal palette:

```toml
[tui.theme.overrides]
text = "reset"
```

### Tint the Chat Panel Background

`surface` defaults to the terminal background (`reset`). Set `bg` for an opaque tint on transparent terminals:

```toml
[tui.theme.overrides]
surface = { bg = "#1e1e2e" }
```
