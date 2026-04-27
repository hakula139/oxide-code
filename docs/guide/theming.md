# Theming

oxide-code's TUI palette is fully user-configurable. Pick a built-in Catppuccin variant, point at a TOML file you wrote yourself, or patch individual color slots on top of any base. No recompile.

## Quick start

```toml
# ox.toml or ~/.config/ox/config.toml

# Built-in name (mocha, macchiato, frappe, latte) or filesystem path.
[tui.theme]
base = "latte"

# Patch individual slots on top of the base.
[tui.theme.overrides]
error = "#ff0000"
accent = { bold = false }
```

Both `[tui.theme]` keys are optional. Without them the default is `mocha` (Catppuccin Mocha) with no overrides.

## Built-in themes

| Name        | Variant                |
| ----------- | ---------------------- |
| `mocha`     | Dark — neutral default |
| `macchiato` | Medium-dark            |
| `frappe`    | Medium                 |
| `latte`     | Light                  |

Each ships as a vendored TOML file under `crates/oxide-code/themes/` and doubles as a copy-paste starting point for custom themes.

## Custom theme files

`base` accepts any filesystem path to a TOML body using the same shape as the vendored themes. `~/` expands to `$HOME`:

```toml
[tui.theme]
base = "~/.config/ox/themes/dark-extra.toml"
```

If the value isn't a built-in name AND can't be read as a file, oxide-code refuses to start with an actionable error message.

## Slot value formats

Every `fg` / `bg` value, and every bare-string slot, accepts:

| Form              | Example      | Maps to                           |
| ----------------- | ------------ | --------------------------------- |
| 6-digit hex       | `"#cdd6f4"`  | 24-bit RGB                        |
| ANSI 16 named     | `"red"`      | terminal palette color (see list) |
| Indexed 256-color | `"ansi:174"` | 256-color palette index           |
| Terminal default  | `"reset"`    | follows the terminal foreground   |

ANSI 16-color names accepted (case-insensitive):

- **Standard** — `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray` (alias `grey`).
- **Bright** — `dark_gray` (alias `dark_grey`), `bright_red` (alias `light_red`), `bright_green`, `bright_yellow`, `bright_blue`, `bright_magenta`, `bright_cyan`, `white`.

See the [ANSI escape code reference][ansi] for what each name maps to in your terminal's palette. Three-digit hex shorthand (`#fff`) is intentionally rejected — always use the full six digits.

[ansi]: https://en.wikipedia.org/wiki/ANSI_escape_code#Colors

## Slot definitions

A theme TOML is a flat document with one entry per slot. Two shapes:

- **Bare color string** — `fg`-only, no modifiers:

  ```toml
  user = "#fab387"
  blockquote = "#a6e3a1"
  ```

- **Inline table** — explicit `fg` / `bg` and any modifier flags:

  ```toml
  accent = { fg = "#89b4fa", bold = true }
  link = { fg = "#89b4fa", underlined = true }
  diff_add_bg = { bg = "#2a3a37" }
  ```

Recognized modifier keys: `bold`, `italic`, `underlined`, `dim`, `reversed`. Unknown keys fail the parse.

A complete theme file has all 31 slots; a missing slot is a parse error (catches typos). For per-slot patches in `[tui.theme.overrides]` the rules differ — see [Overrides](#overrides) below.

## Slots

Each slot maps to one role in the TUI. Override a slot by name to restyle that role.

### Text hierarchy

| Slot    | Role                                 |
| ------- | ------------------------------------ |
| `text`  | Primary text                         |
| `muted` | Secondary text, labels, soft borders |
| `dim`   | Dimmed metadata, timestamps          |

### Surfaces

| Slot      | Role                                      |
| --------- | ----------------------------------------- |
| `surface` | Elevated surface (reserved)               |
| `code_bg` | Code-block background (reserved; bg-only) |

### Semantic accents

| Slot        | Role                              |
| ----------- | --------------------------------- |
| `accent`    | Highlights, active borders (bold) |
| `user`      | User message bar / icon           |
| `secondary` | Assistant message bar / icon      |

### Status indicators

| Slot      | Role                                  |
| --------- | ------------------------------------- |
| `info`    | Informational highlight (reserved)    |
| `success` | Successful tool results, ready status |
| `warning` | Warning status                        |
| `error`   | Errors, failed tools, critical status |

### Code

| Slot                  | Role                                           |
| --------------------- | ---------------------------------------------- |
| `code`                | Code foreground (reserved palette role)        |
| `inline_code`         | Inline `code` spans (between backticks)        |
| `code_block_fallback` | Fenced code blocks with no recognized language |

### Diff backgrounds (bg-only)

| Slot          | Role                                         |
| ------------- | -------------------------------------------- |
| `diff_add_bg` | Background fill for added rows in Edit diffs |
| `diff_del_bg` | Background fill for deleted rows             |

### Markdown headings

| Slot            | Role                                    |
| --------------- | --------------------------------------- |
| `heading_h1`    | H1 — most prominent (bold + underlined) |
| `heading_h2`    | H2 — bold section header                |
| `heading_h3`    | H3 — bold italic                        |
| `heading_minor` | H4–H6 — italic                          |

### Markdown body

| Slot          | Role                                 |
| ------------- | ------------------------------------ |
| `thinking`    | Dimmed thinking text (italic)        |
| `link`        | Markdown links (underlined)          |
| `blockquote`  | Blockquote prefix marker             |
| `list_marker` | Bullet / number marker on list items |

### Markdown chrome

| Slot              | Role                |
| ----------------- | ------------------- |
| `horizontal_rule` | `---` rule          |
| `table_header`    | Table header cell   |
| `table_border`    | Table border glyphs |

### UI chrome

| Slot               | Role                                   |
| ------------------ | -------------------------------------- |
| `tool_border`      | Left border for tool call blocks       |
| `tool_icon`        | Per-tool icon                          |
| `border_focused`   | Focused component border (e.g., input) |
| `border_unfocused` | Unfocused component border             |
| `separator`        | Status bar separator (dimmed pipe)     |

## Overrides

`[tui.theme.overrides]` is a table of `slot_name = patch` pairs. A patch is _additive_ — only the fields it lists are applied to the base slot.

```toml
[tui.theme.overrides]
# Bare-string form — patches fg only; bg and modifiers come from the base.
error = "#ff0000"
# Inline form — patches just modifiers; fg / bg come from the base.
accent = { bold = false }
# Inline form — patches fg AND adds bold.
link = { fg = "#ff79c6", bold = true }
```

Modifier flags use three-state semantics: omitted means "no change," `true` sets the bit, `false` clears it. So `accent = { bold = false }` removes bold from the base accent without disturbing its color.

## Errors

Bisected severity:

- **Theme selection errors** are fatal. An unknown built-in name with no matching file path, a file that can't be read, a file with a parse error in the base body — any of these stop oxide-code at startup with a message identifying what went wrong.
- **Per-slot value errors** warn and fall back. If an override's color string can't be parsed, or its slot name isn't recognized, oxide-code logs a warning to stderr and uses the base slot's value for that role. The TUI still launches.

## Examples

### Minimal — switch to Latte

```toml
[tui.theme]
base = "latte"
```

### Mocha base + a brighter error color

```toml
[tui.theme.overrides]
error = "#ff5555"
```

(`base` defaults to `mocha` when omitted.)

### Custom theme file with a few overrides on top

```toml
[tui.theme]
base = "~/.config/ox/themes/dracula.toml"

[tui.theme.overrides]
accent = { fg = "#bd93f9", bold = true }
link = { fg = "#8be9fd", underlined = true }
```

### Use the terminal's foreground for primary text

```toml
[tui.theme.overrides]
text = "reset"
```

Useful for transparent / non-truecolor terminals where forcing an RGB foreground fights the user's terminal scheme.
