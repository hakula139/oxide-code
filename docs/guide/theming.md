# Theming

oxide-code's TUI palette is user-configurable. Pick one of the bundled Catppuccin or Material palettes, point at a TOML file you wrote yourself, or patch individual color slots on top of any base, all without rebuilding `ox`.

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

Both `[tui.theme]` keys are optional. Without them the default is `mocha` (Catppuccin Mocha) with no overrides.

## Built-In Themes

| Name        | Family     | Variant               |
| ----------- | ---------- | --------------------- |
| `mocha`     | Catppuccin | Dark, neutral default |
| `macchiato` | Catppuccin | Medium-dark           |
| `frappe`    | Catppuccin | Medium                |
| `latte`     | Catppuccin | Light                 |
| `material`  | Material   | Dark M2 baseline      |

Each ships as a vendored TOML file [in the source tree][themes-src] and doubles as a copy-paste starting point for custom themes.

[themes-src]: https://github.com/hakula139/oxide-code/tree/main/crates/ox-tui/themes

## Custom Theme Files

In your user config or a drop-in, `base` accepts any filesystem path to a TOML body using the same shape as the vendored themes. A project `.ox/config.toml` may only name a built-in theme, since a path there would have `ox` read a file the repository chooses. A leading `~/` expands to `$HOME`, but no other expansion happens: environment variables (`$HOME`, `${XDG_CONFIG_HOME}`) and Windows-style references (`%USERPROFILE%`) pass through literally and will fail to read. Relative paths resolve against the directory you launched `ox` from, so prefer absolute or `~/`-anchored paths.

```toml
[tui.theme]
base = "~/.config/ox/themes/dark-extra.toml"
```

If the value isn't a built-in name and can't be read as a regular file of at most 64 KiB, oxide-code refuses to start with an actionable error message.

## Slot Value Formats

Every `fg` / `bg` value, and every bare-string slot, accepts:

| Form              | Example      | Maps to                                                               |
| ----------------- | ------------ | --------------------------------------------------------------------- |
| 6-digit hex       | `"#cdd6f4"`  | 24-bit RGB                                                            |
| ANSI 16 named     | `"red"`      | terminal palette color (see list)                                     |
| Indexed 256-color | `"ansi:174"` | 256-color palette index                                               |
| Terminal default  | `"reset"`    | the terminal default for whichever channel (`fg` or `bg`) it's set to |

ANSI 16-color names accepted (case-insensitive):

- **Standard**: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray` (alias `grey`).
- **Bright**: `dark_gray` (alias `dark_grey`), `bright_red` (alias `light_red`), `bright_green` (alias `light_green`), `bright_yellow` (alias `light_yellow`), `bright_blue` (alias `light_blue`), `bright_magenta` (alias `light_magenta`), `bright_cyan` (alias `light_cyan`), `white` (alias `bright_white` / `light_white`).

See the [ANSI escape code reference][ansi] for what each name maps to in your terminal's palette. Three-digit hex shorthand (`#fff`) is rejected, so write all six digits.

[ansi]: https://en.wikipedia.org/wiki/ANSI_escape_code#Colors

## Slot Definitions

A custom theme file must define **every slot**. A missing slot is a parse error, so typos surface immediately. For partial customization on top of a base, use `[tui.theme.overrides]` instead (see [Overrides](#overrides) below).

> **Note:** the same `slot = "#hex"` line means different things in a theme file vs an override. Inside a theme file body, it's a bare-color slot definition with `fg` set and `bg` / modifiers cleared. Inside `[tui.theme.overrides]`, it's a _patch_ that updates only `fg` and preserves the base slot's `bg` and modifiers. The override semantics are detailed in [Overrides](#overrides).

A theme TOML is a flat document with one entry per slot plus an optional [`syntax_theme`](#code) key. Each slot takes one of two shapes:

- **Bare color string**: `fg`-only, no modifiers:

  ```toml
  user = "#fab387"
  blockquote = "#6c7086"
  ```

- **Inline table**: explicit `fg` / `bg` and any modifier flags:

  ```toml
  accent = { fg = "#89b4fa", bold = true }
  link = { fg = "#89b4fa", underlined = true }
  diff_add = { bg = "#2a3a37" }
  ```

Recognized modifier keys: `bold`, `italic`, `underlined`, `dim`, `reversed`. An unknown key fails a theme file's parse and makes an override [fall back](#errors). In a theme file, modifier keys are plain booleans defaulting to `false`.

## Slots

Each slot maps to one role in the TUI. Override a slot by name to restyle that role.

### Text Hierarchy

| Slot    | Role                                                                             |
| ------- | -------------------------------------------------------------------------------- |
| `text`  | Primary text                                                                     |
| `muted` | Secondary text, labels, and line numbers                                         |
| `dim`   | Low-emphasis text such as tool output, hints, modal footers, and unselected rows |

`dim` is the faintest color for text people read, so keep it readable against your background. The built-ins give it at least the contrast of `tool_border`, and at least 4:1 on their dark backgrounds. Rules and unfocused borders (`border_unfocused`, `horizontal_rule`, `table_border`, and `separator`) can sit a step fainter because they only frame content.

### Surfaces

| Slot      | Role                                                  |
| --------- | ----------------------------------------------------- |
| `surface` | Chat / input / status panel background fill (bg-only) |

> **Always declare `surface` as an inline table with `bg`**, e.g. `surface = { bg = "#1e1e2e" }` or `surface = { bg = "reset" }`. A bare-string `surface = "#1e1e2e"` would route to `fg` (per the slot-definition rules above) and silently repaint every panel's text in the surface color.

### Semantic Accents

| Slot        | Role                                      |
| ----------- | ----------------------------------------- |
| `accent`    | Highlights, active borders (bold)         |
| `user`      | User message bar / icon                   |
| `queued`    | Queued user prompts in the preview ghosts |
| `assistant` | Assistant message bar / icon              |

### Status Indicators

| Slot      | Role                                                                                                                 |
| --------- | -------------------------------------------------------------------------------------------------------------------- |
| `info`    | In-progress / neutral signals (e.g., streaming)                                                                      |
| `success` | Successful tool results, added diff lines, passing PR checks and approvals, and success notices (e.g., after a copy) |
| `warning` | Caution status (e.g., the **Ctrl+C** exit hint)                                                                      |
| `error`   | Errors, failed tools, critical status                                                                                |

### Code

| Slot          | Role                                           |
| ------------- | ---------------------------------------------- |
| `code`        | Fenced code blocks with no recognized language |
| `inline_code` | Inline `code` spans (between backticks)        |

Fenced code with a recognized language takes its token colors from a bundled [syntect] theme, chosen by the optional top-level `syntax_theme` key in a theme file:

```toml
syntax_theme = "InspiredGitHub"
```

Unset, it defaults to `base16-ocean.dark`, which the dark built-ins use. `latte` uses `InspiredGitHub`. The accepted names are `base16-ocean.dark`, `base16-eighties.dark`, `base16-mocha.dark`, `base16-ocean.light`, `InspiredGitHub`, `Solarized (dark)`, and `Solarized (light)`. Any other name is a theme parse error. `[tui.theme.overrides]` patches slots only, so changing `syntax_theme` on a built-in base means copying that theme's TOML into a custom file.

[syntect]: https://github.com/trishume/syntect

### Diff Backgrounds (Background Only)

| Slot       | Role                                         |
| ---------- | -------------------------------------------- |
| `diff_add` | Background fill for added rows in Edit diffs |
| `diff_del` | Background fill for deleted rows             |

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

| Slot               | Role                                        |
| ------------------ | ------------------------------------------- |
| `tool_border`      | Tool borders and the line-number separator  |
| `tool_icon`        | Per-tool icon                               |
| `border_focused`   | Input rules while idle                      |
| `border_unfocused` | Unfocused component border                  |
| `separator`        | Status-line segment separator (`│`)         |
| `selection`        | Chat text selected with the mouse (bg-only) |

The input box draws its rules in `border_focused` while idle and in `accent` while busy, when **Enter** queues the prompt.

Declare `selection` as an inline table with `bg`, such as `selection = { bg = "#393a4c" }`, so selected text keeps its own colors. Pick a background that leaves `dim` and `thinking` text at a contrast of at least 3:1.

### Status-Line Segments

Each status-line segment routes to one of the slots above so re-skinning the slot re-skins everywhere it appears. To restyle one segment, override the slot it uses.

| Segment             | Slot                         |
| ------------------- | ---------------------------- |
| `current-dir`       | `muted`                      |
| `git-branch`        | `accent`                     |
| `pull-request`      | `accent`                     |
| `model`             | `text`                       |
| `model-with-effort` | `text`                       |
| `context-used`      | `text` / `warning` / `error` |
| `session-cost`      | `muted`                      |
| `five-hour-limit`   | `text` / `warning` / `error` |
| `weekly-limit`      | `text` / `warning` / `error` |
| `permission-mode`   | `muted` / `accent` / `error` |
| `run-state` (idle)  | `muted`                      |
| `run-state` (busy)  | `info`                       |
| `run-state` (warn)  | `warning`                    |
| `queued-prompts`    | `muted`                      |
| `task-progress`     | `muted`                      |
| `thread-title`      | `muted`                      |
| `session-id`        | `muted`                      |
| `theme`             | `muted`                      |
| `current-time`      | `dim`                        |

`permission-mode` uses `muted` for `manual`, `accent` for `plan`, and `error` for `yolo`.

`context-used`, `five-hour-limit`, and `weekly-limit` use `text` until the share they show reaches 70%, then `warning`, and `error` from 90%.

`pull-request-checks` and `pull-request-review` take their slot from the state they show: `success` for passing checks or an approval, `warning` for pending checks or a required review, `error` for failing checks or requested changes, and `muted` for cancelled checks.

The separator glyph itself (`│`) is fixed, and the `separator` slot controls only its color and modifiers.

## Overrides

`[tui.theme.overrides]` is a table of `slot_name = patch` pairs. A patch is _additive_, so only the fields it lists are applied to the base slot.

```toml
[tui.theme.overrides]
# Bare-string form patches fg only. bg and modifiers come from the base.
error = "#ff0000"
# Inline form patches just modifiers. fg / bg come from the base.
accent = { bold = false }
# Inline form patches fg and adds bold.
link = { fg = "#ff79c6", bold = true }
```

Modifier flags use **three-state semantics**:

| Flag value | Effect on the base modifier        |
| ---------- | ---------------------------------- |
| omitted    | no change, base value is preserved |
| `true`     | sets the bit                       |
| `false`    | clears the bit                     |

So `accent = { bold = false }` removes bold from the base accent without disturbing its color, and `accent = { italic = true }` adds italic without removing the base bold. An entirely empty patch (`accent = {}`) warns and falls back to the base, since rewriting the base with itself is almost always a typo.

Overrides also apply to every built-in that [`/theme`](slash-commands.md#theme) previews or switches to, so they last for the whole session.

Overrides from every [config layer](configuration.md#layers) merge by slot name, so a project can patch `error` while your user config patches `accent`. When two layers patch the same slot with inline tables, their fields combine and the later layer wins each field. Otherwise the later layer's patch replaces the earlier one.

## Errors

Theme errors fall into two severities:

- **Theme selection errors** are fatal when the TUI starts. An unknown built-in name with no matching file path, a file path set in a project file, a path that is not a regular file of at most 64 KiB, a file that cannot be read, or a file with a parse error in the base body stops oxide-code at startup with a message identifying what went wrong. A parse error gives its line and column without quoting the source line. `-p` and `--no-tui` runs never load the theme, so a theme error does not stop them.
- **Per-slot value errors** warn and fall back. If an override's color string can't be parsed, its slot name isn't recognized, its patch is neither a color string nor a table of `fg`, `bg`, and modifier flags, or an inline patch is empty, oxide-code uses the base slot's value for that role and shows a notice in the chat. The TUI still launches.

## Examples

### Mocha Base with a Brighter Error Color

```toml
[tui.theme.overrides]
error = "#ff5555"
```

(`base` defaults to `mocha` when omitted.)

### Custom Theme File with a Few Overrides on Top

```toml
[tui.theme]
base = "~/.config/ox/themes/dracula.toml"

[tui.theme.overrides]
accent = { fg = "#bd93f9", bold = true }
link = { fg = "#8be9fd", underlined = true }
```

### Use the Terminal's Foreground for Primary Text

```toml
[tui.theme.overrides]
text = "reset"
```

Useful for transparent / non-truecolor terminals where forcing an RGB foreground fights the user's terminal scheme.

### Tint the Chat Panel Background

```toml
[tui.theme.overrides]
surface = { bg = "#1e1e2e" }
```

Default `surface` uses the terminal background (`reset`). Set a `bg` to give the chat / input / status panels an opaque tint, useful on transparent terminals.
