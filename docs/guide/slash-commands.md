# Slash Commands

Slash commands are built-in shortcuts that run client-side, without involving the model. Type `/` to open the autocomplete popup, browse with Up / Down, and complete with Tab.

## Built-in Commands

| Command                                 | Description                                                                          |
| --------------------------------------- | ------------------------------------------------------------------------------------ |
| `/clear` (aliases `/new`, `/reset`)     | Start a fresh session. The previous one stays resumable via `ox -c`.                 |
| `/config`                               | Open the resolved configuration and its layered file paths in a read-only modal.     |
| `/diff`                                 | Show `git diff HEAD` plus untracked files in chat, capped at 64 KB.                  |
| `/effort [<level>]`                     | Open the slider, or set the tier directly (`low`, `medium`, `high`, `xhigh`, `max`). |
| `/help`                                 | Open a read-only modal listing available commands.                                   |
| `/init`                                 | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.                          |
| `/model [<id>]`                         | Open the model + effort picker, or swap directly (alias / substring / exact id).     |
| `/resume [<id-prefix>]` (`/continue`)   | Open the session picker (search, project / all toggle), or jump by id prefix.        |
| `/status`                               | Open a read-only modal of model, effort, cwd, version, auth, and session id.         |
| `/theme [<name>]`                       | Open the theme picker (live preview), or swap directly to a built-in theme.          |

## Autocomplete Popup

When you type `/`, a two-column popup appears above the input:

- **Up / Down** navigate the rows.
- **Tab** completes the selected row. In name mode it inserts `/<name>` plus a trailing space; in arg mode it replaces the typed prefix with the picked value.
- **Enter** submits the current line.
- **Esc** dismisses the popup.

Matches are ranked by tier: name-prefix > alias-prefix > name-substring > alias-substring. Aliases display inline in the canonical row (`/clear (new, reset)`); typing any alias routes to the same impl.

After `/<name>` plus a trailing space, the popup switches to **arg mode** for commands with a curated argument roster (`/model`, `/effort`, `/theme`): rows show valid values with descriptions, prefix-filtered as you type.

## Sending a Literal `/foo`

To send a message that _starts_ with a slash without invoking a command, double the leading slash. Typing `//etc` sends the literal `/etc`.

## Mid-Turn Behavior

Read-only commands (`/config`, `/diff`, `/help`, `/status`, and bare `/model` / `/effort` / `/resume` / `/theme` which open modals) are safe to run while the agent is streaming. State-mutating commands (`/clear`, `/init`, `/model <id>`, `/effort <level>`, `/resume <id-prefix>`, `/theme <name>`) refuse mid-turn — wait for the current response to complete, then retry.

## Model and Effort

Bare `/model` opens the combined model + effort picker; bare `/effort` opens a Speed ↔ Intelligence slider. Both apply on Enter and cancel on Esc.

`/model <id>` resolves in four tiers: alias (`opus`, `sonnet`, `haiku`, with optional `[1m]` for the 1M-context variants) → exact / dated id → unique suffix → unique substring. Swapping clamps the current effort to the new model's caps; `/effort` on a model without effort (Haiku 4.5) errors with a recovery hint. See [Configuration](configuration.md) for the full tier reference and per-model defaults.

## Resuming a Session

Bare `/resume` (alias `/continue`) opens an in-place session picker. Type to filter by id, title, or project; Up / Down or PageUp / PageDown navigate; Tab toggles between the current project and all projects; Enter resumes the highlighted session; Esc cancels. Switching session preserves the running TUI — chat repopulates from the resumed transcript and the next prompt continues that thread.

`/resume <id-prefix>` resolves the prefix against the current project first and widens to all projects if there's no in-project match. Ambiguous prefixes list the candidates with their 8-character ids.

`/resume` mid-session is the in-app equivalent of `ox -c <id-prefix>` at launch — both call the same load + sanitize path. The CLI launcher is unchanged.

## Switching the Theme

`/theme` (no argument) opens a list picker for the built-in themes — Up / Down repaints the full TUI in the candidate theme so you can compare without committing, number keys (`1`–`9`) jump to a row, Enter applies for the rest of the session, Esc snaps back to the original. Restart returns to the theme set in your `ox.toml`.

`/theme <name>` swaps directly to a built-in (`mocha`, `macchiato`, `frappe`, `latte`, `material`). Custom file-path themes aren't accepted via the slash form — edit `~/.config/ox/config.toml` to point `[tui.theme] base` at a custom TOML.

## Stance: No Silent Config Writes

Slash commands never write user config files. Runtime mutations (`/model`, `/effort`, `/theme`) stay session-local; restart returns to the user-declared configuration. Persisting a slash-command choice across restarts will require an explicit subcommand writing to an explicit user-opted-in path — never a silent merge.
