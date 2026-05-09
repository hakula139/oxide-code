# Slash Commands

Type `/` to open the autocomplete popup. **Up / Down** browse, **Tab** completes, **Enter** submits, **Esc** dismisses. Aliases (`/clear (new, reset)`) all route to the same command.

For commands with curated arguments (`/model`, `/effort`, `/theme`), the popup switches to **arg mode** after `/<name>` + space, listing valid values.

## Built-in Commands

| Command                                     | Description                                                                          |
| ------------------------------------------- | ------------------------------------------------------------------------------------ |
| `/clear` (aliases `/new`, `/reset`)         | Start a fresh session. The previous one stays resumable via `ox -c`.                 |
| `/config`                                   | Open the resolved configuration and its layered file paths in a read-only modal.     |
| `/diff`                                     | Show `git diff HEAD` plus untracked files in chat, capped at 64 KB.                  |
| `/effort [<level>]`                         | Open the slider, or set the tier directly (`low`, `medium`, `high`, `xhigh`, `max`). |
| `/help`                                     | Open a read-only modal listing available commands.                                   |
| `/init`                                     | Generate or update the project's `AGENTS.md` / `CLAUDE.md`.                          |
| `/model [<id>]`                             | Open the model + effort picker, or swap directly (alias / substring / exact id).     |
| `/rename [<title>]`                         | Open a single-line modal pre-filled with the current title, or set it directly.      |
| `/resume [<id-prefix>]` (alias `/continue`) | Open the session picker (search, project / all toggle), or jump by id prefix.        |
| `/status`                                   | Open a read-only modal of model, effort, cwd, version, auth, and session id.         |
| `/theme [<name>]`                           | Open the theme picker (live preview), or swap directly to a built-in theme.          |

## Sending a Literal `/foo`

Double the leading slash. Typing `//etc` sends the literal `/etc`.

## Mid-Turn Behavior

State-mutating commands (`/clear`, `/init`, and the typed-arg forms of `/effort`, `/model`, `/rename`, `/resume`, `/theme`) wait for the current turn to finish. Read-only commands and the bare modal-opening forms run anytime.

## Model and Effort

Bare `/model` and `/effort` open pickers; both apply on Enter, cancel on Esc.

`/model <id>` accepts aliases (`opus`, `sonnet`, `haiku` — append `[1m]` for the 1M-context variants), full ids, or any unique suffix or substring. Haiku has no effort tier, so `/effort` on Haiku errors with a recovery hint. See [Configuration](configuration.md) for tier defaults.

## Sessions

`/rename` opens a modal pre-filled with the current title; `/rename <title>` sets it directly. The chosen title sticks and replaces the auto-generated AI title for the rest of the session.

`/resume` opens a searchable session picker — type to filter by id, title, or project, Tab toggles current-project ↔ all projects, Enter resumes. `/resume <id-prefix>` jumps directly; ambiguous prefixes list candidates. Switching sessions preserves the running TUI — chat repopulates and the next prompt continues that thread.

## Theme

`/theme` opens the picker — **Up / Down** previews each theme live, number keys jump to a row, **Enter** applies for the rest of the session, **Esc** reverts.

`/theme <name>` swaps directly to a built-in (`mocha`, `macchiato`, `frappe`, `latte`, `material`). Custom themes go in `~/.config/ox/config.toml` under `[tui.theme] base`.

## Persistence

Slash-command choices stay session-local. Restart returns to your declared configuration in `ox.toml` — no slash command writes user config files.
