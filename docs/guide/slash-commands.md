# Slash Commands

Type `/` to open the autocomplete popup. **Up / Down** browse, **Tab** completes, **Enter** submits, **Esc** dismisses. Aliases (`/clear (new, reset)`) all route to the same command.

For commands with curated arguments (`/model`, `/effort`, `/theme`), the popup switches to **arg mode** after `/<name>` + space, listing valid values.

## Built-in Commands

| Command                                     | Description                                                                          |
| ------------------------------------------- | ------------------------------------------------------------------------------------ |
| `/clear` (aliases `/new`, `/reset`)         | Start a fresh session. The previous one stays resumable via `ox -c`.                 |
| `/compact [<instructions>]`                 | Compress the conversation into a summary. Trailing text steers the focus.            |
| `/config`                                   | Open the resolved configuration and its layered file paths in a read-only modal.     |
| `/delete <id-prefix>`                       | Delete a saved session by id prefix, with a Y/N confirm modal before the unlink.     |
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

State-mutating commands (`/clear`, `/compact`, `/delete`, `/init`, and the typed-arg forms of `/effort`, `/model`, `/rename`, `/resume`, `/theme`) wait for the current turn to finish. Read-only commands and the bare modal-opening forms run anytime.

## Model and Effort

Bare `/model` and `/effort` open pickers. Both apply on Enter and cancel on Esc.

`/model <id>` accepts aliases (`opus`, `sonnet`, `haiku`), full ids, unique suffixes, unique substrings, and a `[1m]` suffix for the 1M-context variants. Haiku has no effort tier, so `/effort` on Haiku errors with a recovery hint. See [Configuration](configuration.md) for tier defaults.

## Compaction

`/compact` streams a one-shot summarization request through the live model, then replaces the in-memory transcript with a single boundary block: a header (`Compacted N messages → 1 summary`) plus the rendered summary. The next prompt continues from the summary rather than the full prior chat.

`/compact <instructions>` appends free-text focus instructions to the rubric (e.g., `/compact focus on the build error and how we fixed it`). Useful when only a slice of the work matters going forward.

The summary lands in the JSONL as a `compact` boundary entry plus a synthetic continuation message. Resuming via `ox -c` shows only the post-compact tail. The pre-compact transcript stays on disk for archival but is not replayed in chat. The file-change tracker resets on compact, so any `Edit` after `/compact` requires a fresh `Read`. Queued prompts survive the compaction.

`/compact` refuses on sessions with fewer than 4 messages, when the model returns an empty summary, or while a turn is in flight (it waits for the current reply to finish first).

## Sessions

`/rename` opens a modal pre-filled with the current title. `/rename <title>` sets it directly. The chosen title sticks and replaces the auto-generated AI title for the rest of the session.

`/resume` opens a searchable session picker. Type to filter by id, title, or project, press Tab to widen the scope from current-project to all projects, and press Enter to resume the highlighted session. `/resume <id-prefix>` jumps directly, and ambiguous prefixes list candidates. Switching sessions preserves the running TUI: chat repopulates and the next prompt continues that thread.

Inside the picker, **Ctrl+D** (or **Delete**) on the cursor row opens a Y/N confirm modal. **Y / Enter** unlinks the JSONL and prints a `Deleted session {id}: {title}` line in chat. **N / Esc / Ctrl+C** dismisses. `/delete <id-prefix>` opens the same confirm directly. Only finalized sessions can be deleted.

## Theme

`/theme` opens the picker. **Up / Down** preview each theme live, number keys jump to a row, **Enter** applies for the rest of the session, and **Esc** reverts.

`/theme <name>` swaps directly to a built-in (`mocha`, `macchiato`, `frappe`, `latte`, `material`). Custom themes go in `~/.config/ox/config.toml` under `[tui.theme] base`.

## Persistence

Slash-command choices stay session-local. Restart returns to your declared configuration in `ox.toml`. No slash command writes user config files.
