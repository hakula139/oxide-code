# Theming

Pick one of the bundled Catppuccin or Material palettes for the TUI, point at a TOML theme file you wrote, or patch individual color slots on top of either.

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

Each ships as a TOML file [in the source tree](https://github.com/hakula139/oxide-code/tree/main/crates/ox-tui/themes), which doubles as a starting point for a custom theme.

## Custom Theme Files

In your user config or a drop-in, `base` accepts the path of a TOML file shaped like the built-in themes:

```toml
[tui.theme]
base = "~/.config/ox/themes/dark-extra.toml"
```

- **Paths**: A leading `~/` expands to your home directory, and nothing else expands, so `$HOME`, `${XDG_CONFIG_HOME}`, and `%USERPROFILE%` fail to read. A relative path resolves against the directory you launched `ox` from.
- **Project config** may only name a built-in theme, since a path there would have `ox` read a file the repository chooses.
- **Limits**: A value that is neither a built-in name nor a readable regular file of at most 64 KiB stops `ox` at startup with an error.

## Slot Value Formats

Every `fg` / `bg` value, and every bare-string slot, accepts:

| Form              | Example      | Maps to                                                                           |
| ----------------- | ------------ | --------------------------------------------------------------------------------- |
| 6-digit hex       | `"#cdd6f4"`  | 24-bit RGB, with no 3-digit shorthand                                             |
| ANSI 16 named     | `"red"`      | A [terminal palette color](https://en.wikipedia.org/wiki/ANSI_escape_code#Colors) |
| Indexed 256-color | `"ansi:174"` | 256-color palette index                                                           |
| Terminal default  | `"reset"`    | The terminal default for the channel (`fg` or `bg`) it is set to                  |

ANSI 16-color names ignore case:

- **Standard**: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray` (alias `grey`).
- **Bright**: `dark_gray` (alias `dark_grey`), `bright_red` (alias `light_red`), `bright_green` (alias `light_green`), `bright_yellow` (alias `light_yellow`), `bright_blue` (alias `light_blue`), `bright_magenta` (alias `light_magenta`), `bright_cyan` (alias `light_cyan`), `white` (alias `bright_white` / `light_white`).

## Slot Definitions

A theme file is a flat TOML document with one entry per slot plus an optional [`syntax_theme`](#code) key. It must define **every slot**, so a missing slot or a typo is a parse error. To change only some slots, use [overrides](#overrides). Each slot takes one of two shapes:

- **Bare color string**: Sets the slot's own channel with no modifiers. That channel is `fg` for a foreground slot and `bg` for a background-only slot:

  ```toml
  user = "#fab387"
  blockquote = "#6c7086"
  surface = "reset"
  ```

- **Inline table**: Sets `fg` / `bg` and any modifier flags, while a background-only slot accepts only `bg`:

  ```toml
  accent = { fg = "#89b4fa", bold = true }
  link = { fg = "#89b4fa", underlined = true }
  diff_add = { bg = "#2a3a37" }
  ```

The modifier keys are `bold`, `italic`, `underlined`, `dim`, and `reversed`, which default to `false` in a theme file. An unknown key fails a theme file's parse and makes an override [fall back](#errors).

## Slots

Each slot styles one role in the TUI.

### Text Hierarchy

| Slot    | Role                                                                             |
| ------- | -------------------------------------------------------------------------------- |
| `text`  | Primary text                                                                     |
| `muted` | Secondary text, labels, and line numbers                                         |
| `dim`   | Low-emphasis text such as tool output, hints, modal footers, and unselected rows |

`dim` is the faintest color for text people read, so keep it readable against your background. The built-ins give it at least the contrast of `tool_border`, and at least 4:1 on their dark backgrounds, while `border_unfocused`, `horizontal_rule`, `table_border`, and `separator` can sit a step fainter because they only frame content.

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

The built-ins use ANSI `blue`, `cyan`, `green`, `magenta`, and `red`, so the gradient follows your terminal palette, and a hex override pins a level to the theme's own colors.

### Code

| Slot          | Role                                           |
| ------------- | ---------------------------------------------- |
| `code`        | Fenced code blocks with no recognized language |
| `inline_code` | Inline `code` spans (between backticks)        |

Fenced code with a recognized language takes its token colors from a bundled [syntect](https://github.com/trishume/syntect) theme, chosen by the optional top-level `syntax_theme` key in a theme file:

```toml
syntax_theme = "InspiredGitHub"
```

Unset, it defaults to `base16-ocean.dark`, which the dark built-ins use, while `latte` uses `InspiredGitHub`. The accepted names are `base16-ocean.dark`, `base16-eighties.dark`, `base16-mocha.dark`, `base16-ocean.light`, `InspiredGitHub`, `Solarized (dark)`, and `Solarized (light)`, and any other name is a parse error. Overrides patch slots only, so changing `syntax_theme` on a built-in base means copying that theme's TOML into a custom file.

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

Pick a `selection` background that leaves `dim` and `thinking` text at a contrast of at least 3:1.

### Status-Line Segments

Each status-line segment uses one of the slots above, so to restyle a segment, override its slot.

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

- **`context-used`, `five-hour-limit`, and `weekly-limit`** use `text` until the share they show reaches 70%, then `warning`, and `error` from 90%.
- **`permission-mode`** uses `muted` for `manual`, `accent` for `plan`, and `error` for `yolo`.
- **`pull-request-checks` and `pull-request-review`** use `success` for passing checks or an approval, `warning` for pending checks or a required review, `error` for failing checks or requested changes, and `muted` for cancelled checks.

## Overrides

`[tui.theme.overrides]` maps slot names to patches. A patch applies only the fields it lists to the base slot:

```toml
[tui.theme.overrides]
# Bare-string form patches the slot's own channel, keeping the base's other channel and modifiers.
error = "#ff0000"
# Inline form patches just modifiers. fg / bg come from the base.
accent = { bold = false }
# Inline form patches fg and adds bold.
link = { fg = "#ff79c6", bold = true }
```

A bare string patches `fg` on a foreground slot and `bg` on a background-only slot, so `selection = "#393a4c"` recolors the highlight without touching the text it covers. A background-only slot ignores `fg` or a modifier in its patch with a notice.

An omitted modifier keeps the base value, `true` sets it, and `false` clears it. So `accent = { bold = false }` removes bold from the base accent without changing its color, and `accent = { italic = true }` adds italic while keeping the base bold. An empty patch (`accent = {}`) warns and falls back to the base, since it is almost always a typo.

Overrides also apply to every built-in that [`/theme`](slash-commands.md#theme) previews or switches to, so they last for the whole session.

Overrides from every [config layer](configuration.md#layers) merge by slot name, so a project can patch `error` while your user config patches `accent`. When two layers patch the same slot with inline tables, their fields combine and the later layer wins each field. Otherwise the later layer's patch replaces the earlier one.

## Errors

- **Theme selection errors** stop the TUI at startup with a message naming the problem: an unknown built-in name with no matching file, a file path set in a project file, a path that is not a regular file of at most 64 KiB, an unreadable file, or a parse error in the theme file, which gives its line and column without quoting the source line. `-p` runs never load the theme, so a theme error does not stop them.
- **Per-slot override errors** show a notice in the chat, and the slot keeps the base value while the TUI still launches. They cover an unparsable color, an unknown slot name, a patch that is neither a color string nor a table of `fg`, `bg`, and modifier flags, an empty inline patch, and `fg` or a modifier on a background-only slot.

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

This helps on transparent or non-truecolor terminals, where an RGB foreground fights your terminal scheme:

```toml
[tui.theme.overrides]
text = "reset"
```

### Tint the Chat Panel Background

`surface` defaults to the terminal background (`reset`). Set a `bg` to give the chat, input, and status panels an opaque tint on a transparent terminal:

```toml
[tui.theme.overrides]
surface = { bg = "#1e1e2e" }
```
